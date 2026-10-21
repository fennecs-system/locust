use super::{api_key, client};
use crate::{
    classify, with_retries, Provider, ProviderSettings, Request, Response, RetryError, Usage,
};
use anyhow::{Context, Result};
use async_trait::async_trait;

pub struct GeminiProvider {
    settings: ProviderSettings,
    http: reqwest::Client,
    base_url: String,
    key: String,
}

impl GeminiProvider {
    pub fn new(
        settings: ProviderSettings,
        base_url: Option<String>,
        key_env: &str,
    ) -> Result<Self> {
        Ok(Self {
            http: client(&settings)?,
            base_url: base_url
                .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".into()),
            key: api_key(key_env)?,
            settings,
        })
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    fn name(&self) -> &str {
        &self.settings.name
    }

    fn model(&self) -> &str {
        &self.settings.model
    }

    async fn complete(&self, req: Request) -> Result<Response> {
        let body = serde_json::json!({
            "systemInstruction": { "parts": [{ "text": req.system }] },
            "contents": [{ "role": "user", "parts": [{ "text": req.prompt }] }],
            "generationConfig": {
                "temperature": req.temperature.unwrap_or(self.settings.temperature),
                "maxOutputTokens": req.max_tokens.unwrap_or(self.settings.max_tokens),
            },
        });

        let value: serde_json::Value = with_retries(&self.settings, |_| {
            let http = &self.http;
            let url = format!(
                "{}/models/{}:generateContent",
                self.base_url.trim_end_matches('/'),
                self.settings.model
            );
            let key = &self.key;
            let body = body.clone();
            async move {
                let resp = http
                    .post(&url)
                    .header("x-goog-api-key", key)
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

        let text = value["candidates"][0]["content"]["parts"]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .context("response had no candidate parts")?;

        let input = value["usageMetadata"]["promptTokenCount"]
            .as_u64()
            .unwrap_or(0);
        let output = value["usageMetadata"]["candidatesTokenCount"]
            .as_u64()
            .unwrap_or(0);

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
