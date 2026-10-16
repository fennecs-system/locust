//! Subprocess adapter for agentic CLIs.
//!
//! The payoff over the HTTP adapters is that the agent runs *inside its own
//! worktree*: it can open files the context pack omitted, grep for every call
//! site, and run the test suite to prove a finding instead of asserting it.
//! The cost is that token accounting depends on what the CLI chooses to report.

use crate::{extract_json, Provider, ProviderSettings, Request, Response, Usage};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

/// How to recover the assistant's text from the CLI's stdout.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Extract {
    /// stdout is the reply.
    #[default]
    Raw,
    /// stdout is a JSON object; read a slash-separated path, e.g. `result`.
    JsonPath(String),
    /// stdout is JSONL; concatenate the given path from every line that has it.
    JsonLines(String),
}

#[derive(Debug, Clone)]
pub struct CliSpec {
    /// argv. Supports `{system}`, `{prompt}` and `{model}` placeholders.
    pub command: Vec<String>,
    /// Written to the child's stdin; same placeholders.
    pub stdin: Option<String>,
    pub extract: Extract,
    /// Path under stdout JSON holding input/output token counts, if reported.
    pub usage_path: Option<String>,
    pub env: HashMap<String, String>,
}

pub struct CliProvider {
    settings: ProviderSettings,
    spec: CliSpec,
}

impl CliProvider {
    pub fn new(settings: ProviderSettings, spec: CliSpec) -> Result<Self> {
        if spec.command.is_empty() {
            bail!("provider `{}`: `command` must not be empty", settings.name);
        }
        Ok(Self { settings, spec })
    }

    fn render(&self, template: &str, req: &Request) -> String {
        template
            .replace("{system}", &req.system)
            .replace("{prompt}", &req.prompt)
            .replace("{model}", &self.settings.model)
    }
}

#[async_trait]
impl Provider for CliProvider {
    fn name(&self) -> &str {
        &self.settings.name
    }

    fn model(&self) -> &str {
        &self.settings.model
    }

    fn needs_workspace(&self) -> bool {
        true
    }

    async fn complete(&self, req: Request) -> Result<Response> {
        let argv: Vec<String> = self.spec.command.iter().map(|a| self.render(a, &req)).collect();
        let (program, args) = argv.split_first().expect("non-empty, checked in new()");

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Agentic CLIs otherwise try to open a pager or prompt for auth.
            .env("CI", "1")
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat");

        for (k, v) in &self.spec.env {
            cmd.env(k, v);
        }
        if let Some(ws) = req.workspace.as_ref() {
            cmd.current_dir(ws);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning `{program}` for provider `{}`", self.settings.name))?;

        if let Some(template) = &self.spec.stdin {
            let payload = self.render(template, &req);
            let mut stdin = child.stdin.take().context("stdin was not piped")?;
            stdin.write_all(payload.as_bytes()).await?;
            stdin.shutdown().await?;
        } else {
            drop(child.stdin.take());
        }

        let timeout = std::time::Duration::from_secs(self.settings.timeout_secs);
        let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(result) => result.context("waiting for CLI provider")?,
            Err(_) => bail!(
                "provider `{}` exceeded timeout of {}s on `{}`",
                self.settings.name,
                self.settings.timeout_secs,
                req.label
            ),
        };

        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            bail!(
                "provider `{}` exited {}: {}",
                self.settings.name,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim().chars().take(600).collect::<String>()
            );
        }

        let text = match &self.spec.extract {
            Extract::Raw => stdout.clone(),
            Extract::JsonPath(path) => {
                let v: serde_json::Value = extract_json(&stdout)
                    .with_context(|| format!("provider `{}` did not emit JSON", self.settings.name))?;
                dig(&v, path)
                    .and_then(|v| v.as_str().map(str::to_string))
                    .with_context(|| format!("no string at `{path}` in CLI output"))?
            }
            Extract::JsonLines(path) => stdout
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter_map(|v| dig(&v, path).and_then(|v| v.as_str().map(str::to_string)))
                .collect::<Vec<_>>()
                .join(""),
        };

        let mut usage = Usage::default();
        if let Some(path) = &self.spec.usage_path {
            if let Ok(v) = extract_json::<serde_json::Value>(&stdout) {
                if let Some(u) = dig(&v, path) {
                    usage.input_tokens = u["input_tokens"]
                        .as_u64()
                        .or_else(|| u["prompt_tokens"].as_u64())
                        .unwrap_or(0);
                    usage.output_tokens = u["output_tokens"]
                        .as_u64()
                        .or_else(|| u["completion_tokens"].as_u64())
                        .unwrap_or(0);
                }
                // Many CLIs report cost directly; prefer that over our estimate.
                if let Some(cost) = v["total_cost_usd"].as_f64().or_else(|| v["cost_usd"].as_f64()) {
                    usage.cost_usd = cost;
                }
            }
        }
        if usage.cost_usd == 0.0 {
            usage.cost_usd = self.settings.estimate_cost(usage.input_tokens, usage.output_tokens);
        }

        Ok(Response {
            text,
            usage,
            provider: self.settings.name.clone(),
            model: self.settings.model.clone(),
        })
    }
}

/// Follow a slash-separated path into a JSON value.
fn dig<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('/').filter(|s| !s.is_empty()).try_fold(v, |acc, key| match key.parse::<usize>() {
        Ok(i) => acc.get(i),
        Err(_) => acc.get(key),
    })
}

/// Ready-made specs for the agentic CLIs people actually have installed, so
/// `locust init` can produce a config that runs without hand-tuning argv.
pub fn preset(name: &str) -> Option<(CliSpec, &'static str)> {
    let spec = match name {
        "claude" => (
            CliSpec {
                // Read-only tools by default: the agent can open files the
                // context pack omitted and grep for call sites, but cannot
                // execute anything. Add `Bash` via the provider's `command` in
                // locust.toml to let it run the test suite and prove findings.
                command: [
                    "claude",
                    "-p",
                    "--output-format",
                    "json",
                    "--allowed-tools",
                    "Read,Grep,Glob",
                    "--append-system-prompt",
                    "{system}",
                ]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                stdin: Some("{prompt}".into()),
                extract: Extract::JsonPath("result".into()),
                usage_path: Some("usage".into()),
                env: HashMap::new(),
            },
            "claude-opus-5",
        ),
        "codex" => (
            CliSpec {
                command: ["codex", "exec", "--json", "--skip-git-repo-check", "-"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                stdin: Some("{system}\n\n{prompt}".into()),
                extract: Extract::JsonLines("msg/message".into()),
                usage_path: None,
                env: HashMap::new(),
            },
            "gpt-5",
        ),
        "gemini" => (
            CliSpec {
                command: ["gemini", "-m", "{model}", "-p", "{prompt}"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                stdin: None,
                extract: Extract::Raw,
                usage_path: None,
                env: HashMap::new(),
            },
            "gemini-2.5-pro",
        ),
        _ => return None,
    };
    Some(spec)
}

pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dig_walks_objects_and_arrays() {
        let v = serde_json::json!({ "msg": { "message": "hi" }, "xs": [1, 2, 3] });
        assert_eq!(dig(&v, "msg/message").unwrap(), "hi");
        assert_eq!(dig(&v, "xs/1").unwrap(), 2);
        assert!(dig(&v, "msg/nope").is_none());
    }
}
