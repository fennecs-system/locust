//! Ground truth for the arena.
//!
//! Without it, the arena's output cannot be read. A run that confirms
//! everything might mean the reviewers are excellent, or that the defence is
//! unmotivated and the panel rubber-stamps — those look identical from the
//! outside, and no amount of staring at a report distinguishes them.
//!
//! A case is a change with a known answer: defects a working arena should
//! confirm. Missing one is a false negative, and the recall figure is the
//! headline.
//!
//! Findings matching nothing planted are reported as **unclassified** rather
//! than counted against the run. An unanticipated finding is often a real
//! defect the case author missed, and scoring it as a false positive would
//! train the benchmark to reward blandness.
//!
//! **Decoys used to live here** — code written to *look* defective and not be,
//! planted so that a defender which refuted them could be told apart from one
//! that concedes everything. The idea was sound and the delivery was not: six
//! decoys across three cases drew no reviewer in any live run, so the column
//! reporting them printed `untested` every time. Known-false claims now reach
//! the defence directly, as a fixed slate in
//! `crates/locust-core/tests/defence_probe.rs`, which does not wait for an
//! attacker to take bait. The measurement survives; the apparatus for hoping
//! it happens by itself does not.

use crate::model::{Finding, Ruling, Severity, Stance, Verdict};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How far from a declared line a finding may sit and still be the same thing.
/// Generous: reviewers cite the symptom where the case author cited the cause.
const LINE_SLACK: u32 = 25;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defect {
    pub id: String,
    pub file: String,
    /// Inclusive line range the defect lives in, if it has one.
    #[serde(default)]
    pub lines: Option<[u32; 2]>,
    #[serde(default)]
    pub severity: Severity,
    /// What it is, for the report. Not used for matching.
    pub summary: String,
    /// Every one of these must appear in the finding's text. Location alone is
    /// too weak: planted defects cluster in the same few functions, so without
    /// this a finding about one would be credited to its neighbour.
    pub matches: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseManifest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub notes: String,
    #[serde(rename = "defect")]
    pub defects: Vec<Defect>,
}

#[derive(Debug, Clone)]
pub struct Case {
    pub manifest: CaseManifest,
    pub dir: PathBuf,
}

impl Case {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("case.toml");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let manifest: CaseManifest =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

