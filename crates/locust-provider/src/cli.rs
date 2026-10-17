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
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    /// stdout is JSONL and carries more than one kind of record. Concatenate
    /// `path` only from lines where `key` equals `equals`.
    ///
    /// Needed by any CLI that streams a transcript rather than a reply: codex
    /// emits one `item.completed` per tool call, per reasoning step and per
    /// message, so taking every item's text would splice the agent's working
    /// notes into its answer.
    JsonLinesWhere { path: String, key: String, equals: String },
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
    /// Prefix argv that the agent command is appended to, for running it under
    /// a sandbox. Empty means run the command directly.
    pub wrapper: Vec<String>,
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

    /// Placeholders available to the sandbox wrapper. Deliberately narrower
    /// than the agent command's: a sandbox argv is a container or jail
    /// invocation, and interpolating a model-authored prompt into it would be
    /// an argument-injection surface for no benefit.
    fn render_wrapper(&self, template: &str, req: &Request) -> String {
        let workspace = req
            .workspace
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        template
            .replace("{workspace}", &workspace)
            .replace("{id}", &req.label)
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
        // The sandbox wrapper, if configured, is argv[0..n] and the agent
        // command is appended to it: `docker run ... -- claude -p ...`.
        // locust never builds a sandbox itself; it only arranges to be run
        // inside one somebody else configured.
        let mut argv: Vec<String> =
            self.spec.wrapper.iter().map(|a| self.render_wrapper(a, &req)).collect();
        argv.extend(self.spec.command.iter().map(|a| self.render(a, &req)));
        let (program, args) = argv.split_first().expect("non-empty, checked in new()");

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Last-resort reaping: if this function returns early for any
            // reason the runtime still kills the child rather than leaking it.
            .kill_on_drop(true)
            // Agentic CLIs otherwise try to open a pager or prompt for auth.
            .env("CI", "1")
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat");

        // Give the child its own process group. Agentic CLIs spawn their own
        // subprocesses, and signalling only the direct child on a timeout
        // leaves those orphaned and still running against the API.
        #[cfg(unix)]
        cmd.process_group(0);

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

        // Drain both pipes concurrently with waiting. A child that fills a
        // pipe buffer blocks on write forever if nothing is reading, so
        // waiting first and reading afterwards can hang until the timeout on
        // output that was already there.
        let mut stdout_pipe = child.stdout.take().context("stdout was not piped")?;
        let mut stderr_pipe = child.stderr.take().context("stderr was not piped")?;
        let out_reader = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stdout_pipe.read_to_end(&mut buf).await;
            buf
        });
        let err_reader = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut buf).await;
            buf
        });

        let timeout = std::time::Duration::from_secs(self.settings.timeout_secs);
        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(status) => status.context("waiting for CLI provider")?,
            Err(_) => {
                terminate(&mut child).await;
                out_reader.abort();
                err_reader.abort();
                bail!(
                    "provider `{}` exceeded timeout of {}s on `{}` (process group terminated)",
                    self.settings.name,
                    self.settings.timeout_secs,
                    req.label
                );
            }
        };

        let out = std::process::Output {
            status,
            stdout: out_reader.await.unwrap_or_default(),
            stderr: err_reader.await.unwrap_or_default(),
        };

        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            bail!(
                "provider `{}` exited {}: {}",
                self.settings.name,
                out.status,
                failure_detail(&stdout, &String::from_utf8_lossy(&out.stderr))
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
            Extract::JsonLines(path) => jsonl(&stdout)
                .filter_map(|v| dig(&v, path).and_then(|v| v.as_str().map(str::to_string)))
                .collect::<Vec<_>>()
                .join(""),
            // The *last* matching record, not all of them joined. An agent
            // that narrates progress before answering would otherwise have its
            // commentary spliced ahead of the report, and whichever JSON came
            // first would be read as the answer.
            Extract::JsonLinesWhere { path, key, equals } => jsonl(&stdout)
                .filter(|v| dig(v, key).and_then(|k| k.as_str()) == Some(equals.as_str()))
                .filter_map(|v| dig(&v, path).and_then(|v| v.as_str().map(str::to_string)))
                .last()
                .unwrap_or_default(),
        };

        // A tool can exit 0 having produced no reply at all -- codex ends a
        // failed turn with an `error` item and no `agent_message`. Returning
        // an empty string here makes that look like a reviewer that read the
        // change and found nothing, which is the most expensive way to fail.
        if text.trim().is_empty() {
            bail!(
                "provider `{}` exited successfully but produced no reply on `{}`: {}",
                self.settings.name,
                req.label,
                stream_errors(&stdout)
                    .unwrap_or_else(|| failure_detail(&stdout, &String::from_utf8_lossy(&out.stderr)))
            );
        }

        let mut usage = Usage::default();
        if let Some(path) = &self.spec.usage_path {
            // A JSONL transcript carries its totals on one of its lines, not in
            // a single document, so look there first and fall back to parsing
            // stdout whole. Taking the last match means a stream that reports
            // running totals ends on the final figure.
            let found = jsonl(&stdout)
                .filter_map(|v| dig(&v, path).cloned())
                .last()
                .or_else(|| {
                    extract_json::<serde_json::Value>(&stdout)
                        .ok()
                        .and_then(|v| dig(&v, path).cloned())
                });

            if let Some(u) = found {
                usage.input_tokens =
                    u["input_tokens"].as_u64().or_else(|| u["prompt_tokens"].as_u64()).unwrap_or(0);
                usage.output_tokens = u["output_tokens"]
                    .as_u64()
                    .or_else(|| u["completion_tokens"].as_u64())
                    .unwrap_or(0);
            }
            // Many CLIs report cost directly; prefer that over our estimate.
            if let Ok(v) = extract_json::<serde_json::Value>(&stdout) {
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

/// Error text a JSONL stream reported about itself, if any.
///
/// A CLI that fails mid-turn says so in its own event stream rather than on
/// stderr, and that message is the only thing that explains an empty reply.
fn stream_errors(stdout: &str) -> Option<String> {
    let mut messages: Vec<String> = Vec::new();
    for value in jsonl(stdout) {
        for key in ["item", "msg", "error"] {
            let Some(node) = dig(&value, key) else { continue };
            let is_error = node.get("type").and_then(|t| t.as_str()) == Some("error")
                || value.get("type").and_then(|t| t.as_str()).is_some_and(|t| t.contains("error"))
                || value.get("type").and_then(|t| t.as_str()) == Some("turn.failed");
            if !is_error {
                continue;
            }
            if let Some(text) =
                node.get("message").or_else(|| node.get("text")).and_then(|m| m.as_str())
            {
                messages.push(text.to_string());
            }
        }
    }
    if messages.is_empty() {
        None
    } else {
        Some(messages.join("; ").chars().take(600).collect())
    }
}

/// Stop a timed-out child and everything it spawned, then reap it.
///
/// SIGTERM to the whole process group first so a CLI can flush and exit
/// cleanly, then SIGKILL to whatever ignored it. The final `wait` is what
/// actually reaps: without it the child lingers as a zombie for the lifetime
/// of the run, and a long run can accumulate one per timed-out agent.
async fn terminate(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let group = -(pid as i32);
        // SAFETY: `group` is the negated pid of a child we spawned into its
        // own process group, so this signals that group and nothing else.
        unsafe {
            libc::kill(group, libc::SIGTERM);
        }
        // Short grace period; a hung agent is not going to use much of it.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        unsafe {
            libc::kill(group, libc::SIGKILL);
        }
    }

    // Also kills on platforms without process groups, and reaps everywhere.
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Best available explanation for a non-zero exit.
///
/// Agentic CLIs routinely report their own failures on **stdout** — often as
/// the same JSON envelope they use for success, with an error flag — and leave
/// stderr empty. Reading only stderr yields `exited exit status: 1: ` with
/// nothing after the colon, which is how a whole run can fail with no way to
/// tell why.
fn failure_detail(stdout: &str, stderr: &str) -> String {
    let clip = |s: &str| s.trim().chars().take(600).collect::<String>();

    if let Ok(v) = extract_json::<serde_json::Value>(stdout) {
        for key in ["error", "message", "result"] {
            if let Some(text) = v.get(key).and_then(|x| x.as_str()) {
                if !text.trim().is_empty() {
                    return clip(text);
                }
            }
        }
    }
    // Prefer stderr when it says anything, then fall back to raw stdout.
    match (clip(stderr), clip(stdout)) {
        (e, _) if !e.is_empty() => e,
        (_, o) if !o.is_empty() => o,
        _ => "no output on stdout or stderr".into(),
    }
}

/// Parse stdout as JSONL, skipping anything that is not a JSON object. CLIs
/// interleave plain-text notices with their event stream often enough that a
/// strict parse would throw the whole transcript away.
fn jsonl(stdout: &str) -> impl Iterator<Item = serde_json::Value> + '_ {
    stdout.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
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
                wrapper: Vec::new(),
            },
            "claude-opus-5",
        ),
        "codex" => (
            CliSpec {
                // `codex exec` has no system-prompt flag, so the two are
                // concatenated on stdin. `--sandbox read-only` is codex's own
                // isolation: a reviewer needs to read and nothing else.
                command: [
                    "codex",
                    "exec",
                    "--json",
                    "--sandbox",
                    "read-only",
                    "--skip-git-repo-check",
                    "-m",
                    "{model}",
                    "-",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect(),
                stdin: Some("{system}\n\n{prompt}".into()),
                // codex streams a transcript: one `item.completed` per tool
                // call, reasoning step and message. Only the messages are the
                // reply.
                extract: Extract::JsonLinesWhere {
                    path: "item/text".into(),
                    key: "item/type".into(),
                    equals: "agent_message".into(),
                },
                usage_path: Some("usage".into()),
                env: HashMap::new(),
                wrapper: Vec::new(),
            },
            "gpt-6.1-sol",
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
                wrapper: Vec::new(),
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

    use crate::ProviderSettings;

    fn spec(command: &[&str]) -> CliSpec {
        CliSpec {
            command: command.iter().map(|s| s.to_string()).collect(),
            stdin: None,
            extract: Extract::Raw,
            usage_path: None,
            env: HashMap::new(),
            wrapper: Vec::new(),
        }
    }

    fn settings(timeout_secs: u64) -> ProviderSettings {
        ProviderSettings { name: "test".into(), timeout_secs, ..ProviderSettings::default() }
    }

    /// A timed-out agent used to be dropped, not killed: `wait_with_output`
    /// consumes the child, so the future was discarded and the process carried
    /// on holding its worktree and spending API quota. Agentic CLIs also spawn
    /// their own subprocesses, so the whole group has to go.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_timeout_kills_the_child_and_everything_it_spawned() {
        let marker = std::env::temp_dir().join(format!("locust-reap-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);

        // Parent sleeps; grandchild outlives it unless the group is signalled,
        // and writes the marker to prove it was still alive.
        let script = format!(
            "( sleep 5; touch {} ) & sleep 30",
            marker.display()
        );
        let provider =
            CliProvider::new(settings(1), spec(&["sh", "-c", &script])).unwrap();

        let started = std::time::Instant::now();
        let err = provider
            .complete(Request::new("t", "sys", "prompt"))
            .await
            .expect_err("must time out");
        assert!(err.to_string().contains("exceeded timeout"), "got: {err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(4), "returned promptly");

        // Past when the grandchild would have fired had it survived.
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        assert!(
            !marker.exists(),
            "grandchild outlived the timeout: only the direct child was signalled"
        );
    }

    /// Output already written must not be lost, and a child that writes more
    /// than a pipe buffer must not deadlock waiting for a reader.
    #[tokio::test]
    async fn a_child_writing_more_than_a_pipe_buffer_still_completes() {
        // 1 MiB, far beyond the usual 64 KiB pipe capacity.
        let script = "yes 0123456789abcdef | head -c 1048576";
        let provider =
            CliProvider::new(settings(30), spec(&["sh", "-c", script])).unwrap();

        let resp = provider
            .complete(Request::new("t", "sys", "prompt"))
            .await
            .expect("must not deadlock");
        assert_eq!(resp.text.len(), 1_048_576);
    }

    /// The sandbox wrapper is a prefix argv the agent command is appended to.
    /// `env -i` stands in for docker/bwrap/systemd-run here: it is a real
    /// command that takes a command as its tail, so the composition is tested
    /// rather than mocked.
    #[tokio::test]
    async fn the_agent_command_runs_inside_the_sandbox_wrapper() {
        let mut spec = spec(&["sh", "-c", "echo $LOCUST_SANDBOX_MARK"]);
        spec.wrapper = ["env", "LOCUST_SANDBOX_MARK=inside"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let provider = CliProvider::new(settings(30), spec).unwrap();
        let resp = provider.complete(Request::new("t", "s", "p")).await.unwrap();
        assert_eq!(resp.text.trim(), "inside", "the agent did not run under the wrapper");
    }

    #[tokio::test]
    async fn the_wrapper_receives_the_workspace_path() {
        let mut spec = spec(&["sh", "-c", "pwd"]);
        // Stands in for `docker run -w {workspace}`.
        spec.wrapper = ["sh", "-c", "cd {workspace} && exec \"$@\"", "--"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let dir = std::env::temp_dir().canonicalize().unwrap();
        let provider = CliProvider::new(settings(30), spec).unwrap();
        let resp = provider
            .complete(Request::new("t", "s", "p").in_workspace(Some(dir.clone())))
            .await
            .unwrap();
        assert_eq!(resp.text.trim(), dir.display().to_string());
    }

    /// A model-authored prompt must never reach the sandbox argv, or a finding
    /// could rewrite the container invocation that is supposed to contain it.
    #[tokio::test]
    async fn prompt_text_is_not_interpolated_into_the_wrapper() {
        let mut spec = spec(&["sh", "-c", "echo done"]);
        spec.wrapper =
            ["env", "WRAP={prompt}{system}"].iter().map(|s| s.to_string()).collect();

        let provider = CliProvider::new(settings(30), spec).unwrap();
        let resp = provider
            .complete(Request::new("t", "SYSTEM-TEXT", "PROMPT-TEXT"))
            .await
            .unwrap();
        assert_eq!(resp.text.trim(), "done");
        // The placeholders are left alone rather than substituted, so there is
        // nothing a prompt can do to the wrapper.
        let rendered = provider.render_wrapper("{prompt}|{system}", &Request::new("t", "S", "P"));
        assert_eq!(rendered, "{prompt}|{system}");
    }

    #[tokio::test]
    async fn no_wrapper_means_the_command_runs_directly() {
        let provider = CliProvider::new(settings(30), spec(&["echo", "plain"])).unwrap();
        let resp = provider.complete(Request::new("t", "s", "p")).await.unwrap();
        assert_eq!(resp.text.trim(), "plain");
    }

    #[tokio::test]
    async fn a_nonzero_exit_surfaces_what_the_tool_printed() {
        let provider = CliProvider::new(
            settings(30),
            spec(&["sh", "-c", "echo 'quota exhausted' ; exit 1"]),
        )
        .unwrap();
        let err = provider.complete(Request::new("t", "s", "p")).await.unwrap_err();
        assert!(err.to_string().contains("quota exhausted"), "got: {err}");
    }

    #[test]
    fn a_failure_reported_only_on_stdout_is_still_explained() {
        // Observed live: four agents failed and locust printed
        // "exited exit status: 1: " with nothing after the colon, because the
        // CLI wrote its reason to stdout.
        let stdout = r#"{"is_error":true,"result":"Usage limit reached","type":"result"}"#;
        assert_eq!(failure_detail(stdout, ""), "Usage limit reached");
    }

    #[test]
    fn stderr_wins_when_it_has_something_to_say() {
        assert_eq!(failure_detail("", "command not found"), "command not found");
        assert_eq!(failure_detail("raw text", ""), "raw text");
    }

    #[test]
    fn total_silence_is_said_out_loud_rather_than_left_blank() {
        assert_eq!(failure_detail("", ""), "no output on stdout or stderr");
        assert!(!failure_detail("  \n ", " ").is_empty());
    }

    /// Shaped from a real `codex exec --json` transcript. The preset originally
    /// guessed `msg/message`, which matches nothing in this stream.
    const CODEX_STREAM: &str = concat!(
        r#"{"type":"thread.started","thread_id":"01a1"}"#,
        "\n",
        r#"{"type":"turn.started"}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"working notes that must not leak into the reply"}}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","text":"grep -rn foo ."}}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"{\"findings\":[]}"}}"#,
        "\n",
        r#"{"type":"turn.completed","usage":{"input_tokens":14176,"cached_input_tokens":12288,"output_tokens":5}}"#,
        "\n",
    );

    fn codex_spec() -> CliSpec {
        let (spec, _) = preset("codex").expect("codex preset");
        spec
    }

    #[test]
    fn the_codex_preset_takes_only_the_agent_message() {
        let Extract::JsonLinesWhere { path, key, equals } = &codex_spec().extract else {
            panic!("codex must filter the transcript, not concatenate it");
        };
        let text: String = jsonl(CODEX_STREAM)
            .filter(|v| dig(v, key).and_then(|k| k.as_str()) == Some(equals.as_str()))
            .filter_map(|v| dig(&v, path).and_then(|v| v.as_str().map(str::to_string)))
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(text, r#"{"findings":[]}"#);
        assert!(!text.contains("working notes"), "reasoning leaked into the reply");
        assert!(!text.contains("grep"), "a tool call leaked into the reply");
    }

    /// Raised by codex reviewing the adapter written for it: joining every
    /// agent message let a progress note ahead of the report be parsed as the
    /// report. Only the final message is the answer.
    #[test]
    fn only_the_final_agent_message_is_the_reply() {
        let stream = concat!(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"{\"status\":\"scanning\"}"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"{\"findings\":[1]}"}}"#,
            "\n",
        );
        let Extract::JsonLinesWhere { path, key, equals } = &codex_spec().extract else {
            panic!("codex must filter the transcript");
        };
        let text = jsonl(stream)
            .filter(|v| dig(v, key).and_then(|k| k.as_str()) == Some(equals.as_str()))
            .filter_map(|v| dig(&v, path).and_then(|v| v.as_str().map(str::to_string)))
            .last()
            .unwrap_or_default();
        assert_eq!(text, r#"{"findings":[1]}"#);
        assert!(!text.contains("scanning"), "a progress note was spliced in ahead of the report");
    }

    /// Raised by claude reviewing the same adapter: a turn that fails after
    /// exiting 0 produces no agent message, and an empty reply reads exactly
    /// like a reviewer that found nothing.
    #[tokio::test]
    async fn an_empty_reply_is_a_failure_that_explains_itself() {
        let mut spec = codex_spec();
        let stream = r#"{"type":"item.completed","item":{"type":"error","message":"stream disconnected before completion"}}"#;
        spec.command = vec!["sh".into(), "-c".into(), format!("echo '{stream}'")];

        let provider = CliProvider::new(settings(30), spec).unwrap();
        let err = provider.complete(Request::new("t", "s", "p")).await.unwrap_err().to_string();
        assert!(err.contains("produced no reply"), "got: {err}");
        assert!(err.contains("stream disconnected"), "the stream's own error must surface: {err}");
    }

    #[test]
    fn usage_is_read_from_the_line_that_carries_it() {
        let usage: Vec<_> = jsonl(CODEX_STREAM).filter_map(|v| dig(&v, "usage").cloned()).collect();
        let last = usage.last().expect("a usage record");
        assert_eq!(last["input_tokens"].as_u64(), Some(14176));
        assert_eq!(last["output_tokens"].as_u64(), Some(5));
    }

    #[test]
    fn a_plain_text_notice_amid_the_stream_does_not_discard_it() {
        let polluted = format!("warning: using fallback metadata\n{CODEX_STREAM}");
        assert_eq!(jsonl(&polluted).count(), 6, "the JSON lines still parse");
    }

    #[test]
    fn the_codex_preset_pins_a_model_and_its_own_sandbox() {
        let spec = codex_spec();
        let argv = spec.command.join(" ");
        // `gpt-5` was not a model this codex knows; passing one it does keeps
        // runs reproducible and fails loudly rather than silently drifting.
        assert!(argv.contains("-m {model}"), "model must be explicit: {argv}");
        assert!(argv.contains("--sandbox read-only"), "a reviewer only reads: {argv}");
        // No system-prompt flag exists, so both halves go in on stdin.
        assert_eq!(spec.stdin.as_deref(), Some("{system}\n\n{prompt}"));
    }

    #[test]
    fn dig_walks_objects_and_arrays() {
        let v = serde_json::json!({ "msg": { "message": "hi" }, "xs": [1, 2, 3] });
        assert_eq!(dig(&v, "msg/message").unwrap(), "hi");
        assert_eq!(dig(&v, "xs/1").unwrap(), 2);
        assert!(dig(&v, "msg/nope").is_none());
    }
}
