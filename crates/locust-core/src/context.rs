//! The context pack: everything an agent is told about the change.
//!
//! Agents backed by an agentic CLI can open whatever they like inside their own
//! worktree, but API-backed agents see only what we hand them. Reviewing a
//! unified diff alone is the single biggest cause of confident, wrong findings
//! — the reviewer cannot see the guard twelve lines above the hunk. So the pack
//! carries the post-change contents of every touched file, within a budget.

use crate::config::ContextConfig;
use anyhow::Result;
use locust_vcs::{ChangedFile, ReviewRange, Vcs};
use std::fmt::Write as _;

pub struct ContextPack {
    pub range: ReviewRange,
    pub files: Vec<ChangedFile>,
    pub diff: String,
    /// Post-change contents of the touched files, already budgeted.
    pub sources: Vec<(String, String)>,
    pub truncated: Vec<String>,
    pub notes: String,
}

impl ContextPack {
    pub fn build(vcs: &dyn Vcs, range: &ReviewRange, cfg: &ContextConfig) -> Result<Self> {
        let all = vcs.changed_files(&range.merge_base, &range.target)?;
        let files: Vec<ChangedFile> =
            all.into_iter().filter(|f| !is_excluded(&f.path, &cfg.exclude)).collect();

        let diff = vcs.diff(&range.merge_base, &range.target)?;

        let mut sources = Vec::new();
        let mut truncated = Vec::new();
        if cfg.include_full_files {
            // Smallest first, so a budget blown by one enormous file does not
            // cost the reviewer every other file's context.
            let mut ordered: Vec<&ChangedFile> =
                files.iter().filter(|f| !f.status.is_deleted()).collect();
            ordered.sort_by_key(|f| f.added + f.removed);

            let mut used = 0usize;
            for f in ordered {
                let Some(text) = vcs.show_file(&range.target, &f.path)? else { continue };
                if looks_binary(&text) {
                    continue;
                }
                if used + text.len() > cfg.max_total_bytes {
                    truncated.push(f.path.clone());
                    continue;
                }
                let text = if text.len() > cfg.max_file_bytes {
                    truncated.push(f.path.clone());
                    clip(&text, cfg.max_file_bytes)
                } else {
                    text
                };
                used += text.len();
                sources.push((f.path.clone(), text));
            }
        }

        Ok(Self {
            range: range.clone(),
            files,
            diff,
            sources,
            truncated,
            notes: cfg.notes.clone(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Rendered for a prompt. `with_sources` is false for agents that have a
    /// worktree and can read the files themselves.
    pub fn render(&self, with_sources: bool) -> String {
        let mut s = String::new();

        let _ = writeln!(s, "## Change under review");
        let _ = writeln!(s, "\nbase `{}` → target `{}`\n", short(&self.range.base), short(&self.range.target));

        let _ = writeln!(s, "### Files touched\n");
        for f in &self.files {
            let _ = writeln!(s, "- `{}` ({:?}, +{} -{})", f.path, f.status, f.added, f.removed);
        }

        if !self.notes.trim().is_empty() {
            let _ = writeln!(s, "\n### Project notes\n\n{}", self.notes.trim());
        }

        let _ = writeln!(s, "\n### Diff\n\n```diff\n{}\n```", self.diff.trim_end());

        if with_sources && !self.sources.is_empty() {
            let _ = writeln!(
                s,
                "\n### Full contents after the change\n\nUse these to judge the hunks in context; \
                 a guard or invariant outside the diff is usually what decides whether a finding is real."
            );
            for (path, text) in &self.sources {
                let lang = language_of(path);
                let _ = writeln!(s, "\n#### `{path}`\n\n```{lang}\n{}\n```", text.trim_end());
            }
        }

        if !self.truncated.is_empty() {
            let _ = writeln!(
                s,
                "\n_Truncated or omitted for size: {}._",
                self.truncated.join(", ")
            );
        }
        s
    }
}

fn is_excluded(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| {
        let p = p.trim();
        if p.is_empty() {
            false
        } else if let Some(suffix) = p.strip_prefix('*') {
            path.ends_with(suffix)
        } else {
            path.contains(p)
        }
    })
}

fn looks_binary(text: &str) -> bool {
    text.bytes().take(8000).any(|b| b == 0)
}

fn clip(text: &str, max: usize) -> String {
    // Keep the head and the tail: imports and signatures live at the top,
    // and the most recently appended code at the bottom.
    let half = max / 2;
    let head_end = floor_char_boundary(text, half);
    let tail_start = ceil_char_boundary(text, text.len().saturating_sub(half));
    format!(
        "{}\n\n// ... {} bytes elided by locust ...\n\n{}",
        &text[..head_end],
        text.len() - max,
        &text[tail_start..]
    )
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn short(rev: &str) -> &str {
    &rev[..rev.len().min(12)]
}

fn language_of(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" => "javascript",
        "py" => "python",
        "go" => "go",
        "java" => "java",
        "kt" => "kotlin",
        "rb" => "ruby",
        "c" | "h" => "c",
        "cc" | "cpp" | "hpp" => "cpp",
        "cs" => "csharp",
        "swift" => "swift",
        "sh" | "bash" => "bash",
        "sql" => "sql",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusions_match_substrings_and_suffixes() {
        let pats = vec!["/node_modules/".to_string(), "*.min.js".to_string()];
        assert!(is_excluded("web/node_modules/x/index.js", &pats));
        assert!(is_excluded("static/app.min.js", &pats));
        assert!(!is_excluded("src/app.js", &pats));
    }

    #[test]
    fn clip_keeps_head_and_tail_without_splitting_a_char() {
        let text = format!("héad{}taïl", "x".repeat(500));
        let out = clip(&text, 100);
        assert!(out.starts_with("héad"));
        assert!(out.ends_with("taïl"));
        assert!(out.contains("elided by locust"));
    }
}
