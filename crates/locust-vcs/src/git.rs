//! git-worktree backend. Works everywhere, no extra installs.

use crate::{run, ChangeStatus, ChangedFile, Vcs, Workspace};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
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

/// NUL-separated records, dropping the trailing empty element git leaves
/// after the final separator.
fn nul_records(raw: &str) -> impl Iterator<Item = &str> {
    raw.split('\0').filter(|r| !r.is_empty())
}

/// `--numstat -z` records are `added\tremoved\tpath`, except for a rename,
/// where the path field is empty and the old and new paths follow as two
/// further records. Binary files report `-` for both counts.
fn parse_numstat(raw: &str) -> HashMap<String, (usize, usize)> {
    let mut out = HashMap::new();
    let mut records = nul_records(raw);

    while let Some(record) = records.next() {
        // splitn(3) so a path containing a tab survives intact.
        let mut fields = record.splitn(3, '\t');
        let added = fields.next().unwrap_or("");
        let removed = fields.next().unwrap_or("");
        let path = fields.next().unwrap_or("");

        // `-` means binary; count it as zero changed lines rather than failing.
        let count = |v: &str| v.parse::<usize>().unwrap_or(0);

        if path.is_empty() {
            // Rename: the real paths are the next two records. Key on the new
            // one, which is what the rest of the pipeline refers to.
            let _old = records.next();
            if let Some(new) = records.next() {
                out.insert(new.to_string(), (count(added), count(removed)));
            }
        } else {
            out.insert(path.to_string(), (count(added), count(removed)));
        }
    }
    out
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

    fn diff_paths(&self, base: &str, target: &str, paths: &[String]) -> Result<String> {
        if paths.is_empty() {
            return Ok(String::new());
        }
        let range = format!("{base}...{target}");
        let mut out = String::new();

        // Chunked so a change touching thousands of files cannot overflow the
        // argument list. Each invocation emits complete, self-contained file
        // sections, so concatenating the chunks yields the same diff.
        for chunk in paths.chunks(300) {
            let mut args: Vec<String> =
                ["diff", "--no-color", "--no-ext-diff", "-M", "--unified=6", &range, "--"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();

            // `:(literal)` stops git reading a path that contains `*`, `?` or a
            // leading `:` as a glob or as pathspec magic.
            args.extend(chunk.iter().map(|p| format!(":(literal){p}")));

            let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
            out.push_str(&run(&self.root, "git", &borrowed)?);
        }
        Ok(out)
    }

    fn changed_files(&self, base: &str, target: &str) -> Result<Vec<ChangedFile>> {
        // `-z` throughout. Without it git quotes any path containing a space, a
        // tab or a non-ASCII byte (`"h\303\251llo.txt"`), and the quoted form
        // is not a path anything can open. It also removes the need to guess
        // how many tab-separated fields a record has, and the old pairwise
        // `zip` of the two command outputs, which silently misaligned every
        // subsequent file if the two ever disagreed on line count.
        let range = format!("{base}...{target}");
        let names = self.git(&["diff", "--name-status", "-z", "-M", &range])?;
        let stats = self.git(&["diff", "--numstat", "-z", "-M", &range])?;

        let line_counts = parse_numstat(&stats);

        let mut files = Vec::new();
        let mut records = nul_records(&names);
        while let Some(code) = records.next() {
            // Renames and copies carry two paths; everything else carries one.
            // The new path is the one under review.
            let path = if code.starts_with('R') || code.starts_with('C') {
                let _old = records.next();
                records.next()
            } else {
                records.next()
            };
            let Some(path) = path else { break };

            let (added, removed) = line_counts.get(path).copied().unwrap_or((0, 0));
            files.push(ChangedFile {
                path: path.to_string(),
                status: ChangeStatus::from_code(code),
                added,
                removed,
            });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChangeStatus;

    fn sh(dir: &Path, args: &[&str]) -> String {
        run(dir, "git", args).unwrap_or_else(|e| panic!("git {args:?}: {e}"))
    }

    /// A repo whose paths are everything a naive tab-splitting parser gets
    /// wrong: spaces, embedded tabs, non-ASCII, a rename and a binary file.
    fn hostile_repo() -> (PathBuf, GitVcs) {
        // A per-call counter, not a timestamp. `SystemTime` has millisecond
        // resolution on some platforms, so four tests starting together landed
        // on the same directory name and raced `git init` against each other.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("locust-git-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        sh(&dir, &["init", "-q", "--initial-branch=main"]);
        sh(&dir, &["config", "user.email", "t@example.com"]);
        sh(&dir, &["config", "user.name", "Test"]);

        std::fs::write(dir.join("keep.txt"), "a\n").unwrap();
        std::fs::write(dir.join("original.txt"), "line\n".repeat(200)).unwrap();
        std::fs::write(dir.join("Cargo.lock"), "lock\n").unwrap();
        sh(&dir, &["add", "-A"]);
        sh(&dir, &["commit", "-qm", "base"]);

        sh(&dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("keep.txt"), "a\nb\n").unwrap();
        std::fs::write(dir.join("with space.txt"), "s\n").unwrap();
        std::fs::write(dir.join("tab\there.txt"), "t\n").unwrap();
        std::fs::write(dir.join("héllo.txt"), "u\n").unwrap();
        std::fs::write(dir.join("bin.dat"), [0u8, 1, 2, 0, 3]).unwrap();
        std::fs::write(dir.join("Cargo.lock"), "lock\nmore\n").unwrap();
        // Rename with a small edit, so git reports R<score> rather than A/D.
        std::fs::rename(dir.join("original.txt"), dir.join("moved.txt")).unwrap();
        let mut moved = std::fs::read_to_string(dir.join("moved.txt")).unwrap();
        moved.push_str("tail\n");
        std::fs::write(dir.join("moved.txt"), moved).unwrap();
        sh(&dir, &["add", "-A"]);
        sh(&dir, &["commit", "-qm", "change"]);

        let vcs = GitVcs::open(&dir).unwrap();
        (dir, vcs)
    }

    #[test]
    fn paths_git_would_quote_survive_parsing() {
        let (dir, vcs) = hostile_repo();
        let files = vcs.changed_files("main", "feature").unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();

        // Raw paths, not git's quoted rendering.
        for expected in ["with space.txt", "tab\there.txt", "héllo.txt"] {
            assert!(paths.contains(&expected), "missing {expected:?} in {paths:?}");
        }
        assert!(
            !paths.iter().any(|p| p.starts_with('"')),
            "quoted path leaked through: {paths:?}"
        );

        // And every reported path is actually readable at the target revision.
        for f in &files {
            if !f.status.is_deleted() {
                assert!(
                    vcs.show_file("feature", &f.path).unwrap().is_some(),
                    "cannot open reported path {:?}",
                    f.path
                );
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn line_counts_land_on_the_right_file() {
        let (dir, vcs) = hostile_repo();
        let files = vcs.changed_files("main", "feature").unwrap();
        let get = |p: &str| files.iter().find(|f| f.path == p).unwrap_or_else(|| panic!("no {p}"));

        // keep.txt gained exactly one line; the old zip-based parser could
        // attribute another file's counts here without any error surfacing.
        assert_eq!((get("keep.txt").added, get("keep.txt").removed), (1, 0));
        assert_eq!(get("with space.txt").added, 1);
        // Binary files report `-`; that must be zero, not a parse failure.
        assert_eq!((get("bin.dat").added, get("bin.dat").removed), (0, 0));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_rename_is_reported_under_its_new_path() {
        let (dir, vcs) = hostile_repo();
        let files = vcs.changed_files("main", "feature").unwrap();
        let moved = files.iter().find(|f| f.path == "moved.txt").expect("renamed file");
        assert_eq!(moved.status, ChangeStatus::Renamed);
        assert!(!files.iter().any(|f| f.path == "original.txt"), "old path must not appear");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_path_scoped_diff_contains_only_those_paths() {
        let (dir, vcs) = hostile_repo();
        let full = vcs.diff("main", "feature").unwrap();
        assert!(full.contains("Cargo.lock"), "the unscoped diff has everything");

        let scoped = vcs.diff_paths("main", "feature", &["keep.txt".to_string()]).unwrap();
        assert!(scoped.contains("keep.txt"));
        assert!(!scoped.contains("Cargo.lock"), "excluded content leaked: {scoped}");
        assert!(!scoped.contains("héllo.txt"));

        // An empty path list is an empty diff, not the whole thing.
        assert!(vcs.diff_paths("main", "feature", &[]).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_path_with_glob_characters_is_matched_literally() {
        let (dir, vcs) = hostile_repo();
        // `*.txt` as a pathspec would match several files; as a literal path it
        // matches nothing, which is what we want when passing real paths.
        let scoped = vcs.diff_paths("main", "feature", &["*.txt".to_string()]).unwrap();
        assert!(scoped.is_empty(), "pathspec glob was interpreted: {scoped}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn numstat_records_parse_without_a_repository() {
        // Rename: empty path field, then old and new as separate records.
        let raw = "1\t1\t\0old.rs\0new.rs\0-\t-\0bin.dat\0".replace("-\t-\0", "-\t-\tbin.dat\0");
        let counts = parse_numstat(&raw);
        assert_eq!(counts.get("new.rs"), Some(&(1, 1)));
        assert_eq!(counts.get("old.rs"), None, "renames key on the new path");
        assert_eq!(counts.get("bin.dat"), Some(&(0, 0)));
    }
}
