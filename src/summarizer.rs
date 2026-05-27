use tracing::{info, warn};

const POST_MAX_CHARS: usize = 8_000;
const COMMENTS_MAX_CHARS: usize = 40_000;
const ARTICLE_MAX_CHARS: usize = 80_000;

#[derive(serde::Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

#[derive(serde::Serialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(serde::Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(serde::Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(serde::Deserialize)]
struct ChoiceMessage {
    content: String,
}

/// Summarize a HN post's title and optional body text via the DeepSeek API.
pub async fn summarize_post(
    client: &reqwest::Client,
    config: &crate::models::LlmConfig,
    title: &str,
    text: Option<&str>,
    hn_id: i64,
) -> Option<String> {
    let content = match text {
        Some(t) => format!("Title: {}\n\nText: {}", title, t),
        None => title.to_string(),
    };
    let content = truncate(&content, POST_MAX_CHARS);

    chat_completion(
        client,
        config,
        "You are a helpful assistant that summarizes HackerNews posts. \
         Provide a concise 2-3 sentence summary of the post.",
        &content,
        hn_id,
    )
    .await
}

/// Summarize HN comments text via the DeepSeek API.
pub async fn summarize_comments(
    client: &reqwest::Client,
    config: &crate::models::LlmConfig,
    comments_text: &str,
    hn_id: i64,
) -> Option<String> {
    let content = truncate(comments_text, COMMENTS_MAX_CHARS);

        chat_completion(
            client,
            config,
            "You are a helpful assistant that summarizes HackerNews comments. \
             Provide a concise 2-3 sentence summary of the key points \
             and opinions expressed in these comments.",
            &content,
            hn_id,
        )
        .await
}

/// Summarize extracted article text via the DeepSeek API.
pub async fn summarize_article(
    client: &reqwest::Client,
    config: &crate::models::LlmConfig,
    article_text: &str,
    hn_id: i64,
) -> Option<String> {
    let content = truncate(article_text, ARTICLE_MAX_CHARS);

        chat_completion(
            client,
            config,
            "You are a helpful assistant that summarizes articles. \
             Provide a concise 2-3 sentence summary of the article.",
            &content,
            hn_id,
        )
        .await
}

async fn chat_completion(
    client: &reqwest::Client,
    config: &crate::models::LlmConfig,
    system_prompt: &str,
    user_content: &str,
    hn_id: i64,
) -> Option<String> {
    let url = format!("{}/v1/chat/completions", config.base_url.trim_end_matches('/'));

    let body = ChatRequest {
        model: config.model.clone(),
        messages: vec![
            Message {
                role: "system".into(),
                content: system_prompt.to_string(),
            },
            Message {
                role: "user".into(),
                content: user_content.to_string(),
            },
        ],
        temperature: 0.3,
        max_tokens: Some(512),
    };

    let log_content = if user_content.len() > 500 {
        format!("{}... (truncated)", &user_content[..500])
    } else {
        user_content.to_string()
    };
    info!(
        hn_id,
        model = %config.model,
        temperature = body.temperature,
        max_tokens = ?body.max_tokens,
        system_prompt = %system_prompt,
        user_content = %log_content,
        "ai request"
    );

    let resp = match client
        .post(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(hn_id, error = %e, "ai request failed");
            return None;
        }
    };

    let chat: ChatResponse = match resp.json().await {
        Ok(c) => c,
        Err(e) => {
            warn!(hn_id, error = %e, "ai response parse failed");
            return None;
        }
    };

    let content = match chat.choices.into_iter().next() {
        Some(c) => c.message.content,
        None => {
            warn!(hn_id, "ai response no choices");
            return None;
        }
    };

    if content.trim().is_empty() {
        warn!(hn_id, "ai response empty");
        return None;
    }

    info!(hn_id, response = %content, "ai response");
    Some(content)
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }
    let mut truncated = String::with_capacity(max_chars);
    for ch in s.chars() {
        if truncated.len() + ch.len_utf8() > max_chars {
            truncated.push_str("...");
            break;
        }
        truncated.push(ch);
    }
    truncated
}
