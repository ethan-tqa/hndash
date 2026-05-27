use rusqlite::{params, Connection, Result};

use crate::models::{Post, PostSummary, Summary};

/// Open or create the SQLite database at `path` and ensure all tables and indexes exist.
pub fn init(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;

    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _migrations (id INTEGER PRIMARY KEY);",
    )?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS posts (
            id            INTEGER PRIMARY KEY,
            hn_id         INTEGER UNIQUE NOT NULL,
            title         TEXT NOT NULL,
            url           TEXT,
            author        TEXT NOT NULL,
            points        INTEGER NOT NULL DEFAULT 0,
            num_comments  INTEGER NOT NULL DEFAULT 0,
            created_at    TEXT NOT NULL,
            fetched_at    TEXT NOT NULL DEFAULT (datetime('now')),
            fetch_status  TEXT NOT NULL DEFAULT 'pending'
                            CHECK(fetch_status IN ('pending','done','error')),
            read_at       TEXT,
            retry_count   INTEGER NOT NULL DEFAULT 0,
            error_message TEXT
        );

        CREATE TABLE IF NOT EXISTS summaries (
            id            INTEGER PRIMARY KEY,
            post_id       INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
            summary_type  TEXT NOT NULL CHECK(summary_type IN ('post','comments','article')),
            content       TEXT NOT NULL,
            model         TEXT NOT NULL,
            created_at    TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE(post_id, summary_type)
        );

        CREATE INDEX IF NOT EXISTS idx_posts_hn_id ON posts(hn_id);
        CREATE INDEX IF NOT EXISTS idx_summaries_post ON summaries(post_id);",
    )?;

    let _ = conn.execute("ALTER TABLE posts ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0", []);
    let _ = conn.execute("ALTER TABLE posts ADD COLUMN error_message TEXT", []);

    Ok(conn)
}

/// Insert a new post or update an existing one (matched by `hn_id`). Returns the internal row id.
pub fn upsert_post(
    conn: &Connection,
    hn_id: i64,
    title: &str,
    url: Option<&str>,
    author: &str,
    points: i64,
    num_comments: i64,
    created_at: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO posts (hn_id, title, url, author, points, num_comments, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(hn_id) DO UPDATE SET
            title       = excluded.title,
            url         = excluded.url,
            author      = excluded.author,
            points      = excluded.points,
            num_comments = excluded.num_comments,
            created_at  = excluded.created_at",
        params![hn_id, title, url, author, points, num_comments, created_at],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Set the `fetch_status` column for a post (`pending` | `done` | `error`).
/// If `error_message` is provided, it is saved (cleared when status is `done`).
pub fn update_fetch_status(conn: &Connection, hn_id: i64, status: &str, error_message: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE posts SET fetch_status = ?1, error_message = ?2 WHERE hn_id = ?3",
        params![status, error_message, hn_id],
    )?;
    Ok(())
}

/// Increment the retry counter for a post.
pub fn increment_retry_count(conn: &Connection, hn_id: i64) -> Result<i64> {
    conn.execute(
        "UPDATE posts SET retry_count = retry_count + 1 WHERE hn_id = ?1",
        params![hn_id],
    )?;
    let new_count: i64 = conn.query_row(
        "SELECT retry_count FROM posts WHERE hn_id = ?1",
        params![hn_id],
        |row| row.get(0),
    )?;
    Ok(new_count)
}

/// Return all posts with `fetch_status = 'error'` and retry_count below the limit.
pub fn get_errored_posts(conn: &Connection, max_retries: i64) -> Result<Vec<(i64, String, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT hn_id, title, url FROM posts WHERE fetch_status = 'error' AND retry_count < ?1",
    )?;
    let rows = stmt.query_map(params![max_retries], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut posts = Vec::new();
    for row in rows {
        posts.push(row?);
    }
    Ok(posts)
}

/// Look up a post by its HN story id. Returns `(internal_id, fetch_status)` if found.
pub fn get_post_by_hn_id(conn: &Connection, hn_id: i64) -> Result<Option<(i64, String)>> {
    let mut stmt = conn.prepare("SELECT id, fetch_status FROM posts WHERE hn_id = ?1")?;
    let mut rows = stmt.query(params![hn_id])?;
    match rows.next()? {
        Some(row) => Ok(Some((row.get(0)?, row.get(1)?))),
        None => Ok(None),
    }
}

/// Insert or replace a summary for a post. `summary_type` is one of `post`, `comments`, `article`.
pub fn insert_summary(
    conn: &Connection,
    post_id: i64,
    summary_type: &str,
    content: &str,
    model: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO summaries (post_id, summary_type, content, model)
         VALUES (?1, ?2, ?3, ?4)",
        params![post_id, summary_type, content, model],
    )?;
    Ok(())
}

