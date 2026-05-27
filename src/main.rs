mod article;
mod config;
mod db;
mod fetcher;
mod models;
mod search;
mod summarizer;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_RETRIES: i64 = 3;

use std::collections::HashMap;

use axum::extract::{Form, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::Router;
use rusqlite::Connection;
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, Layer, EnvFilter};

use crate::fetcher::{search_stories, top_comments};
use crate::models::Config;

type AppStateRef = Arc<AppState>;

struct AppState {
    db: Arc<Mutex<Connection>>,
    search_index: Arc<search::SearchIndex>,
    fetch_in_progress: AtomicBool,
    http_client: reqwest::Client,
    config: Config,
    templates: minijinja::Environment<'static>,
    last_fetch_time: Mutex<Option<String>>,
}

async fn with_db<R>(db: &Arc<Mutex<Connection>>, f: impl FnOnce(&Connection) -> R + Send + 'static) -> R
where
    R: Send + 'static,
{
    let db = Arc::clone(db);
    tokio::task::spawn_blocking(move || {
        let conn = db.lock().expect("db lock");
        f(&conn)
    })
    .await
    .expect("blocking task panicked")
}

#[tokio::main]
async fn main() {
    let file_appender = tracing_appender::rolling::daily("logs", "hndash.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()));

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_filter(EnvFilter::new("warn"));

    tracing_subscriber::registry()
        .with(stdout_layer)
        .with(file_layer)
        .init();

    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = match info.payload().downcast_ref::<&str>() {
            Some(s) => s.to_string(),
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => s.clone(),
                None => "unknown".to_string(),
            },
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "?".to_string());
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(
            target: "panic",
            "PANIC at {location}\n{msg}\n\nBacktrace:\n{backtrace}",
        );
        prev(info);
    }));

    let cfg = config::load_config();

    config::validate_api_key(&cfg).await;

    let conn = db::init(&cfg.db.path).expect("Failed to initialize database");
    info!("Database initialized at {}", cfg.db.path);

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("Failed to create HTTP client");

    let index_content =
        std::fs::read_to_string("templates/index.html").expect("Failed to read template");
    let import_content =
        std::fs::read_to_string("templates/import.html").expect("Failed to read import template");
    let mut templates = minijinja::Environment::new();
    templates
        .add_template_owned("index.html", index_content)
        .expect("Failed to add template");
    templates.add_filter("urlencode", |s: &str| urlencoding::encode(s).to_string());
    templates
        .add_template_owned("import.html", import_content)
        .expect("Failed to add import template");

    let search_index_path = format!("{}_search_index", cfg.db.path.trim_end_matches(".db"));
    let search_index = Arc::new(search::SearchIndex::new(&search_index_path)
        .expect("Failed to initialize search index"));

    let state = Arc::new(AppState {
        db: Arc::new(Mutex::new(conn)),
        search_index,
        fetch_in_progress: AtomicBool::new(false),
        http_client,
        config: cfg,
        templates,
        last_fetch_time: Mutex::new(None),
    });

    let app = Router::new()
        .route("/", get(dashboard))
        .route("/refresh", post(trigger_refresh))
        .route("/resummarize/{hn_id}", post(resummarize))
        .route("/mark-read-post/{hn_id}", post(mark_read_post))
        .route("/mark-read-all", post(mark_all_read))
        .route("/remove-all-posts", post(remove_all_posts))
        .route("/remove-post/{hn_id}", post(remove_post))
        .route("/search", get(search))
        .route("/import", get(import_page).post(import_submit))
        .route("/retry-imports", post(retry_imports))
        .with_state(state.clone());

    let addr = format!("{}:{}", state.config.server.host, state.config.server.port);

    // Recover any posts left in "pending" from a previous crash
    let state_clone = state.clone();
    tokio::spawn(async move {
        recover_pending_posts(&state_clone).await;
    });

    // Recover any import queue items stuck in "processing" from a previous crash
    let pending_imports = with_db(&state.db, |conn| db::reset_stuck_imports(conn).unwrap_or(0)).await;
    if pending_imports > 0 {
        info!(pending_imports, "resuming import queue");
        let state_clone = state.clone();
        tokio::spawn(async move {
            process_import_queue(&state_clone).await;
        });
    }

    // Rebuild Tantivy search index before accepting requests
    let index_for_rebuild = state.search_index.clone();
    let db_for_rebuild = state.db.clone();
    tokio::task::spawn_blocking(move || {
        let conn = db_for_rebuild.lock().expect("db lock");
        index_for_rebuild.rebuild(&conn).unwrap_or_else(|e| {
            warn!(error = %e, "Tantivy rebuild failed (non-fatal)");
        });
    }).await.expect("blocking task panicked");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind");
    info!("Listening on {}", addr);

    // Start fetch cycle after server is bound (design decision #6)
    let state_clone = state.clone();
    tokio::spawn(async move {
        run_fetch_cycle(&state_clone).await;
    });

    let interval_secs = state.config.hn.fetch_interval_minutes * 60;
    if interval_secs > 0 {
        let state_clone = state.clone();
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_secs(interval_secs));
            timer.tick().await;
            loop {
                timer.tick().await;
                run_fetch_cycle(&state_clone).await;
            }
        });
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("Server error");
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for ctrl+c");
    info!("Shutting down gracefully...");
}

