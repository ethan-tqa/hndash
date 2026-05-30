use serde::{Deserialize, Serialize};

/// A single HN post fetched from Algolia and stored in SQLite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Post {
    pub id: i64,
    pub hn_id: i64,
    pub title: String,
    pub url: Option<String>,
    pub author: String,
    pub points: i64,
    pub num_comments: i64,
    pub created_at: String,
    pub fetched_at: String,
    pub fetch_status: String,
    pub read_at: Option<String>,
    pub retry_count: i64,
    pub error_message: Option<String>,
    #[serde(default)]
    pub permanent_failure: bool,
}

/// A generated summary for a post (post text, comments, or article).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub id: i64,
    pub post_id: i64,
    pub summary_type: String,
    pub content: String,
    pub model: String,
    pub created_at: String,
}

/// Top-level application configuration deserialized from `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub hn: HnConfig,
    pub llm: LlmConfig,
    pub server: ServerConfig,
    pub db: DbConfig,
    pub article: ArticleConfig,
}

/// HackerNews / Algolia search configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnConfig {
    pub min_points: u32,
    pub min_comments: u32,
    pub fetch_interval_minutes: u64,
    pub max_fetch_pages: u32,
    pub max_age_hours: u64,
    #[serde(default)]
    pub min_age_hours: Option<u64>,
    #[serde(default = "default_hits_per_page")]
    pub hits_per_page: u32,
}

fn default_hits_per_page() -> u32 {
    50
}

/// DeepSeek / OpenAI-compatible LLM API configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub api_key: String,
    pub model: String,
    pub base_url: String,
}

/// HTTP server bind configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

/// SQLite database path configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbConfig {
    pub path: String,
}

/// A post joined with its summaries, as returned by the dashboard query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostSummary {
    pub post: Post,
    pub summaries: Vec<Summary>,
}

/// Filter for dashboard post listing: unread, read, or all.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ReadFilter {
    #[default]
    Unread,
    Read,
    All,
}

impl ReadFilter {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReadFilter::Unread => "unread",
            ReadFilter::Read => "read",
            ReadFilter::All => "all",
        }
    }
}

/// An item in the import queue (pasted HN URLs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportItem {
    pub id: i64,
    pub hn_id: i64,
    pub url: String,
    pub status: String,
    pub error_message: Option<String>,
    pub created_at: String,
}

/// Article fetching and text extraction configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArticleConfig {
    pub fallback_order: Vec<String>,
    pub timeout_secs: u64,
    pub max_bytes: usize,
    pub min_text_length: usize,
    pub user_agent: String,
    #[serde(default)]
    pub paywalled_domains: Vec<String>,
}
