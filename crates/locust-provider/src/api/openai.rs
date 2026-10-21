use super::{api_key, client};
use crate::{
    classify, with_retries, Provider, ProviderSettings, Request, Response, RetryError, Usage,
};
use anyhow::{Context, Result};
use async_trait::async_trait;

/// OpenAI chat-completions. `base_url` also makes this work for the many
/// API-compatible vendors (OpenRouter, Groq, DeepSeek, xAI, vLLM, Ollama).
pub struct OpenAiProvider {
    settings: ProviderSettings,
    http: reqwest::Client,
    base_url: String,
    key: String,
}

impl OpenAiProvider {
    pub fn new(
        settings: ProviderSettings,
        base_url: Option<String>,
        key_env: &str,
    ) -> Result<Self> {
        Ok(Self {
            http: client(&settings)?,
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".into()),
            key: api_key(key_env)?,
            settings,
        })
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        &self.settings.name
    }

    fn model(&self) -> &str {
        &self.settings.model
    }

    async fn complete(&self, req: Request) -> Result<Response> {
        let body = serde_json::json!({
            "model": self.settings.model,
            "max_completion_tokens": req.max_tokens.unwrap_or(self.settings.max_tokens),
            "temperature": req.temperature.unwrap_or(self.settings.temperature),
            "messages": [
                { "role": "system", "content": req.system },
                { "role": "user", "content": req.prompt },
            ],
        });

        let value: serde_json::Value = with_retries(&self.settings, |_| {
            let http = &self.http;
            let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
            let key = &self.key;
            let body = body.clone();
            async move {
                let resp = http
                    .post(&url)
                    .bearer_auth(key)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| RetryError::Transient(e.into()))?;

                let status = resp.status();
                let text = resp
                    .text()
                    .await
                    .map_err(|e| RetryError::Transient(e.into()))?;
                if !status.is_success() {
                    return Err(classify(status, &text));
                }
                serde_json::from_str(&text).map_err(|e| {
                    RetryError::Permanent(anyhow::Error::from(e).context("decoding response"))
                })
            }
        })
        .await?;

        let text = value["choices"][0]["message"]["content"]
            .as_str()
            .context("response had no message content")?
            .to_string();

        let input = value["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
        let output = value["usage"]["completion_tokens"].as_u64().unwrap_or(0);

        Ok(Response {
            text,
            usage: Usage {
                input_tokens: input,
                output_tokens: output,
                cost_usd: self.settings.estimate_cost(input, output),
            },
            provider: self.settings.name.clone(),
            model: self.settings.model.clone(),
        })
    }
}
