use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use tantivy::collector::{DocSetCollector, TopDocs};
use tantivy::query::{BooleanQuery, FuzzyTermQuery, Occur, RegexQuery};
use tantivy::schema::*;
use tantivy::{doc, Index, IndexWriter, TantivyDocument};

use crate::models::{Post, PostSummary, Summary};

pub struct SearchIndex {
    index: Index,
    title: Field,
    author: Field,
    summary_content: Field,
    hn_id: Field,
    post_id: Field,
    writer: Mutex<IndexWriter>,
}

impl SearchIndex {
    pub fn new(dir: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let mut schema_builder = Schema::builder();
        let title = schema_builder.add_text_field("title", TEXT | STORED);
        let author = schema_builder.add_text_field("author", TEXT);
        let summary_content = schema_builder.add_text_field("summary_content", TEXT);
        let hn_id = schema_builder.add_u64_field("hn_id", STORED);
        let post_id = schema_builder.add_u64_field("post_id", STORED);
        let schema = schema_builder.build();

        let index = if Path::new(dir).exists() {
            Index::open_in_dir(dir)?
        } else {
            std::fs::create_dir_all(dir)?;
            Index::create_in_dir(dir, schema)?
        };

        let writer = index.writer(50_000_000)?;

        Ok(SearchIndex {
            index,
            title,
            author,
            summary_content,
            hn_id,
            post_id,
            writer: Mutex::new(writer),
        })
    }

    pub fn rebuild(&self, conn: &rusqlite::Connection) -> Result<(), Box<dyn std::error::Error>> {
        let mut writer = self.writer.lock().unwrap();
        writer.delete_all_documents()?;

        let mut stmt = conn.prepare(
            "SELECT p.id, p.hn_id, p.title, p.author,
                    COALESCE((SELECT GROUP_CONCAT(content, ' ') FROM summaries WHERE post_id = p.id), '')
             FROM posts p",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;

        let mut count = 0u64;
        for row in rows {
            let (id, hn_id_val, title, author, summary_content) = match row {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "skipping post during search index rebuild");
                    continue;
                }
            };
            if let Err(e) = writer.add_document(doc!(
                self.title => title,
                self.author => author,
                self.summary_content => summary_content,
                self.hn_id => hn_id_val as u64,
                self.post_id => id as u64,
            )) {
                tracing::warn!(post_id = id, error = %e, "skipping document during search index rebuild");
                continue;
            }
            count += 1;
            if count % 1000 == 0 {
                tracing::info!(count, "search index rebuild progress");
            }
        }

