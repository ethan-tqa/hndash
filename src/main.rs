mod article;
mod config;
mod db;
mod fetcher;
mod models;
mod summarizer;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_RETRIES: i64 = 3;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::Router;
use rusqlite::Connection;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::fetcher::{search_stories, top_comments};
use crate::models::Config;

type AppStateRef = Arc<AppState>;

struct AppState {
    db: Mutex<Connection>,
    fetch_in_progress: AtomicBool,
    http_client: reqwest::Client,
    config: Config,
    templates: minijinja::Environment<'static>,
    last_fetch_time: Mutex<Option<String>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = config::load_config();

    config::validate_api_key(&cfg).await;

    let conn = db::init(&cfg.db.path).expect("Failed to initialize database");
    info!("Database initialized at {}", cfg.db.path);

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("Failed to create HTTP client");

    let template_content =
        std::fs::read_to_string("templates/index.html").expect("Failed to read template");
    let mut templates = minijinja::Environment::new();
    templates
        .add_template_owned("index.html", template_content)
        .expect("Failed to add template");

    let state = Arc::new(AppState {
        db: Mutex::new(conn),
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
        .with_state(state.clone());

    let addr = format!("{}:{}", state.config.server.host, state.config.server.port);
    info!("Listening on {}", addr);

    // Recover any posts left in "pending" from a previous crash
    recover_pending_posts(&state).await;

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind");

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

async fn dashboard(state: State<AppStateRef>) -> impl IntoResponse {
    let posts = {
        let conn = state.db.lock().expect("db lock");
        db::query_posts_with_summaries(&conn).unwrap_or_default()
    };

    let last_fetch_time = state.last_fetch_time.lock().expect("last_fetch lock").clone();

    let ctx = serde_json::json!({
        "posts": posts,
        "last_fetch_time": last_fetch_time,
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
    let exists = {
        let conn = state.db.lock().expect("db lock");
        db::get_post_by_hn_id(&conn, hn_id)
            .ok()
            .flatten()
            .is_some()
    };

    if !exists {
        return (StatusCode::NOT_FOUND, "Post not found").into_response();
    }

    // Delete existing summaries
    {
        let conn = state.db.lock().expect("db lock");
        if let Err(e) = db::delete_summaries_for_post(&conn, hn_id) {
            error!(%hn_id, error = %e, "failed to delete summaries");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to delete summaries")
                .into_response();
        }
        if let Err(e) = db::update_fetch_status(&conn, hn_id, "pending", None) {
            error!(%hn_id, error = %e, "failed to set pending status");
        }
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
    let conn = state.db.lock().expect("db lock");
    match db::mark_post_read(&conn, hn_id) {
        Ok(_) => (StatusCode::OK, "Marked as read").into_response(),
        Err(e) => {
            error!(%hn_id, error = %e, "failed to mark post read");
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to mark post read").into_response()
        }
    }
}

async fn mark_all_read(state: State<AppStateRef>) -> impl IntoResponse {
    let conn = state.db.lock().expect("db lock");
    match db::mark_all_read(&conn) {
        Ok(_) => (StatusCode::OK, "All marked as read").into_response(),
        Err(e) => {
            error!(error = %e, "failed to mark all read");
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to mark all read").into_response()
        }
    }
}

async fn remove_all_posts(state: State<AppStateRef>) -> impl IntoResponse {
    let conn = state.db.lock().expect("db lock");
    match db::delete_all_posts(&conn) {
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
    let conn = state.db.lock().expect("db lock");
    match db::delete_post(&conn, hn_id) {
        Ok(_) => (StatusCode::OK, "Post removed").into_response(),
        Err(e) => {
            error!(%hn_id, error = %e, "failed to remove post");
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to remove post").into_response()
        }
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

            {
                let conn = state.db.lock().expect("db lock");
                if let Ok(Some(_)) = db::get_post_by_hn_id(&conn, hn_id) {
                    continue;
                }
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

    let post_id = {
        let conn = state.db.lock().expect("db lock");
        match db::upsert_post(&conn, hn_id, title, url, author, points, num_comments, &created_at)
        {
            Ok(id) => {
                if let Err(e) = db::update_fetch_status(&conn, hn_id, "pending", None) {
                    error!(%hn_id, error = %e, "failed to set pending status");
                }
                id
            }
            Err(e) => {
                error!(%hn_id, error = %e, "failed to insert post");
                return;
            }
        }
    };

    let (ok, error_msg) = fetch_and_summarize(state, post_id, hn_id, title, url, &config.article, &config.llm).await;

    {
        let conn = state.db.lock().expect("db lock");
        let status = if ok { "done" } else { "error" };
        let error_msg = if ok { None } else { Some(error_msg.as_str()) };
        if let Err(e) = db::update_fetch_status(&conn, hn_id, status, error_msg) {
            error!(%hn_id, error = %e, "failed to set {} status", status);
        }
        if !ok {
            let _ = db::increment_retry_count(&conn, hn_id);
        }
    }

    info!(%hn_id, title, ok, "post processed");
}

async fn reprocess_post(state: &AppState, hn_id: i64) {
    let config = &state.config;

    let (post_id, title, url) = {
        let conn = state.db.lock().expect("db lock");
        let mut stmt = match conn.prepare("SELECT id, title, url FROM posts WHERE hn_id = ?1") {
            Ok(s) => s,
            Err(e) => {
                error!(%hn_id, error = %e, "failed to query post");
                return;
            }
        };
        match stmt.query_row(rusqlite::params![hn_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?))
        }) {
            Ok((id, title, url)) => (id, title, url),
            Err(e) => {
                error!(%hn_id, error = %e, "post not found for reprocess");
                return;
            }
        }
    };

    let (ok, error_msg) = fetch_and_summarize(state, post_id, hn_id, &title, url.as_deref(), &config.article, &config.llm).await;

    {
        let conn = state.db.lock().expect("db lock");
        let status = if ok { "done" } else { "error" };
        let error_msg = if ok { None } else { Some(error_msg.as_str()) };
        if let Err(e) = db::update_fetch_status(&conn, hn_id, status, error_msg) {
            error!(%hn_id, error = %e, "failed to set {} status", status);
        }
        if !ok {
            let _ = db::increment_retry_count(&conn, hn_id);
        }
    }

    info!(%hn_id, ok, "reprocess complete");
}

/// Re-process any posts left in `pending` status from a previous crash.
async fn recover_pending_posts(state: &AppState) {
    let pending = {
        let conn = state.db.lock().expect("db lock");
        db::get_pending_posts(&conn).unwrap_or_default()
    };

    if pending.is_empty() {
        return;
    }

    info!(count = pending.len(), "recovering posts left in pending state");

    for (hn_id, title, url) in pending {
        let post_id = {
            let conn = state.db.lock().expect("db lock");
            let mut stmt = match conn.prepare("SELECT id FROM posts WHERE hn_id = ?1") {
                Ok(s) => s,
                Err(e) => {
                    error!(%hn_id, error = %e, "failed to query post_id for recovery");
                    continue;
                }
            };
            match stmt.query_row(rusqlite::params![hn_id], |row| row.get::<_, i64>(0)) {
                Ok(id) => id,
                Err(e) => {
                    error!(%hn_id, error = %e, "post not found for recovery");
                    continue;
                }
            }
        };

        info!(%hn_id, "recovering pending post");
        let (ok, error_msg) = fetch_and_summarize(
            state,
            post_id,
            hn_id,
            &title,
            url.as_deref(),
            &state.config.article,
            &state.config.llm,
        )
        .await;

        {
            let conn = state.db.lock().expect("db lock");
            let status = if ok { "done" } else { "error" };
            let error_msg = if ok { None } else { Some(error_msg.as_str()) };
            if let Err(e) = db::update_fetch_status(&conn, hn_id, status, error_msg) {
                error!(%hn_id, error = %e, "failed to set recovery status");
            }
            if !ok {
                let _ = db::increment_retry_count(&conn, hn_id);
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    info!("pending post recovery complete");
}

/// Retry posts that previously ended in `error` status, up to `MAX_RETRIES` times.
/// Returns the number of posts retried.
async fn retry_errored_posts(state: &AppState) -> usize {
    let errored = {
        let conn = state.db.lock().expect("db lock");
        db::get_errored_posts(&conn, MAX_RETRIES).unwrap_or_default()
    };

    if errored.is_empty() {
        return 0;
    }

    info!(count = errored.len(), "retrying errored posts");

    for (hn_id, title, url) in &errored {
        let (post_id, current_retry_count) = {
            let conn = state.db.lock().expect("db lock");
            let mut stmt = match conn.prepare("SELECT id, retry_count FROM posts WHERE hn_id = ?1") {
                Ok(s) => s,
                Err(e) => {
                    error!(%hn_id, error = %e, "failed to query post for retry");
                    continue;
                }
            };
            match stmt.query_row(rusqlite::params![hn_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            }) {
                Ok((id, rc)) => (id, rc),
                Err(e) => {
                    error!(%hn_id, error = %e, "post not found for retry");
                    continue;
                }
            }
        };

        info!(%hn_id, title, attempt = current_retry_count + 1, max = MAX_RETRIES, "retrying errored post");

        let (ok, error_msg) = fetch_and_summarize(
            state,
            post_id,
            *hn_id,
            title,
            url.as_deref(),
            &state.config.article,
            &state.config.llm,
        )
        .await;

        {
            let conn = state.db.lock().expect("db lock");
            let status = if ok { "done" } else { "error" };
            let error_msg = if ok { None } else { Some(error_msg.as_str()) };
            if let Err(e) = db::update_fetch_status(&conn, *hn_id, status, error_msg) {
                error!(%hn_id, error = %e, "failed to set retry status");
            }
            if !ok {
                let _ = db::increment_retry_count(&conn, *hn_id);
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    errored.len()
}

/// Returns `(success, error_message)`. A `false` success means at least one step failed;
/// the error message describes which steps failed.
async fn fetch_and_summarize(
    state: &AppState,
    post_id: i64,
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
        if !u.is_empty() {
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

    if let Some(ref summary) = post_result {
        let conn = state.db.lock().expect("db lock");
        if let Err(e) = db::insert_summary(&conn, post_id, "post", summary, &llm_config.model) {
            error!(%hn_id, error = %e, "failed to insert post summary");
        }
    } else if post_attempted {
        error!(%hn_id, "post story text summary failed");
        ok = false;
        errors.push("post text summary failed");
    }

    if let Some(ref summary) = comments_result {
        let conn = state.db.lock().expect("db lock");
        if let Err(e) = db::insert_summary(&conn, post_id, "comments", summary, &llm_config.model) {
            error!(%hn_id, error = %e, "failed to insert comments summary");
        }
    } else if comments_attempted {
        error!(%hn_id, "comments summary failed");
        ok = false;
        errors.push("comments summary failed");
    }

    if let Some(ref summary) = article_result {
        let conn = state.db.lock().expect("db lock");
        if let Err(e) = db::insert_summary(&conn, post_id, "article", summary, &llm_config.model) {
            error!(%hn_id, error = %e, "failed to insert article summary");
        }
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


