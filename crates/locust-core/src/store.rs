//! On-disk run artifacts.
//!
//! Every model call is written out verbatim. In a system where the output is
//! an argument between agents, "why did it say that" is the question you will
//! actually need to answer, and a scorecard without the transcript behind it is
//! not auditable.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub struct Store {
    pub run_dir: PathBuf,
    pub run_id: String,
}

impl Store {
    pub fn create(repo_root: &Path, run_id: &str) -> Result<Self> {
        let run_dir = repo_root.join(".locust").join("runs").join(run_id);
        std::fs::create_dir_all(run_dir.join("transcripts"))
            .with_context(|| format!("creating {}", run_dir.display()))?;
        // Agent worktrees and run logs are tooling output, not source.
        let ignore = repo_root.join(".locust").join(".gitignore");
        if !ignore.exists() {
            let _ = std::fs::write(&ignore, "*\n");
        }
        Ok(Self { run_dir, run_id: run_id.to_string() })
    }

    pub fn write_json<T: Serialize>(&self, rel: &str, value: &T) -> Result<()> {
        let path = self.run_dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(value)?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn write_text(&self, rel: &str, text: &str) -> Result<()> {
        let path = self.run_dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Record one model call. Failures here are logged, never propagated: a
    /// full disk should not abort a review that is otherwise working.
    pub fn transcript(&self, label: &str, system: &str, user: &str, output: &str, meta: &str) {
        let name = format!("{}.md", crate::config::slug(label));
        let body = format!(
            "# {label}\n\n{meta}\n\n## System\n\n````\n{system}\n````\n\n## User\n\n````\n{user}\n````\n\n## Output\n\n````\n{output}\n````\n"
        );
        if let Err(e) = self.write_text(&format!("transcripts/{name}"), &body) {
            tracing::warn!("could not write transcript {label}: {e}");
        }
    }

    pub fn generation_dir(&self, generation: u32) -> String {
        format!("gen-{generation}")
    }
}

/// `2026-10-16-143052-a1b2`: sorts chronologically, unique under concurrency.
pub fn new_run_id() -> String {
    let now = chrono::Utc::now().format("%Y-%m-%d-%H%M%S");
    let suffix = uuid::Uuid::new_v4().to_string();
    format!("{now}-{}", &suffix[..4])
}

/// Most recent run under `.locust/runs`, for `locust report` with no argument.
pub fn latest_run(repo_root: &Path) -> Option<PathBuf> {
    let runs = repo_root.join(".locust").join("runs");
    let mut entries: Vec<PathBuf> =
        std::fs::read_dir(runs).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    entries.sort();
    entries.pop()
}