/// Count total posts in the database.
pub fn count_posts(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM posts", [], |row| row.get(0))
}

/// Fetch a page of posts with their associated summaries, ordered by creation time descending.
pub fn query_posts_with_summaries_paginated(
    conn: &Connection,
    page: i64,
    per_page: i64,
) -> Result<Vec<PostSummary>> {
    let offset = (page - 1).max(0) * per_page;
    let mut stmt = conn.prepare(
        "SELECT p.id, p.hn_id, p.title, p.url, p.author, p.points, p.num_comments,
                p.created_at, p.fetched_at, p.fetch_status, p.read_at,
                p.retry_count, p.error_message,
                s.id, s.post_id, s.summary_type, s.content, s.model,
                s.created_at
         FROM (
             SELECT id FROM posts
             ORDER BY created_at DESC
             LIMIT ?1 OFFSET ?2
         ) AS page
         JOIN posts p ON p.id = page.id
         LEFT JOIN summaries s ON s.post_id = p.id
         ORDER BY p.created_at DESC",
    )?;

    let rows = stmt.query_map(params![per_page, offset], |row| {
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

    let mut posts: Vec<PostSummary> = Vec::new();
    for row in rows {
        let (post, summary) = row?;
        match posts.last_mut() {
            Some(ps) if ps.post.id == post.id => {
                if let Some(s) = summary {
                    ps.summaries.push(s);
                }
            }
            _ => {
                posts.push(PostSummary {
                    post,
                    summaries: summary.into_iter().collect(),
                });
            }
        }
    }
    Ok(posts)
}

/// Set `read_at` on a post (marks the entire post as read).
pub fn mark_post_read(conn: &Connection, hn_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE posts SET read_at = datetime('now') WHERE hn_id = ?1 AND read_at IS NULL",
        params![hn_id],
    )?;
    Ok(())
}

/// Set `read_at` on every post where it is currently null.
pub fn mark_all_read(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE posts SET read_at = datetime('now') WHERE read_at IS NULL",
        [],
    )?;
    Ok(())
}

/// Remove all summaries belonging to a post (used before re-summarizing).
pub fn delete_summaries_for_post(conn: &Connection, hn_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM summaries WHERE post_id = (SELECT id FROM posts WHERE hn_id = ?1)",
        params![hn_id],
    )?;
    Ok(())
}

/// Return all posts with `fetch_status = 'pending'` (left over from a crash).
pub fn get_pending_posts(conn: &Connection) -> Result<Vec<(i64, String, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT hn_id, title, url FROM posts WHERE fetch_status = 'pending'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut posts = Vec::new();
    for row in rows {
        posts.push(row?);
    }
    Ok(posts)
}

/// Remove a single post (and all its summaries via CASCADE).
pub fn delete_post(conn: &Connection, hn_id: i64) -> Result<()> {
    conn.execute("DELETE FROM posts WHERE hn_id = ?1", params![hn_id])?;
    Ok(())
}

/// Remove every post (and all summaries via CASCADE).
pub fn delete_all_posts(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM posts", [])?;
    Ok(())
}
