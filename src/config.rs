use std::path::PathBuf;

use clap::Parser;
use crate::models::Config;

/// CLI arguments parsed via clap.
#[derive(Parser, Debug)]
#[command(name = "hndash", about = "HackerNews Dashboard")]
pub struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    pub config: PathBuf,

    #[arg(long)]
    pub db_path: Option<String>,

    #[arg(long)]
    pub host: Option<String>,

    #[arg(long)]
    pub port: Option<u16>,

    #[arg(long)]
    pub api_key: Option<String>,
}

/// Read `config.toml`, merge with CLI overrides, exit on error.
pub fn load_config() -> Config {
    let cli = Cli::parse();

    if !cli.config.exists() {
        tracing::error!(
            "Config file not found at {:?}. Copy config.toml.example to config.toml \
             and fill in your settings.",
            cli.config
        );
        std::process::exit(1);
    }
    let config_path = cli.config;

    let content = std::fs::read_to_string(&config_path).unwrap_or_else(|e| {
        tracing::error!("Failed to read config file {:?}: {}", config_path, e);
        std::process::exit(1);
    });

    let mut config: Config = toml::from_str(&content).unwrap_or_else(|e| {
        tracing::error!("Failed to parse config file: {}", e);
        std::process::exit(1);
    });

    if let Some(db_path) = cli.db_path {
        config.db.path = db_path;
    }
    if let Some(host) = cli.host {
        config.server.host = host;
    }
    if let Some(port) = cli.port {
        config.server.port = port;
    }
    if let Some(api_key) = cli.api_key {
        config.llm.api_key = api_key;
    }

    config
}

/// Verify the DeepSeek API key by calling `GET /v1/models`. Exit on failure.
pub async fn validate_api_key(config: &Config) {
    let client = reqwest::Client::new();
    let base_url = config.llm.base_url.trim_end_matches('/');
    let url = format!("{}/v1/models", base_url);

    match client
        .get(&url)
        .header("Authorization", format!("Bearer {}", config.llm.api_key))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!("DeepSeek API key validated successfully");
        }
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::error!("DeepSeek API key validation failed ({}): {}", status, body);
            std::process::exit(1);
        }
        Err(e) => {
            tracing::error!("Could not reach DeepSeek API: {}", e);
            std::process::exit(1);
        }
    }
}
