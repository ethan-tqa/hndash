use tracing::warn;

use crate::models::HnConfig;

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

/// Full item response from the Algolia items API, including nested comment tree.
#[derive(Debug, serde::Deserialize)]
pub struct ItemResponse {
    pub id: u64,
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
    #[serde(default, rename = "created_at")]
    pub created_at: Option<String>,
    #[serde(default)]
    pub children: Vec<Comment>,
    #[serde(default)]
    pub text: Option<String>,
}

/// A single HN comment with optional nested replies.
#[derive(Debug, serde::Deserialize, Clone)]
pub struct Comment {
    pub id: u64,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub children: Vec<Comment>,
    #[serde(default)]
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
        "https://hn.algolia.com/api/v1/search?tags=story&hitsPerPage={}&page={}&numericFilters={}",
        config.hits_per_page, page,
        urlencoding::encode(&numeric)
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{}... ({} total chars)", truncated, s.len())
    }
}

/// Fetch one page of stories from the Algolia HN search API.
pub async fn search_stories(
    client: &reqwest::Client,
    config: &HnConfig,
    page: u32,
) -> Option<SearchResponse> {
    let url = search_url(config, page);

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!(%page, error = %e, "search_stories HTTP error");
            return None;
        }
    };

    let status = resp.status();
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            warn!(%page, %status, error = %e, "search_stories failed to read response body");
            return None;
        }
    };

    if !status.is_success() {
        warn!(%page, %status, body = %truncate(&body, 500), "search_stories non-success status");
        return None;
    }

    match serde_json::from_str::<SearchResponse>(&body) {
        Ok(r) => Some(r),
        Err(e) => {
            warn!(%page, %status, error = %e, body = %truncate(&body, 500), "search_stories JSON parse error");
            None
        }
    }
}

/// Fetch a single HN item (story or comment) with its full comment tree by id.
pub async fn fetch_item(
    client: &reqwest::Client,
    id: u64,
) -> Option<ItemResponse> {
    let url = format!("https://hn.algolia.com/api/v1/items/{}", id);

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!(%id, error = %e, "fetch_item HTTP error");
            return None;
        }
    };

    let status = resp.status();
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            warn!(%id, %status, error = %e, "fetch_item failed to read response body");
            return None;
        }
    };

    if !status.is_success() {
        warn!(%id, %status, body = %truncate(&body, 500), "fetch_item non-success status");
        return None;
    }

    match serde_json::from_str::<ItemResponse>(&body) {
        Ok(item) => Some(item),
        Err(e) => {
            warn!(%id, %status, error = %e, body = %truncate(&body, 500), "fetch_item JSON parse error");
            None
        }
    }
}

/// Return the first N top-level comments from an item response.
pub fn top_comments(item: &ItemResponse, max: usize) -> Vec<&Comment> {
    item.children.iter().take(max).collect()
}
