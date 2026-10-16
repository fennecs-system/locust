//! Version-control isolation for competing agents.
//!
//! Every agent that needs to touch the filesystem gets its own checkout of the
//! review base so it can read, grep and run tests without colliding with its
//! rivals. The backend is abstracted because `jj`'s operation log is a better
//! fit for spawning and discarding hundreds of speculative workspaces, but not
//! everyone has it installed.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub mod git;
pub mod jj;

pub use git::GitVcs;
pub use jj::JjVcs;

/// A file touched by the diff under review.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: ChangeStatus,
    pub added: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Other,
}

impl ChangeStatus {
    fn from_code(code: &str) -> Self {
        match code.chars().next() {
            Some('A') => Self::Added,
            Some('M') => Self::Modified,
            Some('D') => Self::Deleted,
            Some('R') => Self::Renamed,
            _ => Self::Other,
        }
    }

    pub fn is_deleted(self) -> bool {
        matches!(self, Self::Deleted)
    }
}

/// A private checkout handed to a single agent.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// Stable id, e.g. `gen0-atk-anthropic-2`. Used for the directory name.
    pub id: String,
    pub path: PathBuf,
    /// Revision the workspace was created at.
    pub rev: String,
}

/// The revision pair under review.
#[derive(Debug, Clone)]
pub struct ReviewRange {
    pub base: String,
    pub target: String,
    /// The merge-base of `base` and `target`; what agents are actually checked
    /// out at so they see the pre-change world plus the diff.
    pub merge_base: String,
}

/// Read access to the repository under review, plus disposable checkouts for
/// agents that need to explore it.
///
/// Implementations must tolerate concurrent calls — workspace creation in
/// particular, which the arena drives from many tasks at once and which is not
/// safe to run in parallel against a single git index.
pub trait Vcs: Send + Sync {
    /// Backend name, for reports: `git` or `jj`.
    fn kind(&self) -> &'static str;

    /// Absolute path of the main repository.
    fn root(&self) -> &Path;

    /// Turn a user-supplied revision ("main", "HEAD~3", a change id) into a
    /// stable commit identifier.
    fn resolve(&self, rev: &str) -> Result<String>;

    fn merge_base(&self, base: &str, target: &str) -> Result<String>;

    /// Unified diff between two revisions.
    fn diff(&self, base: &str, target: &str) -> Result<String>;

    fn changed_files(&self, base: &str, target: &str) -> Result<Vec<ChangedFile>>;

    /// Contents of `path` at `rev`, or `None` if it does not exist there.
    fn show_file(&self, rev: &str, path: &str) -> Result<Option<String>>;

    /// Create an isolated checkout at `rev`. Implementations must be safe to
    /// call concurrently.
    fn create_workspace(&self, id: &str, rev: &str) -> Result<Workspace>;

    fn destroy_workspace(&self, ws: &Workspace) -> Result<()>;

    /// Drop every workspace this backend owns, including orphans from a run
    /// that was killed.
    fn prune_all(&self) -> Result<()>;

    fn range(&self, base: &str, target: &str) -> Result<ReviewRange> {
        let base = self.resolve(base)?;
        let target = self.resolve(target)?;
        let merge_base = self.merge_base(&base, &target).unwrap_or_else(|_| base.clone());
        Ok(ReviewRange { base, target, merge_base })
    }
}

/// Run a command, returning stdout on success and a useful error otherwise.
pub(crate) fn run(cwd: &Path, program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(cwd)
        // Keep agent checkouts deterministic and free of user hooks/pagers.
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("failed to spawn `{program}`"))?;

    if !out.status.success() {
        bail!(
            "`{program} {}` failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn detect(root: &Path, prefer: Option<&str>) -> Result<Box<dyn Vcs>> {
    let root = root
        .canonicalize()
        .with_context(|| format!("no such directory: {}", root.display()))?;

    match prefer {
        Some("jj") => Ok(Box::new(JjVcs::open(&root)?)),
        Some("git") => Ok(Box::new(GitVcs::open(&root)?)),
        Some(other) => bail!("unknown vcs backend `{other}` (expected `git` or `jj`)"),
        None => {
            if JjVcs::available(&root) {
                Ok(Box::new(JjVcs::open(&root)?))
            } else {
                Ok(Box::new(GitVcs::open(&root)?))
            }
        }
    }
}