        if !dir.join("base").is_dir() || !dir.join("head").is_dir() {
            bail!("case `{}` needs both a base/ and a head/ tree", manifest.name);
        }
        for d in &manifest.defects {
            if d.matches.is_empty() {
                bail!("defect `{}` has no `matches`; it could never be credited", d.id);
            }
        }
        Ok(Self { manifest, dir: dir.to_path_buf() })
    }

    /// Every case directory under `root`, in a stable order.
    pub fn discover(root: &Path) -> Result<Vec<Case>> {
        let mut cases = Vec::new();
        let mut entries: Vec<PathBuf> = std::fs::read_dir(root)
            .with_context(|| format!("reading cases from {}", root.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join("case.toml").is_file())
            .collect();
        entries.sort();
        for dir in entries {
            cases.push(Case::load(&dir)?);
        }
        Ok(cases)
    }

    pub fn real(&self) -> impl Iterator<Item = &Defect> {
        self.manifest.defects.iter()
    }

    /// Build the case into a throwaway git repository: `base/` as the first
    /// commit on `main`, `head/` as a second commit on a branch, so the arena
    /// reviews exactly the difference between them.
    pub fn materialise(&self, into: &Path) -> Result<PathBuf> {
        let repo = into.join(&self.manifest.name);
        if repo.exists() {
            std::fs::remove_dir_all(&repo)?;
        }
        std::fs::create_dir_all(&repo)?;

        let git = |args: &[&str]| -> Result<()> {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .context("running git")?;
            if !out.status.success() {
                bail!("git {:?}: {}", args, String::from_utf8_lossy(&out.stderr).trim());
            }
            Ok(())
        };

        git(&["init", "-q", "--initial-branch=main"])?;
        git(&["config", "user.email", "bench@locust.test"])?;
        git(&["config", "user.name", "locust bench"])?;

        copy_tree(&self.dir.join("base"), &repo)?;
        git(&["add", "-A"])?;
        git(&["commit", "-qm", "base"])?;

        git(&["checkout", "-q", "-b", "change"])?;
        // Remove the tracked tree first so a file deleted by the change is
        // actually deleted, rather than lingering from base.
        for entry in std::fs::read_dir(&repo)?.flatten() {
            if entry.file_name() == ".git" {
                continue;
            }
            let p = entry.path();
            if p.is_dir() {
                std::fs::remove_dir_all(p)?;
            } else {
                std::fs::remove_file(p)?;
            }
        }
        copy_tree(&self.dir.join("head"), &repo)?;
        git(&["add", "-A"])?;
        git(&["commit", "-qm", "change under review"])?;

        Ok(repo)
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    for entry in std::fs::read_dir(from)?.flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// What a confirmed finding turned out to be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Classified {
    /// Matched a planted defect.
    Real { defect: String },
    /// Matched nothing in the manifest. Possibly a real defect the case
    /// author missed, so it is reported for triage rather than scored.
    Unclassified,
}

/// What a canonical finding's duplicates agree it is, when they are unanimous.
///
/// Returns `None` unless at least one duplicate could be placed and every
/// duplicate that could be placed names the same defect. Disagreement means
/// the cluster is genuinely mixed, which is a triage question, not a credit.
fn cluster_consensus(
    canonical: &Finding,
    all: &[&Finding],
    defects: &[Defect],
) -> Option<Classified> {
    let mut agreed: Option<Classified> = None;
    for sibling in all.iter().filter(|f| f.duplicate_of.as_deref() == Some(&canonical.id)) {
        match classify(sibling, defects) {
            Classified::Unclassified => continue,
            placed => match &agreed {
                None => agreed = Some(placed),
                Some(existing) if *existing == placed => {}
                Some(_) => return None,
            },
        }
    }
    agreed
}

/// Decide what a finding refers to.
///
/// Both the location and the keywords have to agree. Location alone credits a
/// finding to its neighbour, since planted defects cluster; keywords alone
/// credit a finding in an unrelated file that happens to use the same words.
/// Quoted evidence is excluded: it may contain unrelated code. Ambiguous
/// matches are left for human triage rather than settled by manifest order.
pub fn classify(finding: &Finding, defects: &[Defect]) -> Classified {
    match matching(finding, defects).as_slice() {
        [one] => Classified::Real { defect: one.id.clone() },
        // None, or several: a finding that fits more than one planted defect
        // needs a human, regardless of manifest order. `Scorecard::build`
        // reports which entries it straddled rather than discarding that.
        _ => Classified::Unclassified,
    }
}

/// Every planted defect a finding could be describing.
///
/// Both the location and the keywords have to agree, for the reasons in
/// `classify`. Returning the whole set rather than the first hit is what lets
/// the scorecard say *why* a finding was left unplaced: one that matches two
/// entries because it names two mechanisms reads very differently from one
/// that matches nothing, and reporting the second defect as simply "missed"
/// when a confirmed finding described it is the failure this avoids.
pub fn matching<'a>(finding: &Finding, defects: &'a [Defect]) -> Vec<&'a Defect> {
    let haystack = format!(
        "{} {} {}",
        finding.raw.title, finding.raw.claim, finding.raw.failure_scenario
    )
    .to_lowercase();

    defects
        .iter()
        .filter(|defect| normalize(&defect.file) == normalize(&finding.raw.file))
        .filter(|defect| match (defect.lines, finding.raw.line) {
            (Some([lo, hi]), Some(line)) => line + LINE_SLACK >= lo && line <= hi + LINE_SLACK,
            _ => true,
        })
        .filter(|defect| defect.matches.iter().all(|m| haystack.contains(&m.to_lowercase())))
        .collect()
}

fn normalize(path: &str) -> String {
    path.trim().trim_start_matches("./").trim_start_matches('/').to_lowercase()
}

