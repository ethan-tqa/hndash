# hndash — HackerNews Dashboard

A lightweight Rust dashboard that surfaces interesting HackerNews posts by fetching them from the official Algolia API, summarizing post content, comments, and linked articles via a local LLM (DeepSeek), and presenting everything in a minimal web UI.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  config.toml                                                │
│  ── on startup, if missing → error + suggest example        │
│  ── API key validated via DeepSeek /models endpoint         │
└──────────┬──────────────────────────────────────────────────┘
           │
┌──────────▼──────────────────────────────────────────────────┐
│  axum server (127.0.0.1:8080)                               │
│  ├─ GET  /              — dashboard page                    │
│  ├─ POST /refresh       — trigger fetch cycle               │
│  ├─ POST /resummarize   — re-summarize one post             │
│  ├─ POST /mark-read     — mark summary as read              │
│  └─ POST /mark-read-all — mark all as read                  │
│                                                             │
│  State: AtomicBool (fetch_in_progress)                      │
│         rusqlite::Connection (behind Mutex)                 │
└──────────┬──────────────────────────────────────────────────┘
           │
┌──────────▼──────────────────────────────────────────────────┐
│  Fetch Cycle (on startup + every N min + manual)            │
│                                                             │
│  Step 1: Paginate Algolia until we have enough new posts    │
│           or run out of pages                                │
│    → GET /api/v1/search?tags=story&numericFilters=...       │
│      &hitsPerPage=50&page=0..N                              │
│    → 500ms delay between pages                              │
│    → Ignore posts already in DB (by hn_id)                  │
│                                                             │
│  Step 2: For each new post (sequentially):                  │
│    INSERT post with status='pending'                        │
│    ├─ Fetch post details (individual Algolia call)          │
│    ├─ Fetch top 30 comments                                 │
│    ├─ Fetch article (with archive fallback chain)           │
│    ├─ Delay 500ms                                           │
│    ├─ LLM: summarize post text                              │
│    ├─ Delay 1s                                              │
│    ├─ LLM: summarize comments                               │
│    ├─ Delay 1s                                              │
│    ├─ LLM: summarize article (skip if fetch failed)         │
│    └─ UPDATE status='done'                                  │
│                                                             │
│  On any failure: UPDATE status='error', log, continue       │
└──────────┬──────────────────────────────────────────────────┘
           │
┌──────────▼──────────────────────────────────────────────────┐
│  SQLite (hndash.db)                                         │
│                                                             │
│  posts(hn_id, title, url, author, points,                  │
│        num_comments, created_at, fetched_at,                │
│        fetch_status TEXT DEFAULT 'pending')                  │
│    — fetch_status IN ('pending','done','error')             │
│                                                             │
│  summaries(post_id, summary_type, content, model,          │
│            created_at, read_at)                              │
│    — summary_type IN ('post','comments','article')          │
│    — read_at = NULL means unread                            │
└─────────────────────────────────────────────────────────────┘
```

## Project Structure

```
hndash/
├── Cargo.toml
├── config.toml.example
├── DESIGN.md
├── TODO.md
├── src/
│   ├── main.rs           # axum server, routes, periodic task
│   ├── config.rs         # deserialize config.toml + clap overrides
│   ├── db.rs             # rusqlite init, CRUD for posts & summaries
│   ├── models.rs         # Post, Summary, Config structs
│   ├── fetcher.rs        # HN Algolia API client
│   ├── summarizer.rs     # DeepSeek chat completions API client
│   └── article.rs        # HTTP fetch + HTML→text extraction + archive fallback
└── templates/
    └── index.html        # minijinja template for the dashboard page
