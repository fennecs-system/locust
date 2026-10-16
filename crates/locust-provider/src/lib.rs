//! Provider abstraction: how an agent reaches a model.
//!
//! Two adapters ship:
//!
//! * [`api`] — locust drives the HTTP API directly. Uniform across vendors,
//!   exact token accounting, and the model only sees the context pack we built
//!   for it. This is the default.
//! * [`cli`] — locust shells out to an agentic CLI (`claude -p`, `codex exec`,
//!   …) inside the agent's own worktree. The agent can read files, grep and run
//!   tests, which makes a finding provable rather than merely asserted.

use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;

pub mod api;
pub mod cli;
pub mod json;

pub use json::extract_json;

/// One model call.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub system: String,
    pub prompt: String,
    /// Checkout the agent may explore. Only meaningful for workspace-backed
    /// providers; API providers ignore it.
    pub workspace: Option<PathBuf>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Hint for logging and run artifacts, e.g. `gen1/attack/concurrency-hawk`.
    pub label: String,
}

impl Request {
    pub fn new(label: impl Into<String>, system: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            system: system.into(),
            prompt: prompt.into(),
            ..Default::default()
        }
    }

    pub fn in_workspace(mut self, path: Option<PathBuf>) -> Self {
        self.workspace = path;
        self
    }

    pub fn temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Vendor-reported cost when available; otherwise estimated from the
    /// configured per-million rates.
    pub cost_usd: f64,
}

impl Usage {
    pub fn merge(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cost_usd += other.cost_usd;
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub text: String,
    pub usage: Usage,
    pub provider: String,
    pub model: String,
}

/// A way of getting a completion out of a model.
///
/// Implementations must be safe to call concurrently and from many tasks at
/// once: the arena runs dozens of agents in parallel against a single instance
/// of each provider.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Name as written in `locust.toml`, e.g. `anthropic`.
    fn name(&self) -> &str;

    /// Model identifier, for reports and cost accounting.
    fn model(&self) -> &str;

    /// Whether this provider wants its own checkout. True for agentic CLIs.
    fn needs_workspace(&self) -> bool {
        false
    }

    /// Run one request to completion. Implementations own their own retry
    /// policy; the arena treats an error here as that agent failing, which
    /// shrinks the population for that phase rather than aborting the run.
    async fn complete(&self, req: Request) -> Result<Response>;
}

pub type SharedProvider = Arc<dyn Provider>;

/// Vendor-neutral knobs every adapter understands.
#[derive(Debug, Clone)]
pub struct ProviderSettings {
    pub name: String,
    pub model: String,
    pub max_tokens: u32,
    pub temperature: f32,
    pub timeout_secs: u64,
    pub input_cost_per_mtok: f64,
    pub output_cost_per_mtok: f64,
    pub max_retries: u32,
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            name: "anthropic".into(),
            model: "claude-opus-5".into(),
            max_tokens: 8192,
            temperature: 0.7,
            timeout_secs: 600,
            input_cost_per_mtok: 0.0,
            output_cost_per_mtok: 0.0,
            max_retries: 3,
        }
    }
}

impl ProviderSettings {
    pub fn estimate_cost(&self, input: u64, output: u64) -> f64 {
        (input as f64 / 1_000_000.0) * self.input_cost_per_mtok
            + (output as f64 / 1_000_000.0) * self.output_cost_per_mtok
    }
}

/// Retry wrapper shared by the HTTP adapters: exponential backoff on
/// rate limits and transient 5xx, immediate failure on anything else.
pub(crate) async fn with_retries<F, Fut, T>(settings: &ProviderSettings, mut f: F) -> Result<T>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, RetryError>>,
{
    let mut attempt = 0;
    loop {
        match f(attempt).await {
            Ok(v) => return Ok(v),
            Err(RetryError::Permanent(e)) => return Err(e),
            Err(RetryError::Transient(e)) => {
                if attempt >= settings.max_retries {
                    return Err(e.context(format!("giving up after {} attempts", attempt + 1)));
                }
                let backoff = std::time::Duration::from_millis(1000u64 << attempt.min(5));
                tracing::warn!(provider = %settings.name, attempt, "retrying in {backoff:?}: {e}");
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
        }
    }
}

pub(crate) enum RetryError {
    Transient(anyhow::Error),
    Permanent(anyhow::Error),
}

pub(crate) fn classify(status: reqwest::StatusCode, body: &str) -> RetryError {
    let e = anyhow::anyhow!("HTTP {status}: {}", body.chars().take(600).collect::<String>());
    if status.as_u16() == 429 || status.is_server_error() {
        RetryError::Transient(e)
    } else {
        RetryError::Permanent(e)
    }
}
