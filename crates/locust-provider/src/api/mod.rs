//! Native HTTP adapters. One module per wire format.

mod anthropic;
mod gemini;
mod openai;

pub use anthropic::AnthropicProvider;
pub use gemini::GeminiProvider;
pub use openai::OpenAiProvider;

use crate::ProviderSettings;
use anyhow::{Context, Result};

/// Which wire format a configured API provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wire {
    Anthropic,
    /// OpenAI chat-completions; also covers the many vendors that clone it
    /// (OpenRouter, Groq, DeepSeek, xAI, local servers) via `base_url`.
    OpenAi,
    Gemini,
}

pub(crate) fn client(settings: &ProviderSettings) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(settings.timeout_secs))
        .user_agent(concat!("locust/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")
}

pub(crate) fn api_key(var: &str) -> Result<String> {
    std::env::var(var)
        .with_context(|| format!("environment variable `{var}` is not set"))
        .map(|k| k.trim().to_string())
}
