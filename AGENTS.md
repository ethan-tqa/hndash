# hndash — Agent Guide

## Quick start
```bash
cp config.toml.example config.toml    # edit: fill in deepseek api_key
cargo run
```
Validates API key via `GET /v1/models`, binds `127.0.0.1:8080`, immediately starts a fetch cycle, then schedules periodic fetches.

## Commands
- `cargo build` / `cargo run` — No test, lint, or CI infrastructure exists.
- CLI overrides: `cargo run -- --db-path <path> --host <ip> --port <port> --api-key <key>`

## Architecture (single crate, no workspace)
```
src/
├── main.rs         # axum 0.8 server, routes, fetch cycle orchestration
├── config.rs       # config.toml + clap CLI overrides + API key validation
├── db.rs           # SQLite init & CRUD (rusqlite bundled, WAL mode, FK enabled)
├── fetcher.rs      # HN Algolia API client (search, item detail, comments)
├── summarizer.rs   # DeepSeek chat completions API client
├── article.rs      # HTTP article fetch + HTML→text (tl crate) + archive fallback chain
├── search.rs       # Tantivy full-text search index
└── models.rs       # Post, Summary, Config, ImportItem, ArticleConfig structs
templates/
├── index.html      # dashboard (minijinja + `urlencode` filter)
└── import.html     # import-by-URL page
ext/                # standalone Chrome MV3 extension ("Tab Closer") — not part of hndash
```

## Routes (axum 0.8 — uses `{param}`, not `:param`)
| Method | Path | Purpose |
|--------|------|---------|
| GET | `/` | Dashboard (pagination via `?page=N`) |
| POST | `/refresh` | Trigger fetch cycle (409 if already running) |
| POST | `/resummarize/{hn_id}` | Re-summarize a post |
| POST | `/mark-read-post/{hn_id}` | Mark one post read |
| POST | `/mark-read-all` | Mark all read |
| POST | `/remove-post/{hn_id}` | Delete a post (removes from search index) |
| POST | `/remove-all-posts` | Delete all posts (clears search index) |
| GET/POST | `/import` | Import by HN URL or numeric ID |
| POST | `/retry-imports` | Retry failed imports |
| GET | `/search` | Full-text search (`?q=...&page=N`) |

## Key quirks
- **Rust edition 2024** — unusual syntax features (e.g. `impl Trait` in RPIT). Match existing code style.
- **No `chrono`** — custom `ts_to_iso()` in `main.rs`. SQLite uses `datetime('now')` for timestamps.
- **DB behind `Arc<Mutex<Connection>>`** — all DB access via `with_db()` helper which spawns a `tokio::task::spawn_blocking` (rusqlite is not async).
- **LLM calls run concurrently per post** — post, comments, and article summaries are fetched via `tokio::join!` within `fetch_and_summarize`. No forced delay between them.
- **500ms delay** between posts, between Algolia pages, and between import queue items.
- **LLM config is DeepSeek-specific**: `reasoning_effort: "high"` and `thinking: { type: "enabled" }` are always sent. `max_tokens: 512`.
- **Context limits**: post text = 8K chars, comments = 40K chars, article = 80K chars (truncated before sending).
- **Crash recovery**: posts stuck in `pending` status and imports stuck in `processing` are resumed on startup.
- **Permanent failures**: PDFs, YouTube, and known paywalled domains (`wsj.com`, `bloomberg.com`, `nytimes.com`, etc.) are never retried — checked via `is_permanent_failure()`.
- **Retry logic**: errored posts retried up to `MAX_RETRIES = 3`, stopped early if marked `permanent_failure`.
- **Search index**: Tantivy index stored at `<db_path>_search_index`; rebuilt on startup, updated incrementally per-post.
- **Article fallback**: tries original URL → iterates `config.fallback_order` (default: `jina_reader`, `web.archive.org`, `archive.is`). Order is configurable.
- **`.gitignore`d**: `config.toml`, `/target`, `/logs/`, `hndash_search_index/`
- **Logging**: stdout at `info` level via `RUST_LOG` env var; file at `warn` level to `logs/hndash.log` (daily rotation).
- **Panic hook** logs full backtrace to tracing before re-panicking.
- **`ext/`** is a standalone Chrome MV3 extension — do not modify unless explicitly asked.