        writer.commit()?;
        tracing::info!(count, "search index rebuild complete");
        Ok(())
    }

    pub fn upsert(&self, conn: &rusqlite::Connection, post_id_internal: i64) {
        let (hn_id_val, title, author, summary_content) = match conn.query_row(
            "SELECT p.hn_id, p.title, p.author,
                    COALESCE((SELECT GROUP_CONCAT(content, ' ') FROM summaries WHERE post_id = p.id), '')
             FROM posts p WHERE p.id = ?1",
            rusqlite::params![post_id_internal],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        ) {
            Ok(row) => row,
            Err(e) => {
                tracing::error!(post_id = post_id_internal, error = %e, "failed to load post data for search index");
                return;
            }
        };

        let mut writer = match self.writer.lock() {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(post_id = post_id_internal, error = %e, "search index mutex poisoned");
                return;
            }
        };

        let term = tantivy::Term::from_field_u64(self.post_id, post_id_internal as u64);
        writer.delete_term(term);
        if let Err(e) = writer.add_document(doc!(
            self.title => title,
            self.author => author,
            self.summary_content => summary_content,
            self.hn_id => hn_id_val as u64,
            self.post_id => post_id_internal as u64,
        )) {
            tracing::error!(post_id = post_id_internal, error = %e, "failed to add document to search index");
            return;
        }
        if let Err(e) = writer.commit() {
            tracing::error!(post_id = post_id_internal, error = %e, "failed to commit search index");
        }
    }

    pub fn remove(&self, post_id_internal: i64) {
        let mut writer = match self.writer.lock() {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(post_id = post_id_internal, error = %e, "search index mutex poisoned");
                return;
            }
        };

        let term = tantivy::Term::from_field_u64(self.post_id, post_id_internal as u64);
        writer.delete_term(term);
        if let Err(e) = writer.commit() {
            tracing::error!(post_id = post_id_internal, error = %e, "failed to commit search index removal");
        }
    }

    pub fn clear(&self) {
        let mut writer = match self.writer.lock() {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(error = %e, "search index mutex poisoned");
                return;
            }
        };

        if let Err(e) = writer.delete_all_documents() {
            tracing::error!(error = %e, "failed to clear search index");
            return;
        }
        if let Err(e) = writer.commit() {
            tracing::error!(error = %e, "failed to commit search index clear");
        }
    }

    pub fn search(
        &self,
        query_str: &str,
        page: usize,
        per_page: usize,
        conn: &rusqlite::Connection,
    ) -> Result<(Vec<PostSummary>, usize), Box<dyn std::error::Error + Send + Sync>> {
        if query_str.trim().is_empty() {
            return Ok((Vec::new(), 0));
        }

        let reader = self.index.reader()?;
        let searcher = reader.searcher();

        let words: Vec<&str> = query_str.split_whitespace().filter(|w| !w.is_empty()).collect();
        let fields = [self.title, self.author, self.summary_content];

        let mut subqueries: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
        for word in &words {
            let mut field_ors: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
            for field in &fields {
                let term = tantivy::Term::from_field_text(*field, word);
                field_ors.push((Occur::Should, Box::new(FuzzyTermQuery::new(term, 1, true))));
                let pattern = format!(r"{}.*", regex::escape(word));
                if let Ok(regex_q) = RegexQuery::from_pattern(&pattern, *field) {
                    field_ors.push((Occur::Should, Box::new(regex_q)));
                }
            }
            subqueries.push((Occur::Must, Box::new(BooleanQuery::new(field_ors))));
        }

        let query: Box<dyn tantivy::query::Query> = if subqueries.is_empty() {
            return Ok((Vec::new(), 0));
        } else if subqueries.len() == 1 {
            subqueries.into_iter().next().unwrap().1
        } else {
            Box::new(BooleanQuery::new(subqueries))
        };

        let total_hits = searcher.search(&*query, &DocSetCollector)?.len();

        let top_docs = searcher.search(&*query, &TopDocs::with_limit(page * per_page))?;

        let offset = (page - 1) * per_page;
        let page_docs = if offset < top_docs.len() {
            &top_docs[offset..top_docs.len().min(offset + per_page)]
        } else {
            &[]
        };

        let hn_ids: Vec<i64> = page_docs
            .iter()
            .filter_map(|(_score, doc_addr)| {
                let doc: TantivyDocument = searcher.doc::<TantivyDocument>(*doc_addr).ok()?;
                doc.get_first(self.hn_id).and_then(|v| v.as_u64()).map(|id| id as i64)
            })
            .collect();

        let posts = if hn_ids.is_empty() {
            Vec::new()
        } else {
            let placeholders: Vec<String> = hn_ids.iter().map(|_| "?".to_string()).collect();
            let sql = format!(
                "SELECT p.id, p.hn_id, p.title, p.url, p.author, p.points, p.num_comments,
                        p.created_at, p.fetched_at, p.fetch_status, p.read_at,
                        p.retry_count, p.error_message,
                        s.id, s.post_id, s.summary_type, s.content, s.model,
                        s.created_at
                 FROM posts p
                 LEFT JOIN summaries s ON s.post_id = p.id
                 WHERE p.hn_id IN ({})
                 ORDER BY p.id",
                placeholders.join(",")
            );
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::types::ToSql> =
                hn_ids.iter().map(|id| id as &dyn rusqlite::types::ToSql).collect();
            let rows = stmt.query_map(params.as_slice(), |row| {
                let post = Post {
                    id: row.get(0)?,
                    hn_id: row.get(1)?,
                    title: row.get(2)?,
                    url: row.get(3)?,
                    author: row.get(4)?,
                    points: row.get(5)?,
                    num_comments: row.get(6)?,
                    created_at: row.get(7)?,
                    fetched_at: row.get(8)?,
                    fetch_status: row.get(9)?,
                    read_at: row.get(10)?,
                    retry_count: row.get(11)?,
                    error_message: row.get(12)?,
                };
                let s_id: Option<i64> = row.get(13)?;
                let summary = if s_id.is_some() {
                    Some(Summary {
                        id: s_id.unwrap(),
                        post_id: row.get(14)?,
                        summary_type: row.get(15)?,
                        content: row.get(16)?,
                        model: row.get(17)?,
                        created_at: row.get(18)?,
                    })
                } else {
                    None
                };
                Ok((post, summary))
            })?;

            let mut post_map: HashMap<i64, PostSummary> = HashMap::new();
            for row in rows {
                let (post, summary) = row?;
                match post_map.get_mut(&post.hn_id) {
                    Some(ps) => {
                        if let Some(s) = summary {
                            ps.summaries.push(s);
                        }
                    }
                    None => {
                        post_map.insert(
                            post.hn_id,
                            PostSummary {
                                post,
                                summaries: summary.into_iter().collect(),
                            },
                        );
                    }
                }
            }

            hn_ids
                .iter()
                .filter_map(|id| post_map.remove(id))
                .collect()
        };

        Ok((posts, total_hits))
    }
}
