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
}

/// A single HN comment with optional nested replies.
#[derive(Debug, serde::Deserialize, Clone)]
pub struct Comment {
    pub id: u64,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub children: Vec<Comment>,
}

/// Build the Algolia search URL with numeric filters for a given page.
pub fn search_url(config: &HnConfig, page: u32) -> String {
    let cutoff = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .saturating_sub(config.max_age_hours * 3600);

    let numeric = format!(
        "points>={},num_comments>={},created_at_i>={}",
        config.min_points, config.min_comments, cutoff
    );
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

/// Fetch a single HN item (story or comment) with its full comment tree by id.
pub async fn fetch_item(
    client: &reqwest::Client,
    id: u64,
) -> Option<ItemResponse> {
    let url = format!("https://hn.algolia.com/api/v1/items/{}", id);
    let resp = client.get(&url).send().await.ok()?;
    resp.json::<ItemResponse>().await.ok()
}

/// Return the first N top-level comments from an item response.
pub fn top_comments(item: &ItemResponse, max: usize) -> Vec<&Comment> {
    item.children.iter().take(max).collect()
}
