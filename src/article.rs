use std::fs;
use std::time::Duration;

use reqwest::Client;
use tl::Node;
use tracing::warn;

use crate::models::ArticleConfig;

const LOW_QUALITY_PATTERNS: &[&str] = &[
    "verify you are human",
    "verify your identity",
    "captcha",
    "checking your browser",
    "cloudflare",
    "attention required",
    "please enable cookies",
    "access denied",
    "legal reasons",
    "cannot access",
    "just a moment...",
    "challenge platform",
    "ddos protection",
    "blocked",
];

fn is_low_quality(text: &str) -> bool {
    let lower = text.to_lowercase();
    LOW_QUALITY_PATTERNS.iter().any(|&p| lower.contains(p))
}

fn dump_body(hn_id: i64, source: &str, body: &str) {
    if let Err(e) = fs::create_dir_all("logs/article_dumps") {
        warn!(%hn_id, %e, "failed to create article dump directory");
        return;
    }
    let path = format!("logs/article_dumps/{}_{}.html", hn_id, source);
    if let Err(e) = fs::write(&path, body) {
        warn!(%hn_id, %e, "failed to write article dump");
    }
}

/// Fetch an article URL and extract readable text, falling through configured
/// fallbacks (Jina Reader, Wayback Machine, archive.is, etc.).
pub async fn fetch_article(config: &ArticleConfig, url: &str, hn_id: i64) -> Option<String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(config.timeout_secs))
        .user_agent(&config.user_agent)
        .build()
        .ok()?;

    let html = fetch_url(&client, url, config.max_bytes).await;
    if let Some(ref html) = html {
        let text = extract_text(html);
        if text.len() >= config.min_text_length && !is_low_quality(&text) {
            return Some(text);
        }
        dump_body(hn_id, "original", html);
        warn!(%url, %hn_id, extracted = text.len(), html_len = html.len(),
            "original fetch text too short or low quality, body dumped"
        );
    }

    for (i, fallback) in config.fallback_order.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        match fallback.as_str() {
            "jina_reader" => {
                if let Some(text) = try_jina_reader(&client, url, config, hn_id).await {
                    return Some(text);
                }
            }
            "web.archive.org" => {
                if let Some(text) = try_wayback(&client, url, config, hn_id).await {
                    return Some(text);
                }
            }
            "archive.is" => {
                if let Some(text) = try_archive_is(&client, url, config, hn_id).await {
                    return Some(text);
                }
            }
            other => warn!("unknown fallback in config: {}", other),
        }
    }

    None
}

