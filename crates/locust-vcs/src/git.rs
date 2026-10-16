//! git-worktree backend. Works everywhere, no extra installs.

use crate::{run, ChangeStatus, ChangedFile, Vcs, Workspace};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct GitVcs {
    root: PathBuf,
    workspace_root: PathBuf,
    /// `git worktree add` takes the repository index lock, so concurrent adds
    /// race. Agents are spawned in parallel, so serialise just this step.
    add_lock: Mutex<()>,
}

impl GitVcs {
    pub fn open(root: &Path) -> Result<Self> {
        let top = run(root, "git", &["rev-parse", "--show-toplevel"])
            .context("not a git repository")?
            .trim()
            .to_string();
        let root = PathBuf::from(top);
        let workspace_root = root.join(".locust").join("wt");
        Ok(Self { root, workspace_root, add_lock: Mutex::new(()) })
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        run(&self.root, "git", args)
    }
}

impl Vcs for GitVcs {
    fn kind(&self) -> &'static str {
        "git"
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, rev: &str) -> Result<String> {
        Ok(self
            .git(&["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
            .with_context(|| format!("cannot resolve revision `{rev}`"))?
            .trim()
            .to_string())
    }

    fn merge_base(&self, base: &str, target: &str) -> Result<String> {
        Ok(self.git(&["merge-base", base, target])?.trim().to_string())
    }

    fn diff(&self, base: &str, target: &str) -> Result<String> {
        // Three-dot: what `target` introduced relative to their common
        // ancestor, not every change that landed on `base` meanwhile.
        self.git(&[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-M",
            "--unified=6",
            &format!("{base}...{target}"),
        ])
    }

    fn changed_files(&self, base: &str, target: &str) -> Result<Vec<ChangedFile>> {
        let names = self.git(&["diff", "--name-status", "-M", &format!("{base}...{target}")])?;
        let stats = self.git(&["diff", "--numstat", "-M", &format!("{base}...{target}")])?;

        let mut files = Vec::new();
        for (name_line, stat_line) in names.lines().zip(stats.lines()) {
            let mut nf = name_line.split('\t');
            let (Some(code), Some(first)) = (nf.next(), nf.next()) else { continue };
            // For renames the second tab-separated field is the old path and
            // the third is the new one; numstat agrees on the ordering.
            let path = nf.next().unwrap_or(first).to_string();

            let mut sf = stat_line.split('\t');
            let added = sf.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let removed = sf.next().and_then(|v| v.parse().ok()).unwrap_or(0);

            files.push(ChangedFile { path, status: ChangeStatus::from_code(code), added, removed });
        }
        Ok(files)
    }

    fn show_file(&self, rev: &str, path: &str) -> Result<Option<String>> {
        match self.git(&["show", &format!("{rev}:{path}")]) {
            Ok(contents) => Ok(Some(contents)),
            // Absent at this revision (added or deleted by the diff).
            Err(_) => Ok(None),
        }
    }

    fn create_workspace(&self, id: &str, rev: &str) -> Result<Workspace> {
        if id.contains(['/', '\\']) || id.contains("..") {
            bail!("invalid workspace id `{id}`");
        }
        let path = self.workspace_root.join(id);
        std::fs::create_dir_all(&self.workspace_root)?;

        let _guard = self.add_lock.lock().unwrap_or_else(|e| e.into_inner());
        if path.exists() {
            // Leftover from an interrupted run; reclaim it.
            let _ = self.git(&["worktree", "remove", "--force", &path.to_string_lossy()]);
            let _ = std::fs::remove_dir_all(&path);
        }
        self.git(&["worktree", "add", "--detach", "--quiet", &path.to_string_lossy(), rev])
            .with_context(|| format!("creating worktree `{id}` at {rev}"))?;

        Ok(Workspace { id: id.to_string(), path, rev: rev.to_string() })
    }

    fn destroy_workspace(&self, ws: &Workspace) -> Result<()> {
        let path = ws.path.to_string_lossy().into_owned();
        if let Err(e) = self.git(&["worktree", "remove", "--force", &path]) {
            tracing::debug!("worktree remove failed, unlinking directly: {e}");
            let _ = std::fs::remove_dir_all(&ws.path);
            let _ = self.git(&["worktree", "prune"]);
        }
        Ok(())
    }

    fn prune_all(&self) -> Result<()> {
        if self.workspace_root.exists() {
            for entry in std::fs::read_dir(&self.workspace_root)?.flatten() {
                let p = entry.path().to_string_lossy().into_owned();
                let _ = self.git(&["worktree", "remove", "--force", &p]);
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        let _ = self.git(&["worktree", "prune"]);
        Ok(())
    }
}