// ── Route handlers ──────────────────────────────────────────

async fn dashboard(
    state: State<AppStateRef>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let page: i64 = params
        .get("page")
        .and_then(|p| p.parse().ok())
        .unwrap_or(1)
        .max(1);
    let per_page: i64 = 20;

    let (posts, total_posts) = with_db(&state.db, move |conn| {
        let total = db::count_posts(conn).unwrap_or(0);
        let posts =
            db::query_posts_with_summaries_paginated(conn, page, per_page).unwrap_or_default();
        (posts, total)
    }).await;

    let total_pages = (total_posts + per_page - 1) / per_page;
    let page_range: Vec<i64> = (1..=total_pages.max(1)).collect();

    let last_fetch_time = state.last_fetch_time.lock().expect("last_fetch lock").clone();

    let ctx = serde_json::json!({
        "posts": posts,
        "page": page,
        "total_pages": total_pages,
        "total_posts": total_posts,
        "page_range": page_range,
        "last_fetch_time": last_fetch_time,
        "query": "",
    });

    match state.templates.get_template("index.html") {
        Ok(tmpl) => match tmpl.render(ctx) {
            Ok(html) => Html(html).into_response(),
            Err(e) => {
                error!(error = %e, "template render failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
            }
        },
        Err(e) => {
            error!(error = %e, "template not found");
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

async fn trigger_refresh(state: State<AppStateRef>) -> impl IntoResponse {
    if state.fetch_in_progress.load(Ordering::Acquire) {
        return (StatusCode::CONFLICT, "Fetch already in progress").into_response();
    }

    let inner = state.0.clone();
    tokio::spawn(async move {
        run_fetch_cycle(&inner).await;
    });

    (StatusCode::ACCEPTED, "Refresh triggered").into_response()
}

async fn resummarize(
    state: State<AppStateRef>,
    Path(hn_id): Path<i64>,
) -> impl IntoResponse {
    // Check post exists
    let exists = with_db(&state.db, move |conn| {
        db::get_post_by_hn_id(conn, hn_id)
            .ok()
            .flatten()
            .is_some()
    }).await;

    if !exists {
        return (StatusCode::NOT_FOUND, "Post not found").into_response();
    }

    // Delete existing summaries
    let summaries_ok = with_db(&state.db, move |conn| {
        if let Err(e) = db::delete_summaries_for_post(conn, hn_id) {
            error!(%hn_id, error = %e, "failed to delete summaries");
            return false;
        }
        if let Err(e) = db::update_fetch_status(conn, hn_id, "pending", None) {
            error!(%hn_id, error = %e, "failed to set pending status");
        }
        true
    }).await;

    if !summaries_ok {
        return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to delete summaries")
            .into_response();
    }

    // Reprocess in background
    let inner = state.0.clone();
    tokio::spawn(async move {
        reprocess_post(&inner, hn_id).await;
    });

    (StatusCode::ACCEPTED, "Re-summarize queued").into_response()
}

async fn mark_read_post(
    state: State<AppStateRef>,
    Path(hn_id): Path<i64>,
) -> impl IntoResponse {
    match with_db(&state.db, move |conn| db::mark_post_read(conn, hn_id)).await {
        Ok(_) => (StatusCode::OK, "Marked as read").into_response(),
        Err(e) => {
            error!(%hn_id, error = %e, "failed to mark post read");
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to mark post read").into_response()
        }
    }
}

async fn mark_all_read(state: State<AppStateRef>) -> impl IntoResponse {
    match with_db(&state.db, |conn| db::mark_all_read(conn)).await {
        Ok(_) => (StatusCode::OK, "All marked as read").into_response(),
        Err(e) => {
            error!(error = %e, "failed to mark all read");
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to mark all read").into_response()
        }
    }
}

async fn remove_all_posts(state: State<AppStateRef>) -> impl IntoResponse {
    let search_idx = state.search_index.clone();
    match with_db(&state.db, move |conn| {
        search_idx.clear();
        db::delete_all_posts(conn)
    }).await {
        Ok(_) => (StatusCode::OK, "All posts removed").into_response(),
        Err(e) => {
            error!(error = %e, "failed to remove all posts");
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to remove posts").into_response()
        }
    }
}

async fn remove_post(
    state: State<AppStateRef>,
    Path(hn_id): Path<i64>,
) -> impl IntoResponse {
    let search_idx = state.search_index.clone();
    match with_db(&state.db, move |conn| {
        let post_id: Option<i64> = conn
            .query_row("SELECT id FROM posts WHERE hn_id = ?1", rusqlite::params![hn_id], |row| row.get(0))
            .ok();
        if let Some(pid) = post_id {
            search_idx.remove(pid);
        }
        db::delete_post(conn, hn_id)
    }).await {
        Ok(_) => (StatusCode::OK, "Post removed").into_response(),
        Err(e) => {
            error!(%hn_id, error = %e, "failed to remove post");
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to remove post").into_response()
        }
    }
}

// ── Import page ─────────────────────────────────────────────

fn extract_hn_ids(text: &str) -> Vec<i64> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            if let Ok(id) = line.parse::<i64>() {
                return Some(id);
            }
            let pos = line.find("id=")?;
            let after = &pos + 3..;
            let id_str: String = line[after]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            id_str.parse::<i64>().ok()
        })
        .collect()
}

async fn import_page(state: State<AppStateRef>) -> impl IntoResponse {
    let queue = with_db(&state.db, |conn| db::get_all_imports(conn).unwrap_or_default()).await;
    let has_errors = queue.iter().any(|item| item.status == "error");

    let ctx = serde_json::json!({
        "queue": queue,
        "pasted_text": "",
        "imported_count": 0,
        "has_errors": has_errors,
    });

    match state.templates.get_template("import.html") {
        Ok(tmpl) => match tmpl.render(ctx) {
            Ok(html) => Html(html).into_response(),
            Err(e) => {
                error!(error = %e, "import template render failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
            }
        },
        Err(e) => {
            error!(error = %e, "import template not found");
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

async fn import_submit(
    state: State<AppStateRef>,
    Form(form): Form<HashMap<String, String>>,
) -> impl IntoResponse {
    let pasted = form.get("urls").map(|s| s.as_str()).unwrap_or("");
    let ids = extract_hn_ids(pasted);
    let (imported_count, pending) = with_db(&state.db, move |conn| {
        let mut count = 0i64;
        for id in &ids {
            let url = format!("https://news.ycombinator.com/item?id={}", id);
            if db::insert_import_item(conn, *id, &url).is_ok() {
                count += 1;
            }
        }
        let pending = db::count_pending_imports(conn).unwrap_or(0);
        (count, pending)
    }).await;

    if pending > 0 {
        let state_clone = state.0.clone();
        tokio::spawn(async move {
            process_import_queue(&state_clone).await;
        });
    }

    let queue = with_db(&state.db, |conn| db::get_all_imports(conn).unwrap_or_default()).await;
    let has_errors = queue.iter().any(|item| item.status == "error");

    let ctx = serde_json::json!({
        "queue": queue,
        "pasted_text": pasted,
        "imported_count": imported_count,
        "has_errors": has_errors,
    });

    match state.templates.get_template("import.html") {
        Ok(tmpl) => match tmpl.render(ctx) {
            Ok(html) => Html(html).into_response(),
            Err(e) => {
                error!(error = %e, "import template render failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
            }
        },
        Err(e) => {
            error!(error = %e, "import template not found");
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

async fn retry_imports(state: State<AppStateRef>) -> impl IntoResponse {
    let pending = with_db(&state.db, |conn| db::reset_errored_imports(conn).unwrap_or(0)).await;
    if pending > 0 {
        let state_clone = state.0.clone();
        tokio::spawn(async move {
            process_import_queue(&state_clone).await;
        });
    }
    Redirect::to("/import")
}

// ── Search ──────────────────────────────────────────────────

async fn search(
    state: State<AppStateRef>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let query = params.get("q").map(|s| s.as_str()).unwrap_or("").trim().to_string();
    if query.is_empty() {
        return Redirect::to("/").into_response();
    }

    let page: i64 = params
        .get("page")
        .and_then(|p| p.parse().ok())
        .unwrap_or(1)
        .max(1);
    let per_page: i64 = 20;

    let db = state.db.clone();
    let search_idx = state.search_index.clone();
    let query_in_closure = query.clone();
    let raw_result = tokio::task::spawn_blocking(move || {
        let conn = db.lock().expect("db lock");
        search_idx.search(&query_in_closure, page as usize, per_page as usize, &conn)
    }).await.expect("blocking task panicked");

    let (posts, total_posts): (Vec<_>, usize) = match raw_result {
        Ok(result) => result,
        Err(e) => {
            error!(error = %e, query = %query, "search failed");
            (Vec::new(), 0)
        }
    };
    let total_posts_i64 = total_posts as i64;

    let total_pages = (total_posts_i64 + per_page - 1) / per_page;
    let page_range: Vec<i64> = (1..=total_pages.max(1)).collect();

    let ctx = serde_json::json!({
        "posts": posts,
        "page": page,
        "total_pages": total_pages,
        "total_posts": total_posts,
        "page_range": page_range,
        "query": query,
    });

    match state.templates.get_template("index.html") {
        Ok(tmpl) => match tmpl.render(ctx) {
            Ok(html) => Html(html).into_response(),
            Err(e) => {
                error!(error = %e, "template render failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
            }
        },
        Err(e) => {
            error!(error = %e, "template not found");
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

async fn process_import_queue(state: &AppState) {
    loop {
        let item = with_db(&state.db, |conn| db::claim_next_import(conn).unwrap_or(None)).await;

        let (hn_id, _url) = match item {
            Some((id, url)) => (id, url),
            None => break,
        };

        // Skip if the post was already created (e.g. by the fetch cycle)
        let already_exists = with_db(&state.db, move |conn| {
            matches!(db::get_post_by_hn_id(conn, hn_id), Ok(Some(_)))
        }).await;
        if already_exists {
            info!(%hn_id, "skipping import, post already exists");
            with_db(&state.db, move |conn| {
                db::update_import_status(conn, hn_id, "done", None).ok();
            }).await;
            continue;
        }

        info!(%hn_id, "importing post");

        let result = {
            let item = fetcher::fetch_item(&state.http_client, hn_id as u64).await;
            match item {
                Some(item) => {
                    let title = item.title.as_deref().unwrap_or("Untitled");
                    let author = item.author.as_deref().unwrap_or("unknown");
                    let points = item.points.unwrap_or(0) as i64;
                    fn count_comments(children: &[fetcher::Comment]) -> i64 {
                        let mut count = 0i64;
                        for child in children {
                            count += 1 + count_comments(&child.children);
                        }
                        count
                    }
                    let num_comments = count_comments(&item.children);
                    let created_at = item
                        .created_at
                        .as_deref()
                        .unwrap_or("")
                        .to_string();

                    let article_url = item.url.as_deref();

                    let title_owned = title.to_string();
                    let author_owned = author.to_string();
                    let article_url_owned = article_url.map(|s| s.to_string());

                    let search_idx = state.search_index.clone();
                    let _post_id = match with_db(&state.db, move |conn| {
                        let id = db::upsert_post(
                            conn, hn_id, &title_owned, article_url_owned.as_deref(),
                            &author_owned, points, num_comments, &created_at,
                        )?;
                        search_idx.upsert(conn, id);
                        let _ = db::update_fetch_status(conn, hn_id, "pending", None);
                        Ok::<_, rusqlite::Error>(id)
                    }).await {
                        Ok(id) => id,
                        Err(e) => {
                            error!(%hn_id, error = %e, "failed to insert imported post");
                            return;
                        }
                    };

                    let (ok, _summary_error) = fetch_and_summarize(
                        state, hn_id, title, article_url,
                        &state.config.article, &state.config.llm,
                    )
                    .await;

                    let error_msg: Option<&'static str> = if ok { None } else { Some("summarization failed") };
                    let status_str: &'static str = if ok { "done" } else { "error" };
                    let msg = error_msg.map(|s| s.to_string());
                    with_db(&state.db, move |conn| {
                        db::update_fetch_status(conn, hn_id, status_str, msg.as_deref()).ok();
                    }).await;

                    (status_str.to_string(), None::<String>)
                }
                None => {
                    error!(%hn_id, "failed to fetch item from HN API");
                    ("error".to_string(), Some("failed to fetch item from HN API".to_string()))
                }
            }
        };

        let result_status = result.0.clone();
        let result_msg = result.1.clone();
        with_db(&state.db, move |conn| {
            db::update_import_status(conn, hn_id, &result_status, result_msg.as_deref()).ok();
        }).await;

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ── Fetch cycle ─────────────────────────────────────────────

async fn run_fetch_cycle(state: &AppState) {
    if state.fetch_in_progress.swap(true, Ordering::AcqRel) {
        warn!("Fetch cycle already in progress, skipping");
        return;
    }

    info!("Fetch cycle starting");
    let config = &state.config;
    let hn = &config.hn;

    let mut new_posts = 0;

    for page in 0..hn.max_fetch_pages {
        let resp = match search_stories(&state.http_client, hn, page).await {
            Some(r) => r,
            None => {
                warn!(page, "search_stories returned None, stopping pagination");
                break;
            }
        };

        let total_on_page = resp.hits.len();
        info!(page, total_on_page, "search page fetched");

        for hit in &resp.hits {
            let hn_id: i64 = match hit.object_id.parse() {
                Ok(id) => id,
                Err(_) => continue,
            };

            if with_db(&state.db, move |conn| {
                matches!(db::get_post_by_hn_id(conn, hn_id), Ok(Some(_)))
            }).await {
                continue;
            }

            process_post(state, hit, hn_id).await;
            new_posts += 1;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        if page >= resp.nb_pages.saturating_sub(1) {
            info!("Reached last page, stopping pagination");
            break;
        }

        if page + 1 < hn.max_fetch_pages {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let now_str = ts_to_iso(now_secs as i64);
    *state.last_fetch_time.lock().expect("last_fetch lock") = Some(now_str);

    let retried = retry_errored_posts(state).await;
    if retried > 0 {
        info!(retried, "retried errored posts");
    }

    info!(new_posts, "Fetch cycle complete");
    state.fetch_in_progress.store(false, Ordering::Release);
}

async fn process_post(state: &AppState, hit: &fetcher::SearchHit, hn_id: i64) {
    let config = &state.config;
    let title = hit.title.as_deref().unwrap_or("Untitled");
    let url = hit.url.as_deref();
    let author = hit.author.as_deref().unwrap_or("unknown");
    let points = hit.points.unwrap_or(0) as i64;
    let num_comments = hit.num_comments.unwrap_or(0) as i64;
    let created_at = hit
        .created_at
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ts_to_iso(hit.created_at_i));

    let title_owned = title.to_string();
    let author_owned = author.to_string();
    let url_owned = url.map(|s| s.to_string());

    let search_idx = state.search_index.clone();
    let _post_id = match with_db(&state.db, move |conn| {
        let id = db::upsert_post(conn, hn_id, &title_owned, url_owned.as_deref(), &author_owned, points, num_comments, &created_at)?;
        search_idx.upsert(conn, id);
        if let Err(e) = db::update_fetch_status(conn, hn_id, "pending", None) {
            error!(%hn_id, error = %e, "failed to set pending status");
        }
        Ok::<_, rusqlite::Error>(id)
    }).await {
        Ok(id) => id,
        Err(e) => {
            error!(%hn_id, error = %e, "failed to insert post");
            return;
        }
    };

    let (ok, error_msg) = fetch_and_summarize(state, hn_id, title, url, &config.article, &config.llm).await;

    let status: &'static str = if ok { "done" } else { "error" };
    let error_msg_owned: Option<String> = if ok { None } else { Some(error_msg) };
    with_db(&state.db, move |conn| {
        if let Err(e) = db::update_fetch_status(conn, hn_id, status, error_msg_owned.as_deref()) {
            error!(%hn_id, error = %e, "failed to set {} status", status);
        }
        if !ok {
            let _ = db::increment_retry_count(conn, hn_id);
        }
    }).await;

    info!(%hn_id, title, ok, "post processed");
}

async fn reprocess_post(state: &AppState, hn_id: i64) {
    let config = &state.config;

    let (_post_id, title, url) = match with_db(&state.db, move |conn| {
        conn.prepare("SELECT id, title, url FROM posts WHERE hn_id = ?1")
            .and_then(|mut stmt| stmt.query_row(rusqlite::params![hn_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?))
            }))
    }).await {
        Ok((id, title, url)) => (id, title, url),
        Err(e) => {
            error!(%hn_id, error = %e, "post not found for reprocess");
            return;
        }
    };

    let (ok, error_msg) = fetch_and_summarize(state, hn_id, &title, url.as_deref(), &config.article, &config.llm).await;

    let status: &'static str = if ok { "done" } else { "error" };
    let error_msg_owned: Option<String> = if ok { None } else { Some(error_msg) };
    with_db(&state.db, move |conn| {
        if let Err(e) = db::update_fetch_status(conn, hn_id, status, error_msg_owned.as_deref()) {
            error!(%hn_id, error = %e, "failed to set {} status", status);
        }
        if !ok {
            let _ = db::increment_retry_count(conn, hn_id);
        }
    }).await;

    info!(%hn_id, ok, "reprocess complete");
}

/// Re-process any posts left in `pending` status from a previous crash.
async fn recover_pending_posts(state: &AppState) {
    let pending = with_db(&state.db, |conn| {
        db::get_pending_posts(conn).unwrap_or_default()
    }).await;

    if pending.is_empty() {
        return;
    }

    info!(count = pending.len(), "recovering posts left in pending state");

    for (hn_id, title, url) in pending {
        let _post_id = match with_db(&state.db, move |conn| {
            conn.prepare("SELECT id FROM posts WHERE hn_id = ?1")
                .and_then(|mut stmt| stmt.query_row(rusqlite::params![hn_id], |row| row.get::<_, i64>(0)))
        }).await {
            Ok(id) => id,
            Err(e) => {
                error!(%hn_id, error = %e, "post not found for recovery");
                continue;
            }
        };

        info!(%hn_id, "recovering pending post");
        let (ok, error_msg) = fetch_and_summarize(
            state,
            hn_id,
            &title,
            url.as_deref(),
            &state.config.article,
            &state.config.llm,
        )
        .await;

        let status: &'static str = if ok { "done" } else { "error" };
        let error_msg_owned: Option<String> = if ok { None } else { Some(error_msg) };
        with_db(&state.db, move |conn| {
            if let Err(e) = db::update_fetch_status(conn, hn_id, status, error_msg_owned.as_deref()) {
                error!(%hn_id, error = %e, "failed to set recovery status");
            }
            if !ok {
                let _ = db::increment_retry_count(conn, hn_id);
            }
        }).await;

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    info!("pending post recovery complete");
}

/// Retry posts that previously ended in `error` status, up to `MAX_RETRIES` times.
/// Returns the number of posts retried.
async fn retry_errored_posts(state: &AppState) -> usize {
    let errored = with_db(&state.db, |conn| {
        db::get_errored_posts(conn, MAX_RETRIES).unwrap_or_default()
    }).await;

    if errored.is_empty() {
        return 0;
    }

    info!(count = errored.len(), "retrying errored posts");

    for (hn_id_ref, title, url) in &errored {
        let hn_id = *hn_id_ref;

        let (_post_id, current_retry_count) = match with_db(&state.db, move |conn| {
            conn.prepare("SELECT id, retry_count FROM posts WHERE hn_id = ?1")
                .and_then(|mut stmt| stmt.query_row(rusqlite::params![hn_id], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                }))
        }).await {
            Ok((id, rc)) => (id, rc),
            Err(e) => {
                error!(%hn_id, error = %e, "post not found for retry");
                continue;
            }
        };

        info!(%hn_id, title, attempt = current_retry_count + 1, max = MAX_RETRIES, "retrying errored post");

        let (ok, error_msg) = fetch_and_summarize(
            state,
            hn_id,
            title,
            url.as_deref(),
            &state.config.article,
            &state.config.llm,
        )
        .await;

        let status: &'static str = if ok { "done" } else { "error" };
        let error_msg_owned: Option<String> = if ok { None } else { Some(error_msg) };
        with_db(&state.db, move |conn| {
            if let Err(e) = db::update_fetch_status(conn, hn_id, status, error_msg_owned.as_deref()) {
                error!(%hn_id, error = %e, "failed to set retry status");
            }
            if !ok {
                let _ = db::increment_retry_count(conn, hn_id);
            }
        }).await;

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    errored.len()
}

/// Returns `(success, error_message)`. A `false` success means at least one step failed;
/// the error message describes which steps failed.
async fn fetch_and_summarize(
    state: &AppState,
    hn_id: i64,
    title: &str,
    url: Option<&str>,
    article_config: &crate::models::ArticleConfig,
    llm_config: &crate::models::LlmConfig,
) -> (bool, String) {
    let item = fetcher::fetch_item(&state.http_client, hn_id as u64).await;

    let comments_text = item.as_ref().map(|item| {
        let top = top_comments(item, 30);
        top.iter()
            .filter_map(|c| {
                let author_disp = c.author.as_deref().unwrap_or("anonymous");
                let text = c.text.as_deref().unwrap_or("");
                if text.is_empty() {
                    None
                } else {
                    Some(format!("{}: {}", author_disp, text))
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    });

    let article_text = if let Some(u) = url {
        if !u.is_empty() && !u.contains("news.ycombinator.com/item?id=") {
            article::fetch_article(article_config, u).await
        } else {
            None
        }
    } else {
        None
    };

    let story_text = item.as_ref()
        .and_then(|i| i.text.as_deref())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let post_fut = async {
        if let Some(ref story) = story_text {
            info!(%hn_id, "summarizing post story text");
            let result = summarizer::summarize_post(
                &state.http_client, llm_config, title, Some(story), hn_id
            ).await;
            (result, true)
        } else {
            (None, false)
        }
    };

    let comments_fut = async {
        if let Some(ref text) = comments_text {
            if !text.is_empty() && text.len() > 10 {
                info!(%hn_id, "summarizing comments");
                let result = summarizer::summarize_comments(
                    &state.http_client, llm_config, text, hn_id
                ).await;
                (result, true)
            } else {
                (None, false)
            }
        } else {
            (None, false)
        }
    };

    let article_fut = async {
        if let Some(ref text) = article_text {
            if !text.is_empty() {
                info!(%hn_id, "summarizing article");
                let result = summarizer::summarize_article(
                    &state.http_client, llm_config, text, hn_id
                ).await;
                (result, true)
            } else {
                (None, false)
            }
        } else {
            (None, false)
        }
    };

    let ((post_result, post_attempted),
         (comments_result, comments_attempted),
         (article_result, article_attempted)) = tokio::join!(post_fut, comments_fut, article_fut);

    let mut ok = true;
    let mut errors: Vec<&str> = Vec::new();

    let model = llm_config.model.clone();

    let search_idx = state.search_index.clone();

    if let Some(ref summary) = post_result {
        let summary_owned = summary.clone();
        let model = model.clone();
        let search_idx = search_idx.clone();
        with_db(&state.db, move |conn| {
                    if let Ok(Some((current_id, _))) = db::get_post_by_hn_id(conn, hn_id) {
                        if let Err(e) = db::insert_summary(conn, current_id, "post", &summary_owned, &model) {
                            error!(%hn_id, error = %e, "failed to insert post summary");
                        }
                        search_idx.upsert(conn, current_id);
                    }
                }).await;
            } else if post_attempted {
        error!(%hn_id, "post story text summary failed");
        ok = false;
        errors.push("post text summary failed");
    }

    if let Some(ref summary) = comments_result {
        let summary_owned = summary.clone();
        let model = model.clone();
        let search_idx = search_idx.clone();
        with_db(&state.db, move |conn| {
                    if let Ok(Some((current_id, _))) = db::get_post_by_hn_id(conn, hn_id) {
                        if let Err(e) = db::insert_summary(conn, current_id, "comments", &summary_owned, &model) {
                            error!(%hn_id, error = %e, "failed to insert comments summary");
                        }
                        search_idx.upsert(conn, current_id);
                    }
                }).await;
            } else if comments_attempted {
        error!(%hn_id, "comments summary failed");
        ok = false;
        errors.push("comments summary failed");
    }

    if let Some(ref summary) = article_result {
        let summary_owned = summary.clone();
        let search_idx = search_idx.clone();
        with_db(&state.db, move |conn| {
                    if let Ok(Some((current_id, _))) = db::get_post_by_hn_id(conn, hn_id) {
                        if let Err(e) = db::insert_summary(conn, current_id, "article", &summary_owned, &model) {
                            error!(%hn_id, error = %e, "failed to insert article summary");
                        }
                        search_idx.upsert(conn, current_id);
                    }
        }).await;
    } else if article_attempted {
        error!(%hn_id, "article summary failed");
        ok = false;
        errors.push("article summary failed");
    }

    let error_msg = errors.join("; ");
    (ok, error_msg)
}

fn ts_to_iso(ts: i64) -> String {
    let mut d = ts as i64;
    let h = d % 86400 / 3600;
    let m = d % 3600 / 60;
    let s = d % 60;
    d /= 86400;

    let mut y = 1970i64;
    loop {
        let days_in = if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
            366
        } else {
            365
        };
        if d < days_in {
            break;
        }
        d -= days_in;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mon_days: &[i64] = if leap {
        &[31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        &[31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut mo = 1i64;
    for &md in mon_days {
        if d < md {
            break;
        }
        d -= md;
        mo += 1;
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        mo,
        d + 1,
        h,
        m,
        s
    )
}