/// How one case went.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Scorecard {
    pub case: String,
    pub real_planted: usize,
    /// Planted defects a confirmed finding matched.
    pub real_found: Vec<String>,
    pub real_missed: Vec<String>,
    /// Confirmed findings matching nothing planted; need a human to triage.
    pub unclassified: Vec<String>,
    /// Confirmed findings that described more than one planted defect at once.
    /// Scoring credits at most one of them, so without this the others read as
    /// missed even though the run surfaced them.
    #[serde(default)]
    pub straddled: Vec<String>,

    // --- what the defence actually did ---
    pub rebuttals: usize,
    pub refuted_stance: usize,
    pub narrowed_stance: usize,
    pub conceded_stance: usize,

    pub findings_raised: usize,
    pub findings_confirmed: usize,
    pub findings_refuted: usize,
    pub elapsed_secs: f64,
    pub cost_usd: f64,
}

impl Scorecard {
    /// Share of planted defects the arena confirmed.
    pub fn recall(&self) -> f64 {
        ratio(self.real_found.len(), self.real_planted)
    }

    /// Share of rebuttals that pushed back at all, rather than conceding.
    pub fn defence_pushback(&self) -> f64 {
        ratio(self.refuted_stance + self.narrowed_stance, self.rebuttals)
    }

    pub fn build(case: &Case, outcome: &crate::model::RunOutcome) -> Self {
        let mut card = Scorecard {
            case: case.manifest.name.clone(),
            real_planted: case.real().count(),
            elapsed_secs: outcome.elapsed_secs,
            cost_usd: outcome.usage.cost_usd,
            ..Default::default()
        };

        for gen in &outcome.generations {
            card.findings_raised += gen.findings.len();
            for r in &gen.rulings {
                match r.raw.verdict {
                    Verdict::Confirmed => card.findings_confirmed += 1,
                    Verdict::Refuted => card.findings_refuted += 1,
                    Verdict::Undecided => {}
                }
            }
            for r in &gen.rebuttals {
                card.rebuttals += 1;
                match r.raw.stance {
                    Stance::Refute => card.refuted_stance += 1,
                    Stance::Narrow => card.narrowed_stance += 1,
                    Stance::Concede => card.conceded_stance += 1,
                }
            }
        }

        // Every finding raised, so a cluster's duplicates can speak for a
        // canonical member the manifest cannot place.
        let all_findings: Vec<&Finding> =
            outcome.generations.iter().flat_map(|g| g.findings.iter()).collect();

        for (finding, _) in outcome.confirmed_findings() {
            let mut verdict = classify(finding, &case.manifest.defects);

            // A finding can name two planted defects at once — "writes before
            // the check, and swallows the denial" is one sentence about two
            // bugs. Only one of them can be credited, so record the straddle:
            // otherwise the other appears in `real_missed` and the next reader
            // goes hunting for a reviewer weakness the run does not have.
            let candidates = matching(finding, &case.manifest.defects);
            if candidates.len() > 1 {
                let ids: Vec<&str> = candidates.iter().map(|d| d.id.as_str()).collect();
                card.straddled.push(format!(
                    "{} — {} (names {})",
                    finding.location(),
                    finding.raw.title,
                    ids.join(" + ")
                ));
            }

            // A thorough finding names several mechanisms, so it matches
            // several entries and the ambiguity rule discards it — while the
            // narrower duplicates clustered beneath it each name one defect
            // unambiguously. Consulting them recovers an identification the
            // run already made, rather than inventing one: it is used only
            // when the canonical member is unplaceable and the duplicates
            // agree on exactly one defect.
            if verdict == Classified::Unclassified {
                if let Some(agreed) = cluster_consensus(finding, &all_findings, &case.manifest.defects)
                {
                    verdict = agreed;
                }
            }

            match verdict {
                Classified::Real { defect } => {
                    if !card.real_found.contains(&defect) {
                        card.real_found.push(defect);
                    }
                }
                Classified::Unclassified => {
                    card.unclassified.push(format!(
                        "{} — {}",
                        finding.location(),
                        finding.raw.title
                    ));
                }
            }
        }

        // A defect named by a straddling finding is neither found nor missed:
        // the run surfaced it, but the credit went to the entry it shared the
        // finding with. Listing it as missed is how a benchmark sends the next
        // reader hunting a weakness that is not there.
        let straddled_ids: Vec<String> = outcome
            .confirmed_findings()
            .iter()
            .flat_map(|(f, _)| matching(f, &case.manifest.defects))
            .map(|d| d.id.clone())
            .collect();
        card.real_missed = case
            .real()
            .map(|d| d.id.clone())
            .filter(|id| !card.real_found.contains(id) && !straddled_ids.contains(id))
            .collect();
        card
    }
}

