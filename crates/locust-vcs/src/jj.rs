//! Jujutsu backend.
//!
//! `jj` is the better substrate for this tool: anonymous heads mean a
//! generation of speculative workspaces costs nothing to create or throw away,
//! and the operation log lets you rewind a whole generation. It is optional,
//! so the backend degrades to git for anything jj does not expose directly.

use crate::{run, ChangedFile, Vcs, Workspace};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct JjVcs {
    root: PathBuf,
    workspace_root: PathBuf,
    /// jj colocates with git; reuse the git backend for diff plumbing that jj
    /// expresses differently but identically.
    git: crate::git::GitVcs,
    add_lock: Mutex<()>,
}

impl JjVcs {
    /// True when `jj` is installed *and* `root` is inside a jj workspace.
    pub fn available(root: &Path) -> bool {
        run(root, "jj", &["--ignore-working-copy", "root"]).is_ok()
    }

    pub fn open(root: &Path) -> Result<Self> {
        let top = run(root, "jj", &["--ignore-working-copy", "root"])
            .context("not a jj workspace")?
            .trim()
            .to_string();
        let root = PathBuf::from(top);
        let git = crate::git::GitVcs::open(&root)
            .context("locust's jj backend requires a git-colocated repository")?;
        let workspace_root = root.join(".locust").join("wt");
        Ok(Self { root, workspace_root, git, add_lock: Mutex::new(()) })
    }

    fn jj(&self, args: &[&str]) -> Result<String> {
        let mut full = vec!["--ignore-working-copy", "--color=never"];
        full.extend_from_slice(args);
        run(&self.root, "jj", &full)
    }
}

impl Vcs for JjVcs {
    fn kind(&self) -> &'static str {
        "jj"
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, rev: &str) -> Result<String> {
        // Ask jj for the backing git commit so both backends speak the same
        // identifiers and the git plumbing below stays valid.
        match self.jj(&["log", "--no-graph", "-r", rev, "-T", "commit_id"]) {
            Ok(id) if !id.trim().is_empty() => Ok(id.trim().to_string()),
            _ => self.git.resolve(rev),
        }
    }

    fn merge_base(&self, base: &str, target: &str) -> Result<String> {
        self.git.merge_base(base, target)
    }

    fn diff(&self, base: &str, target: &str) -> Result<String> {
        self.git.diff(base, target)
    }

    fn diff_paths(&self, base: &str, target: &str, paths: &[String]) -> Result<String> {
        self.git.diff_paths(base, target, paths)
    }

    fn changed_files(&self, base: &str, target: &str) -> Result<Vec<ChangedFile>> {
        self.git.changed_files(base, target)
    }

    fn show_file(&self, rev: &str, path: &str) -> Result<Option<String>> {
        self.git.show_file(rev, path)
    }

    fn create_workspace(&self, id: &str, rev: &str) -> Result<Workspace> {
        if id.contains(['/', '\\']) || id.contains("..") {
            bail!("invalid workspace id `{id}`");
        }
        let path = self.workspace_root.join(id);
        std::fs::create_dir_all(&self.workspace_root)?;

        let _guard = self.add_lock.lock().unwrap_or_else(|e| e.into_inner());
        if path.exists() {
            let _ = self.jj(&["workspace", "forget", id]);
            let _ = std::fs::remove_dir_all(&path);
        }
        self.jj(&["workspace", "add", "--name", id, "--revision", rev, &path.to_string_lossy()])
            .with_context(|| format!("creating jj workspace `{id}` at {rev}"))?;

        Ok(Workspace { id: id.to_string(), path, rev: rev.to_string() })
    }

    fn destroy_workspace(&self, ws: &Workspace) -> Result<()> {
        let _ = self.jj(&["workspace", "forget", &ws.id]);
        let _ = std::fs::remove_dir_all(&ws.path);
        Ok(())
    }

    fn prune_all(&self) -> Result<()> {
        if self.workspace_root.exists() {
            for entry in std::fs::read_dir(&self.workspace_root)?.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    let _ = self.jj(&["workspace", "forget", name]);
                }
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        Ok(())
    }
}
