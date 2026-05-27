use std::collections::HashMap;

use serde_json::value::RawValue;

use crate::models::HnConfig;

/// Flat comment with no recursive children field — used for stream-deserializing
/// top-level comments without recursing into nested replies.
#[derive(Debug, serde::Deserialize)]
struct FlatComment {
    id: u64,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

/// Algolia search API response containing a page of story hits.
#[derive(Debug, serde::Deserialize)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    #[serde(rename = "nbPages")]
    pub nb_pages: u32,
    pub page: u32,
}

/// A single story result from the Algolia search API.
#[derive(Debug, serde::Deserialize)]
pub struct SearchHit {
    #[serde(rename = "objectID")]
    pub object_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub points: Option<i32>,
    #[serde(default, rename = "num_comments")]
    pub num_comments: Option<i32>,
    #[serde(rename = "created_at_i")]
    pub created_at_i: i64,
    #[serde(default, rename = "created_at")]
    pub created_at: Option<String>,
}

/// Full item response from the Algolia items API, with top-level comments only.
#[derive(Debug)]
pub struct ItemResponse {
    pub id: u64,
    pub title: Option<String>,
    pub url: Option<String>,
    pub author: Option<String>,
    pub points: Option<i32>,
    pub num_comments: Option<i32>,
    pub created_at: Option<String>,
    pub children: Vec<Comment>,
    pub text: Option<String>,
}

/// A single HN comment (top-level only, no nested replies).
#[derive(Debug, Clone)]
pub struct Comment {
    pub id: u64,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub children: Vec<Comment>,
    pub text: Option<String>,
}

/// Build the Algolia search URL with numeric filters for a given page.
pub fn search_url(config: &HnConfig, page: u32) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let mut filters = vec![
        format!("points>={}", config.min_points),
        format!("num_comments>={}", config.min_comments),
    ];

    if config.max_age_hours > 0 {
        let cutoff = now.saturating_sub(config.max_age_hours * 3600);
        filters.push(format!("created_at_i>={}", cutoff));
    }

    if let Some(min_hours) = config.min_age_hours {
        if min_hours > 0 {
            let cutoff = now.saturating_sub(min_hours * 3600);
            filters.push(format!("created_at_i<={}", cutoff));
        }
    }

    let numeric = filters.join(",");
    format!(
        "https://hn.algolia.com/api/v1/search?tags=story&hitsPerPage=50&page={}&numericFilters={}",
        page,
        urlencoding::encode(&numeric)
    )
}

/// Fetch one page of stories from the Algolia HN search API.
pub async fn search_stories(
    client: &reqwest::Client,
    config: &HnConfig,
    page: u32,
) -> Option<SearchResponse> {
    let url = search_url(config, page);
    let resp = client.get(&url).send().await.ok()?;
    resp.json::<SearchResponse>().await.ok()
}

/// Fetch a single HN item (story or comment) with its top-level comments.
/// Uses `RawValue` + `StreamDeserializer` to avoid stack overflow from deeply
/// nested comment trees in the Algolia API response.
pub async fn fetch_item(
    client: &reqwest::Client,
    id: u64,
) -> Option<ItemResponse> {
    let url = format!("https://hn.algolia.com/api/v1/items/{}", id);
    let resp = client.get(&url).send().await.ok()?;
    let body = resp.bytes().await.ok()?;

    // Parse top-level object as a map of key → Box<RawValue>.
    // RawValue stores raw JSON bytes without recursive structure —
    // it simply counts bracket pairs to find each value's extent.
    let map: HashMap<String, Box<RawValue>> = serde_json::from_slice(&body).ok()?;

    let raw = |key: &str| map.get(key).map(|r| r.get());

    let id: u64 = serde_json::from_str(raw("id")?).ok()?;
    let title: Option<String> = raw("title").and_then(|s| serde_json::from_str(s).ok());
    let url: Option<String> = raw("url").and_then(|s| serde_json::from_str(s).ok());
    let author: Option<String> = raw("author").and_then(|s| serde_json::from_str(s).ok());
    let points: Option<i32> = raw("points")
        .and_then(|s| serde_json::from_str::<i64>(s).ok())
        .map(|v| v as i32);
    let num_comments: Option<i32> = raw("num_comments")
        .and_then(|s| serde_json::from_str::<i64>(s).ok())
        .map(|v| v as i32);
    let created_at: Option<String> = raw("created_at")
        .and_then(|s| serde_json::from_str(s).ok());
    let text: Option<String> = raw("text").and_then(|s| serde_json::from_str(s).ok());

    // Stream-deserialize children array — each element is FlatComment
    // (no recursive `children` field), so no stack frames are consumed
    // per comment regardless of nesting depth.
    let children: Vec<Comment> = raw("children")
        .map(|s| {
            serde_json::Deserializer::from_str(s)
                .into_iter::<FlatComment>()
                .take(30)
                .filter_map(|r| r.ok())
                .map(|fc| Comment {
                    id: fc.id,
                    author: fc.author,
                    created_at: fc.created_at,
                    children: Vec::new(),
                    text: fc.text,
                })
                .collect()
        })
        .unwrap_or_default();

    Some(ItemResponse {
        id,
        title,
        url,
        author,
        points,
        num_comments,
        created_at,
        children,
        text,
    })
}

/// Return the first N top-level comments from an item response.
pub fn top_comments(item: &ItemResponse, max: usize) -> Vec<&Comment> {
    item.children.iter().take(max).collect()
}
