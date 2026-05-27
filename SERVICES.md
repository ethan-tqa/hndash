# Article Content Extraction — Services & Libraries

This project fetches article text from URLs to feed to the LLM for summarization.
The current pipeline is:

1. **Try the original URL** directly — extract text via HTML parsing
2. **Fall through configured archives** (`jina_reader`, `web.archive.org`, `archive.is` in order)

Below are freely available alternatives for each layer.

---

## External Services (drop-in fallback options)

These work by prepending an API URL to the article URL, similar to `r.jina.ai`.

| Service | URL Pattern | Free Tier | Notes |
|---|---|---|---|
| **Jina AI Reader** | `https://r.jina.ai/{url}` | No key needed (rate-limited) | Already integrated. Returns clean markdown. |
| **Firecrawl** | Self-host or `api.firecrawl.dev/v1/scrape` | 500 pages/month, open-source (AGPL) | Can be self-hosted via Docker. Has Rust SDK. |
| **ScrapingBee** | `https://app.scrapingbee.com/api/v1?api_key=...&url=...` | 1,000 req/month (free API key) | Handles JS rendering, proxies. |
| **ScraperAPI** | `https://api.scraperapi.com?api_key=...&url=...` | 1,000 req/month (free API key) | Proxy rotation, CAPTCHA handling. |

Trade-off: all except Jina require an API key or self-hosting.

---

## Rust Crates — Local Article Extraction

Replacing the current custom `extract_text()` with a proper crate would improve extraction
quality and reduce reliance on external archives.

### Mozilla Readability ports

| Crate | Notes |
|---|---|
| **readabilityrs** | Active Rust port of Mozilla Readability |
| **readable-rs** | Native Rust port, pure Rust |
| **legible** | Another Readability.js port, well-maintained |
| **readable-readability** | "Really fast" readability |
| **readex** | Combines Readability + Trafilatura + htmldate — most comprehensive |

### General content extraction

| Crate | Notes |
|---|---|
| **dom_smoothie** | Very popular, active. Extracts relevant content directly. |
| **rs-trafilatura** | Rust port of Python's trafilatura (used by academic tools) |
| **html2text** | Mature, 1.5M+ downloads. HTML → plain text. |

### Recommendation

Replace `extract_text` with **`dom_smoothie`** or **`readabilityrs`** for a significant
improvement in local extraction quality. Add **Firecrawl** as an additional external
fallback for cases where the original URL is completely unparseable.