fn ratio(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        n as f64 / d as f64
    }
}

/// Unused today, but the signature the scorer needs if rulings are ever scored
/// per-defect rather than per-finding.
pub fn verdict_of<'a>(rulings: &'a [Ruling], finding: &Finding) -> Option<&'a Ruling> {
    rulings.iter().find(|r| r.raw.finding_id == finding.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Grounding, RawFinding};

    #[test]
    fn the_session_cache_manifest_distinguishes_claims_not_function_names() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../cases/session-cache");
        let case = Case::load(&root).unwrap();
        let examples = [
            (30, "put() evicts another session when updating a key, even though put() holds the lock.",
                Classified::Real { defect: "put-evicts-on-update".into() }),
            (47, "purge_expired() deletes a refreshed session after taking its stale snapshot.",
                Classified::Real { defect: "purge-deletes-refreshed-session".into() }),
            // Known-false claims are probe input now, not manifest entries. The
            // manifest must leave them unplaced rather than quietly crediting
            // one to a real defect in the same function.
            (43, "size() reads len(self._entries) without the lock, which looks like a data race.",
                Classified::Unclassified),
            (47, "purge_expired() mutates _entries while iterating it: RuntimeError: dictionary changed size during iteration.",
                Classified::Unclassified),
        ];
        for (line, text, expected) in examples {
            let mut f = finding("src/cache.py", Some(line), text, "");
            assert_eq!(classify(&f, &case.manifest.defects), expected);
            f.raw.evidence = "get() lock _hits KeyError LRU put() evict update size() purge_expired() iteration".into();
            assert_eq!(classify(&f, &case.manifest.defects), expected);
        }
        let ambiguous = finding("src/cache.py", Some(30), "get() has no lock and put() evicts when updating", "");
        assert_eq!(classify(&ambiguous, &case.manifest.defects), Classified::Unclassified);
        let mut reversed = case.manifest.defects.clone();
        reversed.reverse();
        assert_eq!(classify(&ambiguous, &reversed), Classified::Unclassified);
    }

    #[test]
    fn reported_resurrection_is_already_covered_by_unlocked_reads() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../cases/session-cache");
        let case = Case::load(&root).unwrap();
        // Verbatim finding from the live run cited by the handoff. Its title,
        // claim and scenario must all be tested for keyword collisions.
        let mut reported = finding("src/cache.py", Some(25),
            r#"Unlocked refresh in get() resurrects a session just evicted by put()"#,
            r#"get() reads the entry, then later writes `self._entries[key] = (value, time.time() + self.ttl)` without holding `self._lock`. If put() evicts that key under the lock between get's read and its write-back, get re-inserts the evicted session with a fresh TTL. This undoes the eviction and pushes the store past max_entries."#);
        reported.raw.failure_scenario = r#"The cache is full. Thread A in get('u1') has read `entry` and passed the expiry check. Thread B in put('u2') holds the lock, finds 'u1' coldest, and deletes it from _entries and _hits, then inserts 'u2'. Thread A resumes and runs `self._hits['u1'] = 0+1` and `self._entries['u1'] = (value, now+ttl)`. 'u1' is alive again with a full new TTL, and len(_entries) == max_entries+1. The same happens if purge_expired removes the key in that window. Each such race adds one more entry beyond the bound."#.into();
        let expected = Classified::Real { defect: "unlocked-read-write".into() };
        assert_eq!(classify(&reported, &case.manifest.defects), expected);
        let mut reversed = case.manifest.defects.clone();
        reversed.reverse();
        assert_eq!(classify(&reported, &reversed), expected);
    }

    fn defect(id: &str, file: &str, lines: Option<[u32; 2]>, matches: &[&str]) -> Defect {
        Defect {
            id: id.into(),
            file: file.into(),
            lines,
            severity: Severity::High,
            summary: "s".into(),
            matches: matches.iter().map(|m| m.to_string()).collect(),
        }
    }

    fn finding(file: &str, line: Option<u32>, title: &str, claim: &str) -> Finding {
        Finding {
            id: "f1".into(),
            generation: 0,
            persona: "p".into(),
            persona_id: "p".into(),
            provider: "x".into(),
            raw: RawFinding {
                file: file.into(),
                line,
                category: String::new(),
                title: title.into(),
                claim: claim.into(),
                failure_scenario: String::new(),
                evidence: String::new(),
                severity: Severity::High,
                altitude: Default::default(),
                confidence: 0.9,
            },
            cluster: "f1".into(),
            duplicate_of: None,
            corroboration: 1,
            grounding: Grounding::Verified,
        }
    }

    /// A reviewer that writes "writes before the check *and* swallows the
    /// denial" has found two planted defects in one sentence. Only one of them
    /// can be credited, and the other must not be reported as missed: that is
    /// how a benchmark sends the next reader hunting a weakness the run does
    /// not have, which has already happened twice in this project's history.
    #[test]
    fn a_finding_describing_two_defects_straddles_rather_than_going_missing() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../cases/permission-check");
        let case = Case::load(&root).unwrap();

        // Verbatim from the live run of 20 October 2026.
        let title = "save_invoice writes before the permission check and ignores Denied";
        let claim = "save_invoice calls store.write(invoice) before it calls check_permission. \
                     It then catches any exception and only logs a warning, so a user without \
                     invoice:write still gets their invoice saved.";
        let scenario = "check_permission raises Denied('invoice:write'), the `except Exception` \
                        catches it and logs a warning, and the function returns invoice.id.";

        let outcome: crate::model::RunOutcome = serde_json::from_value(serde_json::json!({
            "run_id": "t", "started_at": "2026-10-20T00:00:00Z", "base": "main",
            "target": "change", "vcs": "git", "themes": [], "elapsed_secs": 1.0,
            "usage": {"input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0},
            "generations": [{
                "generation": 0, "pressure": 1.0, "personas": [], "rebuttals": [], "fitness": [],
                "usage": {"input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0},
                "elapsed_secs": 1.0,
                "findings": [{
                    "id": "f0-0-0", "generation": 0, "persona": "p", "persona_id": "p",
                    "provider": "x", "cluster": "f0-0-0", "corroboration": 1,
                    "grounding": {"status": "verified"}, "file": "src/api.py", "line": 16,
                    "category": "correctness", "title": title, "claim": claim,
                    "failure_scenario": scenario, "evidence": "", "severity": "critical",
                    "altitude": "local", "confidence": 0.9
                }],
                "rulings": [{
                    "finding_id": "f0-0-0", "verdict": "confirmed", "severity": "critical",
                    "altitude": "local", "reasoning": "r", "novel": true,
                    "judge_provider": "y", "generation": 0, "panel": [], "dissent": 0.0
                }]
            }]
        }))
        .unwrap();

        let card = Scorecard::build(&case, &outcome);
        assert_eq!(card.straddled.len(), 1, "the straddle must be reported: {card:?}");
        assert!(card.straddled[0].contains("denial-swallowed-in-save-invoice"));
        assert!(card.straddled[0].contains("write-precedes-the-check"));
        for id in ["denial-swallowed-in-save-invoice", "write-precedes-the-check"] {
            assert!(
                !card.real_missed.contains(&id.to_string()),
                "`{id}` was described by a confirmed finding, so it is not missed: {:?}",
                card.real_missed
            );
        }
        // The defects nothing described are still missed.
        assert!(card.real_missed.contains(&"cache-keyed-on-user-alone".to_string()));
    }

    #[test]
    fn a_finding_matching_location_and_keywords_is_credited() {
        let defects = vec![defect("desync", "src/cache.py", Some([20, 36]), &["_hits", "KeyError"])];
        let f = finding("src/cache.py", Some(22), "expiry leaves _hits populated", "_evict then raises KeyError");
        assert_eq!(classify(&f, &defects), Classified::Real { defect: "desync".into() });
    }

    #[test]
    fn location_alone_does_not_credit_a_neighbouring_defect() {
        // Planted defects cluster in the same function; without the keyword
        // requirement each would be credited for the other's discovery.
        let defects = vec![defect("desync", "src/cache.py", Some([20, 36]), &["_hits", "KeyError"])];
        let f = finding("src/cache.py", Some(24), "get() mutates shared state without the lock", "a race");
        assert_eq!(classify(&f, &defects), Classified::Unclassified);
    }

    #[test]
    fn keywords_alone_do_not_credit_a_finding_in_another_file() {
        let defects = vec![defect("desync", "src/cache.py", Some([20, 36]), &["_hits"])];
        let f = finding("src/other.py", Some(22), "_hits is never pruned", "leak");
        assert_eq!(classify(&f, &defects), Classified::Unclassified);
    }

    #[test]
    fn a_reviewer_citing_the_symptom_rather_than_the_cause_still_matches() {
        let defects = vec![defect("desync", "src/cache.py", Some([20, 20]), &["_hits"])];
        // 40 lines away is the same defect seen from `_evict`; slack covers it.
        let f = finding("src/cache.py", Some(38), "_hits and _entries disagree", "boom");
        assert_eq!(classify(&f, &defects), Classified::Real { defect: "desync".into() });
        // 300 lines away is a different part of the file.
        let far = finding("src/cache.py", Some(300), "_hits and _entries disagree", "boom");
        assert_eq!(classify(&far, &defects), Classified::Unclassified);
    }

    #[test]
    fn a_defect_with_no_line_range_matches_anywhere_in_its_file() {
        let defects = vec![defect("anywhere", "a.rs", None, &["widget"])];
        let f = finding("a.rs", Some(9999), "widget is wrong", "x");
        assert_eq!(classify(&f, &defects), Classified::Real { defect: "anywhere".into() });
    }

    #[test]
    fn paths_are_compared_leniently() {
        let defects = vec![defect("d", "src/cache.py", None, &["x"])];
        let f = finding("./src/cache.py", None, "x happens", "x");
        assert_eq!(classify(&f, &defects), Classified::Real { defect: "d".into() });
    }

    /// The case has to become a repository whose diff is exactly the change,
    /// or the arena reviews the wrong thing and every score is meaningless.
    #[test]
    fn a_case_materialises_into_a_repo_whose_diff_is_the_change() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../cases/session-cache");
        if !root.exists() {
            return; // cases are optional for a library consumer
        }
        let case = Case::load(&root).expect("case loads");
        let tmp = std::env::temp_dir().join(format!("locust-bench-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let repo = case.materialise(&tmp).expect("materialises");

        let git = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };

        // Two commits, and the branch the config reviews actually exists.
        assert_eq!(git(&["rev-list", "--count", "HEAD"]).trim(), "2");
        assert!(git(&["branch"]).contains("change"));

        let diff = git(&["diff", "main...change"]);
        assert!(diff.contains("max_entries"), "the change is in the diff");
        assert!(diff.contains("_hits"), "and so is the planted defect");
        // api.py is identical in both trees, so it must not appear.
        assert!(!diff.contains("api.py"), "unchanged files must not be in the diff");

        // Every file a defect names has to exist at the reviewed revision,
        // otherwise the citation check would reject correct findings.
        for d in &case.manifest.defects {
            let shown = git(&["show", &format!("change:{}", d.file)]);
            assert!(!shown.is_empty(), "defect `{}` names a missing file {}", d.id, d.file);
        }

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// From a live benchmark run: the arena reported
    /// "Expiry in get() leaves an orphan _hits key; eviction then KeyErrors
    /// forever" — which is `hits-entries-desync` — and the scorecard listed
    /// that defect as missed.
    ///
    /// The cause was a one-character keyword on a neighbouring entry: the
    /// failure scenario happens to say "hits=0", which satisfied
    /// `["_hits", "0"]`, so two entries matched and the ambiguity rule
    /// discarded a correct identification. Leaving genuinely ambiguous
    /// matches for triage is right; a keyword broad enough to manufacture the
    /// ambiguity is not, and the cost is a benchmark that under-reports
    /// recall and names the wrong defect as missed.
    #[test]
    fn a_found_defect_is_not_reported_as_missed_through_a_broad_neighbour() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../cases/session-cache");
        if !root.exists() {
            return;
        }
        let case = Case::load(&root).expect("case loads");

        // Verbatim from the run that exposed this, including the "hits=0"
        // that caused the collision.
        let mut reported = finding(
            "src/cache.py",
            Some(21),
            "Expiry in get() leaves an orphan _hits key; eviction then KeyErrors forever",
            "When get() finds an expired entry it deletes `self._entries[key]` but leaves              `self._hits[key]`. _evict() picks its victim from `_hits` and runs              `del self._entries[coldest]` first, so it can raise KeyError.",
        );
        reported.raw.failure_scenario =
            "Session K is put (hits=0) and expires, then a request calls get(K): line 21              deletes _entries[K], but _hits[K]=0 stays."
                .into();

        assert_eq!(
            classify(&reported, &case.manifest.defects),
            Classified::Real { defect: "hits-entries-desync".into() },
        );
    }

    /// From the second live run: three reviewers independently reported
    /// `hits-entries-desync`, the arena confirmed it, and the scorecard listed
    /// it as missed.
    ///
    /// Clustering had made the most thorough report canonical. That report
    /// names both the orphaned hit count and the missing lock, so it matched
    /// two manifest entries and the ambiguity rule discarded it — while the
    /// narrower duplicates beneath it each named one defect cleanly and were
    /// never consulted, because only canonical findings are classified.
    ///
    /// Penalising a reviewer for thoroughness is exactly backwards.
    #[test]
    fn a_cluster_speaks_for_a_canonical_finding_the_manifest_cannot_place() {
        let ambiguous = Defect {
            id: "ambiguous-neighbour".into(),
            file: "src/cache.py".into(),
            lines: None,
            severity: Severity::High,
            summary: "s".into(),
            matches: vec!["lock".into()],
        };
        let desync = defect("desync", "src/cache.py", None, &["_hits", "KeyError"]);
        let defects = vec![desync, ambiguous];

        // Thorough: names the orphan *and* the lock, so it matches both.
        let mut canonical = finding(
            "src/cache.py",
            Some(21),
            "expiry leaves _hits and the lock does not cover get()",
            "_evict then raises KeyError",
        );
        canonical.id = "f0-0-1".into();
        assert_eq!(classify(&canonical, &defects), Classified::Unclassified);

        // Narrow duplicates beneath it, each placeable.
        let mut dup = finding("src/cache.py", Some(20), "stale _hits key", "later puts raise KeyError");
        dup.id = "f0-2-0".into();
        dup.duplicate_of = Some("f0-0-1".into());

        let all = vec![&canonical, &dup];
        assert_eq!(
            cluster_consensus(&canonical, &all, &defects),
            Some(Classified::Real { defect: "desync".into() }),
        );

        // A mixed cluster stays a triage question rather than a credit.
        let mut other = finding("src/cache.py", Some(26), "get() has no lock", "race");
        other.id = "f0-9-0".into();
        other.duplicate_of = Some("f0-0-1".into());
        let mixed = vec![&canonical, &dup, &other];
        assert_eq!(cluster_consensus(&canonical, &mixed, &defects), None);
    }

    #[test]
    fn scores_are_zero_safe_and_read_the_right_way_round() {
        let card = Scorecard {
            real_planted: 4,
            real_found: vec!["a".into(), "b".into()],
            rebuttals: 10,
            refuted_stance: 2,
            narrowed_stance: 1,
            conceded_stance: 7,
            ..Default::default()
        };
        assert!((card.recall() - 0.5).abs() < 1e-9);
        assert!((card.defence_pushback() - 0.3).abs() < 1e-9);

        // An empty case must not divide by zero or claim a perfect score it
        // did not earn.
        let empty = Scorecard::default();
        assert_eq!(empty.recall(), 0.0);
        assert_eq!(empty.defence_pushback(), 0.0);
    }

}