```

## Dependencies

| Crate | Purpose |
|---|---|
| `tokio` (full) | Async runtime + interval timers |
| `axum` 0.8 | HTTP web framework |
| `serde` / `serde_json` | JSON deserialization |
| `rusqlite` (bundled) | SQLite |
| `reqwest` (json) | HTTP client for HN API, article fetching, DeepSeek API |
| `tl` | HTML parsing for article text extraction |
| `tower-http` (cors) | CORS middleware |
| `clap` (derive) | CLI args |
| `toml` | Config parsing |
| `tracing` / `tracing-subscriber` | Logging |
| `minijinja` | HTML templating |

## Database Schema

```sql
CREATE TABLE posts (
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
                    CHECK(fetch_status IN ('pending','done','error'))
);

CREATE TABLE summaries (
    id            INTEGER PRIMARY KEY,
    post_id       INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    summary_type  TEXT NOT NULL CHECK(summary_type IN ('post','comments','article')),
    content       TEXT NOT NULL,
    model         TEXT NOT NULL,
    created_at    TEXT NOT NULL DEFAULT (datetime('now')),
    read_at       TEXT,
    UNIQUE(post_id, summary_type)
);

CREATE INDEX idx_posts_hn_id ON posts(hn_id);
CREATE INDEX idx_summaries_post ON summaries(post_id);
```

## Configuration (config.toml)

```toml
[hn]
min_points = 20
min_comments = 10
fetch_interval_minutes = 30
max_fetch_pages = 5

[llm]
api_key = "sk-your-deepseek-api-key"
model = "deepseek-chat"
base_url = "https://api.deepseek.com"

[server]
host = "127.0.0.1"
port = 8080

[db]
path = "hndash.db"

[article]
fallback_order = ["archive.is", "web.archive.org"]
timeout_secs = 30
max_bytes = 204800
min_text_length = 100
user_agent = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
```

## Fallback Chain for Article Fetching

1. **Try original URL** — standard HTTP GET with browser-like headers
   - Success & extracted text ≥ `min_text_length` → return text
   - Fail (4xx/5xx/timeout) OR text too short OR binary content-type → fall through

2. **Try archive.is** — `GET https://archive.is/newest/<encoded_url>`
   - Redirects to most recent snapshot if one exists
   - Detect failure: page title contains "not found" OR extracted text < 200 chars
   - Success → extract and return text

3. **Try Wayback Machine (web.archive.org)**
   - CDX API: `GET /cdx/search/cdx?url=<url>&output=json&limit=1&fl=timestamp`
   - If snapshot exists: `GET /web/<timestamp>/<url>`
   - Success → extract and return text

4. **All failed** → return `None` → skip article summary

## API Routes

| Route | Method | Purpose |
|---|---|---|
| `/` | GET | Dashboard: table of posts with expandable summaries |
| `/refresh` | POST | Trigger fetch cycle immediately (409 if already running) |
| `/resummarize/:hn_id` | POST | Delete existing summaries for a post and re-queue it |
| `/mark-read/:hn_id/:type` | POST | Mark a specific summary as read |
| `/mark-read-all` | POST | Mark all summaries as read |

## Design Decisions

| # | Concern | Decision |
|---|---------|----------|
| 1 | Algolia pagination | Page through `max_fetch_pages` pages (5), 500ms between pages, skip duplicates |
| 2 | Article extraction | Plain HTTP with browser UA. JS-rendered/blocked sites fail gracefully |
| 3 | LLM context limits | Truncate article to 80K chars, comments to 40K chars before sending |
| 4 | Error handling | `fetch_status = done/error`. Failed posts visible in UI with retry button |
| 5 | Concurrent fetch guard | `AtomicBool` — returns 409 if a cycle is already running |
| 6 | First fetch timing | Immediately on startup after server binds |
| 7 | Re-summarization | Skip if all 3 summary rows exist. `/resummarize` deletes and re-processes |
| 8 | Config missing on 1st run | Print clear error with path to example file |
| 9 | API key validation | Test call to DeepSeek `/v1/models` on startup |
| 10 | API usage | All outbound calls sequential with delays (500ms–1s between calls) |
