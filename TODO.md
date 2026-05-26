# hndash — Task Tracker

## Phase 1: Project Scaffold
- [x] Initialize Cargo.toml with all dependencies
- [x] Create `config.toml.example` file
- [x] Set up `tracing-subscriber` for logging

## Phase 2: Models & Configuration
- [x] `src/models.rs` — Define Post, Summary, Config structs
- [x] `src/config.rs` — Deserialize `config.toml`, CLI arg overrides via clap, API key validation on startup

## Phase 3: Database Layer
- [x] `src/db.rs` — Initialize SQLite, create tables
- [x] CRUD: insert/update post, insert summaries, query posts with summaries
- [x] CRUD: mark summary as read, mark all as read
- [x] CRUD: delete summaries for a post (resummarize)

## Phase 4: Article Extraction
- [x] `src/article.rs` — Fetch article via HTTP with browser-like headers
- [x] Extract readable text using `tl` (strip script/style, extract <article>/<p>)
- [x] archive.is fallback (detect "not found", check text length)
- [x] Wayback Machine CDX API fallback

## Phase 5: HN Fetcher
- [x] `src/fetcher.rs` — Algolia search with numeric filters (points, comments)
- [x] Pagination through multiple pages
- [x] Fetch individual post details
- [x] Fetch top 30 comments for a post
- [x] Deduplication against existing DB entries

## Phase 6: LLM Summarizer
- [x] `src/summarizer.rs` — DeepSeek chat completions API client
- [x] Summarize: post text prompt
- [x] Summarize: comments prompt
- [x] Summarize: article text prompt
- [x] Truncation for large inputs
- [x] Sequential calls with 1s delay between them

## Phase 7: Fetch Cycle Orchestration
- [x] `src/main.rs` — Fetch cycle function (startup + interval + manual)
- [x] Concurrent fetch guard (AtomicBool, return 409)
- [x] Status tracking (pending → done/error)
- [x] Cycle state: serialize all steps, one post at a time

## Phase 8: Web UI
- [x] `templates/index.html` — minijinja template
- [x] Post table with title, points, comments, status, read indicators
- [x] Expandable summaries (vanilla JS toggle)
- [x] Refresh button
- [x] Mark-all-read button
- [x] Per-post re-summarize button
- [x] Status bar (last fetch time, post count)
- [x] Minimal CSS in `<style>` block

## Phase 9: Routes & Server
- [x] `src/main.rs` — axum server setup
- [x] `GET /` — render dashboard
- [x] `POST /refresh` — trigger fetch cycle
- [x] `POST /resummarize/:hn_id` — re-summarize one post
- [x] `POST /mark-read/:hn_id/:type` — mark summary read
- [x] `POST /mark-read-all` — mark all read

## Phase 10: Polish
- [x] Graceful shutdown
- [x] Error messages display in UI
- [x] Verify config.toml.example matches actual config struct
- [x] Review logging output
- [x] Documentation comments on public functions