async fn fetch_url(client: &Client, url: &str, max_bytes: usize) -> Option<String> {
    let response = client
        .get(url)
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .header("Accept-Language", "en-US,en;q=0.5")
        .send()
        .await
        .ok()?;

    let status = response.status();
    if !status.is_success() {
        warn!(%url, %status, "non-success status");
        return None;
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type.starts_with("text/")
        && !content_type.contains("html")
        && !content_type.contains("xml")
    {
        warn!(%url, %content_type, "non-text content type");
        return None;
    }

    let body = response.bytes().await.ok()?;
    if body.len() > max_bytes {
        warn!(%url, len = body.len(), max = max_bytes, "response body exceeds max_bytes, truncating");
    }
    let body = &body[..body.len().min(max_bytes)];
    let body_str = String::from_utf8_lossy(body).to_string();
    Some(body_str)
}

async fn try_archive_is(client: &Client, url: &str, config: &ArticleConfig, hn_id: i64) -> Option<String> {
    let encoded = urlencoding::encode(url);
    let archive_url = format!("https://archive.is/newest/{}", encoded);

    for attempt in 0..3 {
        let response = client
            .get(&archive_url)
            .header("User-Agent", &config.user_agent)
            .send()
            .await
            .ok()?;

        let status = response.status();

        let body = response.bytes().await.ok()?;
        let html = String::from_utf8_lossy(&body).to_string();

        let body_lower = html.to_lowercase();
        let rate_limited = status == 429
            || body_lower.contains("rate limit")
            || body_lower.contains("too many requests")
            || body_lower.contains("try again later");

        if rate_limited {
            let delay = 5 * (attempt + 1);
            warn!(%url, attempt, %delay, "archive.is rate limited, retrying after backoff");
            tokio::time::sleep(Duration::from_secs(delay)).await;
            continue;
        }

        if !status.is_success() {
            warn!(%url, %status, "archive.is non-success");
            return None;
        }

        let text = extract_text(&html);

        if is_low_quality(&text) {
            dump_body(hn_id, "archive_is", &html);
            warn!(%url, %hn_id, attempt, "archive.is low quality content, body dumped");
            continue;
        }

        if text.len() >= 200 {
            return Some(text);
        }

        dump_body(hn_id, "archive_is", &html);
        warn!(%url, %hn_id, extracted = text.len(), html_len = html.len(),
            "archive.is text too short or not found, body dumped"
        );
    }

    None
}

async fn try_wayback(client: &Client, url: &str, config: &ArticleConfig, hn_id: i64) -> Option<String> {
    let cdx_url = format!(
        "https://web.archive.org/cdx/search/cdx?url={}&output=json&limit=3&fl=timestamp,statuscode",
        urlencoding::encode(url)
    );

    let resp = client.get(&cdx_url).send().await.ok()?;
    let rows: Vec<Vec<String>> = resp.json().await.ok()?;

    for i in 1..rows.len() {
        let timestamp = rows.get(i)?.first()?.clone();
        let status_code = rows.get(i)?.get(1).cloned();

        // Skip snapshots that returned errors
        if let Some(code) = &status_code {
            if code.starts_with('4') || code.starts_with('5') {
                continue;
            }
        }

        // Use id_ modifier to get raw content without Wayback banner
        let snapshot_url = format!("https://web.archive.org/web/{}id_/{}", timestamp, url);

        let html = fetch_url(client, &snapshot_url, config.max_bytes).await?;
        let text = extract_text(&html);

        if text.len() < config.min_text_length {
            dump_body(hn_id, "wayback", &html);
            warn!(%url, %hn_id, extracted = text.len(), min = config.min_text_length,
                "wayback snapshot text too short, body dumped, trying next snapshot");
            continue;
        }

        if is_low_quality(&text) {
            dump_body(hn_id, "wayback", &html);
            warn!(%url, %hn_id, "wayback snapshot low-quality content, body dumped, trying next snapshot");
            continue;
        }

        return Some(text);
    }

    None
}

async fn try_jina_reader(client: &Client, url: &str, config: &ArticleConfig, hn_id: i64) -> Option<String> {
    let reader_url = format!("https://r.jina.ai/{}", url);

    let response = client
        .get(&reader_url)
        .header("Accept", "text/plain, text/markdown, text/html")
        .header("User-Agent", &config.user_agent)
        .send()
        .await
        .ok()?;

    let status = response.status();
    if !status.is_success() {
        warn!(%url, %status, "jina reader non-success");
        return None;
    }

    let body = response.bytes().await.ok()?;
    let text = String::from_utf8_lossy(&body).to_string();

    if is_low_quality(&text) {
        dump_body(hn_id, "jina", &text);
        warn!(%url, %hn_id, "jina reader returned low-quality content, body dumped");
        return None;
    }

    if text.len() < config.min_text_length {
        dump_body(hn_id, "jina", &text);
        warn!(%url, %hn_id, len = text.len(), "jina reader text too short, body dumped");
        return None;
    }

    Some(text)
}

fn extract_text(html: &str) -> String {
    let dom = match tl::parse(html, tl::ParserOptions::default()) {
        Ok(d) => d,
        Err(_) => return String::new(),
    };
    let parser = dom.parser();

    let mut text = String::new();

    if let Some(iter) = dom.query_selector("article") {
        for handle in iter {
            if let Some(node) = handle.get(parser) {
                if let Some(tag) = node.as_tag() {
                    collect_node_text(parser, tag, &mut text);
                    text.push('\n');
                }
            }
        }
        if !text.trim().is_empty() {
            return collapse_whitespace(&text);
        }
        text.clear();
    }

    if let Some(iter) = dom.query_selector("p") {
        for handle in iter {
            if let Some(node) = handle.get(parser) {
                if let Some(tag) = node.as_tag() {
                    collect_node_text(parser, tag, &mut text);
                    text.push('\n');
                }
            }
        }
        if !text.trim().is_empty() {
            return collapse_whitespace(&text);
        }
        text.clear();
    }

    if let Some(iter) = dom.query_selector("body") {
        for handle in iter {
            if let Some(node) = handle.get(parser) {
                if let Some(tag) = node.as_tag() {
                    collect_node_text(parser, tag, &mut text);
                }
            }
        }
    }

    collapse_whitespace(&text)
}

fn collect_node_text(parser: &tl::Parser, tag: &tl::HTMLTag, out: &mut String) {
    for child_handle in tag.children().top().iter() {
        let node = match child_handle.get(parser) {
            Some(n) => n,
            None => continue,
        };
        match node {
            Node::Tag(child_tag) => {
                let name = child_tag.name();
                if name == "script" || name == "style" || name == "noscript" {
                    continue;
                }
                collect_node_text(parser, child_tag, out);
                let name_str = name.as_utf8_str();
                match name_str.as_ref() {
                    "p" | "br" | "div" | "h1" | "h2" | "h3" | "li" | "tr" | "td" => {
                        out.push(' ');
                    }
                    _ => {}
                }
            }
            Node::Raw(bytes) => {
                out.push_str(&bytes.as_utf8_str());
            }
            Node::Comment(_) => {}
        }
    }
}

fn collapse_whitespace(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_was_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_was_space {
                result.push(' ');
                prev_was_space = true;
            }
        } else {
            result.push(ch);
            prev_was_space = false;
        }
    }
    result.trim().to_string()
}
