//! Clustering near-identical findings.
//!
//! Running eight reviewers across three vendors on the same diff produces the
//! same bug five times in five wordings. Collapsing those matters twice over:
//! the judge should rule on each distinct claim once, and the number of
//! *independent* providers that found a thing is real signal about whether it
//! is there.
//!
//! Deliberately lexical rather than model-based: it runs on every finding of
//! every generation, and a clustering pass that itself costs an LLM call would
//! dominate the budget while introducing its own errors.

use crate::model::{Finding, Ruling};
use std::collections::{HashMap, HashSet};

/// Findings within this many lines of each other may be the same defect.
const LINE_WINDOW: u32 = 12;
/// Token overlap (Jaccard) above which two titles are treated as one claim.
const SIMILARITY: f64 = 0.45;

/// Assign cluster ids, pick a canonical member per cluster, and count how many
/// distinct providers found each one.
pub fn cluster(findings: &mut [Finding]) {
    // Highest confidence first so the canonical member is the best-argued one.
    let mut order: Vec<usize> = (0..findings.len()).collect();
    order.sort_by(|&a, &b| {
        findings[b]
            .raw
            .confidence
            .partial_cmp(&findings[a].raw.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| findings[b].raw.severity.cmp(&findings[a].raw.severity))
    });

    // index -> canonical index
    let mut canonical_of: HashMap<usize, usize> = HashMap::new();
    let mut canonicals: Vec<usize> = Vec::new();

    for &i in &order {
        let found = canonicals.iter().copied().find(|&c| same_defect(&findings[c], &findings[i]));
        match found {
            Some(c) => {
                canonical_of.insert(i, c);
            }
            None => {
                canonical_of.insert(i, i);
                canonicals.push(i);
            }
        }
    }

    // Distinct providers per cluster.
    let mut providers: HashMap<usize, HashSet<String>> = HashMap::new();
    for (&i, &c) in &canonical_of {
        providers.entry(c).or_default().insert(findings[i].provider.clone());
    }

    let cluster_ids: HashMap<usize, String> = canonicals
        .iter()
        .map(|&c| (c, findings[c].id.clone()))
        .collect();

    for i in 0..findings.len() {
        let c = canonical_of[&i];
        let corroboration = providers.get(&c).map(|p| p.len()).unwrap_or(1);
        findings[i].cluster = cluster_ids[&c].clone();
        findings[i].corroboration = corroboration;
        findings[i].duplicate_of = if c == i { None } else { Some(findings[c].id.clone()) };
    }
}

/// Fold the judge's own duplicate calls into the clustering, after it has
/// ruled. Lexical clustering runs first because it is free and saves judge
/// tokens; this catches what it cannot see.
///
/// Returns the ids of findings demoted to duplicates, so their now-redundant
/// rulings can be dropped — otherwise one defect is counted twice in every
/// total the report prints.
pub fn apply_judge_duplicates(findings: &mut [Finding], rulings: &[Ruling]) -> HashSet<String> {
    let claimed: HashMap<&str, &str> = rulings
        .iter()
        .filter_map(|r| {
            r.raw.duplicate_of.as_deref().map(|target| (r.raw.finding_id.as_str(), target))
        })
        // A case cannot be a duplicate of itself, however the judge phrases it.
        .filter(|(id, target)| id != target)
        .collect();
    if claimed.is_empty() {
        return HashSet::new();
    }

    // Owned keys: the mutation pass below needs `findings` mutably while this
    // lookup is still live.
    let index: HashMap<String, usize> =
        findings.iter().enumerate().map(|(i, f)| (f.id.clone(), i)).collect();

    // Follow chains (A→B→C) to the end, refusing to loop. The judge can emit
    // a cycle, and a cycle would otherwise erase every finding in it.
    let resolve = |start: &str| -> Option<String> {
        let mut seen = HashSet::new();
        let mut current = start;
        loop {
            if !seen.insert(current.to_string()) {
                return None;
            }
            match claimed.get(current) {
                Some(next) if index.contains_key(*next as &str) => current = next,
                Some(_) => return None,
                None => return Some(current.to_string()),
            }
        }
    };

    let mut demoted = HashSet::new();
    let mut assignments: Vec<(usize, String)> = Vec::new();
    for id in claimed.keys() {
        let Some(&i) = index.get(*id) else { continue };
        if !findings[i].is_canonical() {
            continue;
        }
        let Some(target) = resolve(id) else { continue };
        if target == *id {
            continue;
        }
        let Some(&t) = index.get(target.as_str()) else { continue };
        if !findings[t].is_canonical() {
            continue;
        }
        assignments.push((i, target.clone()));
        demoted.insert((*id).to_string());
    }

    for (i, target) in assignments {
        let cluster = match index.get(&target) {
            Some(&t) => findings[t].cluster.clone(),
            None => target.clone(),
        };
        // Everything already pointing at this finding has to follow it across.
        let old_cluster = findings[i].cluster.clone();
        for f in findings.iter_mut() {
            if f.cluster == old_cluster {
                f.cluster = cluster.clone();
            }
        }
        findings[i].duplicate_of = Some(target);
    }

    recount_corroboration(findings);
    demoted
}

/// Distinct providers per cluster, recomputed after any re-clustering.
fn recount_corroboration(findings: &mut [Finding]) {
    let mut providers: HashMap<String, HashSet<String>> = HashMap::new();
    for f in findings.iter() {
        providers.entry(f.cluster.clone()).or_default().insert(f.provider.clone());
    }
    for f in findings.iter_mut() {
        f.corroboration = providers.get(&f.cluster).map(|p| p.len()).unwrap_or(1);
    }
}

fn same_defect(a: &Finding, b: &Finding) -> bool {
    if normalize_path(&a.raw.file) != normalize_path(&b.raw.file) {
        return false;
    }
    match (a.raw.line, b.raw.line) {
        // Different regions of a large file are usually different defects,
        // unless the claims are near-identical anyway.
        (Some(x), Some(y)) if x.abs_diff(y) > LINE_WINDOW => {
            similarity(&a.raw.title, &b.raw.title) > 0.75
        }
        _ => {
            similarity(&a.raw.title, &b.raw.title) >= SIMILARITY
                || similarity(&a.raw.claim, &b.raw.claim) >= SIMILARITY
        }
    }
}

fn normalize_path(p: &str) -> &str {
    p.trim_start_matches("./").trim_start_matches('/')
}

/// Jaccard overlap of content words.
fn similarity(a: &str, b: &str) -> f64 {
    let ta = tokens(a);
    let tb = tokens(b);
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let inter = ta.intersection(&tb).count() as f64;
    let union = ta.union(&tb).count() as f64;
    inter / union
}

/// Words that carry no signal about *which* defect is being described.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "be", "been", "this", "that", "these", "those",
    "it", "its", "in", "on", "at", "to", "of", "for", "with", "and", "or", "but", "not", "no",
    "if", "when", "which", "there", "can", "could", "will", "would", "may", "might", "should",
    "does", "do", "has", "have", "had", "from", "by", "as", "into", "then", "than", "so",
];

fn tokens(s: &str) -> HashSet<String> {
    let stop: HashSet<&str> = STOPWORDS.iter().copied().collect();
    s.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() > 2)
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| !stop.contains(w.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RawFinding, Severity};

    fn finding(id: &str, provider: &str, file: &str, line: Option<u32>, title: &str, conf: f32) -> Finding {
        Finding {
            id: id.into(),
            generation: 0,
            persona: "p".into(),
            persona_id: "p".into(),
            provider: provider.into(),
            raw: RawFinding {
                file: file.into(),
                line,
                category: "x".into(),
                title: title.into(),
                claim: title.into(),
                failure_scenario: String::new(),
                evidence: String::new(),
                severity: Severity::Medium,
                altitude: Default::default(),
                confidence: conf,
            },
            cluster: String::new(),
            duplicate_of: None,
            corroboration: 0,
            grounding: crate::model::Grounding::Verified,
        }
    }

    #[test]
    fn same_bug_from_two_vendors_collapses_and_counts_corroboration() {
        let mut fs = vec![
            finding("f1", "anthropic", "src/pool.rs", Some(40), "connection returned to pool without resetting transaction state", 0.9),
            finding("f2", "openai", "src/pool.rs", Some(44), "transaction state not reset before connection is returned to the pool", 0.7),
        ];
        cluster(&mut fs);
        assert_eq!(fs[0].cluster, fs[1].cluster);
        // Higher confidence wins the canonical slot.
        assert!(fs[0].is_canonical());
        assert_eq!(fs[1].duplicate_of.as_deref(), Some("f1"));
        assert_eq!(fs[0].corroboration, 2);
    }

    #[test]
    fn same_vendor_twice_is_not_corroboration() {
        let mut fs = vec![
            finding("f1", "anthropic", "src/pool.rs", Some(40), "connection returned to pool without resetting transaction state", 0.9),
            finding("f2", "anthropic", "src/pool.rs", Some(41), "connection returned to pool without resetting transaction state", 0.8),
        ];
        cluster(&mut fs);
        assert_eq!(fs[0].corroboration, 1);
    }

    #[test]
    fn different_defects_in_one_file_stay_apart() {
        let mut fs = vec![
            finding("f1", "anthropic", "src/pool.rs", Some(40), "transaction state is not reset on release", 0.9),
            finding("f2", "openai", "src/pool.rs", Some(300), "idle timeout is compared in seconds against a millisecond value", 0.8),
        ];
        cluster(&mut fs);
        assert_ne!(fs[0].cluster, fs[1].cluster);
        assert!(fs[1].is_canonical());
    }

    #[test]
    fn different_files_never_merge() {
        let mut fs = vec![
            finding("f1", "a", "src/pool.rs", Some(40), "state not reset", 0.9),
            finding("f2", "b", "src/queue.rs", Some(40), "state not reset", 0.9),
        ];
        cluster(&mut fs);
        assert_ne!(fs[0].cluster, fs[1].cluster);
    }

    fn ruling_dup(id: &str, target: Option<&str>) -> Ruling {
        use crate::model::{RawRuling, Verdict};
        Ruling {
            raw: RawRuling {
                finding_id: id.into(),
                verdict: Verdict::Confirmed,
                severity: Severity::High,
                altitude: Default::default(),
                reasoning: String::new(),
                novel: true,
                duplicate_of: target.map(str::to_string),
            },
            judge_provider: "j".into(),
            generation: 0,
            panel: Vec::new(),
            dissent: 0.0,
        }
    }

    /// The case the live run exposed: two reviewers describing one defect from
    /// opposite ends share almost no vocabulary, so lexical clustering keeps
    /// them apart and only the judge can see they are the same bug.
    #[test]
    fn judge_merges_what_token_overlap_cannot() {
        let mut fs = vec![
            finding("f1", "anthropic", "src/cache.py", Some(20),
                "expiry in get deletes from entries but not hits so evict raises KeyError", 0.9),
            finding("f2", "openai", "src/cache.py", Some(35),
                "evict can pick a key absent from entries and raise KeyError", 0.8),
        ];
        cluster(&mut fs);
        assert_ne!(fs[0].cluster, fs[1].cluster, "lexical clustering leaves these apart");

        let demoted = apply_judge_duplicates(&mut fs, &[ruling_dup("f2", Some("f1"))]);

        assert_eq!(demoted, HashSet::from(["f2".to_string()]));
        assert_eq!(fs[1].duplicate_of.as_deref(), Some("f1"));
        assert_eq!(fs[0].cluster, fs[1].cluster);
        assert_eq!(fs[0].corroboration, 2, "cross-vendor agreement now visible");
        assert_eq!(fs[1].corroboration, 2);
    }

    #[test]
    fn a_duplicate_cycle_from_the_judge_erases_nothing() {
        let mut fs = vec![
            finding("f1", "a", "x.rs", Some(1), "alpha claim here", 0.9),
            finding("f2", "b", "x.rs", Some(90), "entirely separate beta assertion", 0.8),
        ];
        cluster(&mut fs);
        // The judge says each is a duplicate of the other.
        let demoted = apply_judge_duplicates(
            &mut fs,
            &[ruling_dup("f1", Some("f2")), ruling_dup("f2", Some("f1"))],
        );
        assert!(demoted.is_empty(), "a cycle must be refused, not followed");
        assert!(fs.iter().all(|f| f.is_canonical()), "both findings survive");
    }

    #[test]
    fn a_self_referential_or_unknown_target_is_ignored() {
        let mut fs = vec![finding("f1", "a", "x.rs", Some(1), "alpha claim here", 0.9)];
        cluster(&mut fs);
        let demoted = apply_judge_duplicates(
            &mut fs,
            &[ruling_dup("f1", Some("f1")), ruling_dup("f1", Some("nonexistent"))],
        );
        assert!(demoted.is_empty());
        assert!(fs[0].is_canonical());
    }

    #[test]
    fn a_chain_of_duplicates_collapses_to_one_canonical() {
        let mut fs = vec![
            finding("f1", "a", "x.rs", Some(1), "first distinct wording alpha", 0.9),
            finding("f2", "b", "x.rs", Some(90), "second distinct wording beta", 0.8),
            finding("f3", "c", "x.rs", Some(180), "third distinct wording gamma", 0.7),
        ];
        cluster(&mut fs);
        assert!(fs.iter().all(|f| f.is_canonical()), "all distinct lexically");

        // f3 -> f2 -> f1
        let demoted = apply_judge_duplicates(
            &mut fs,
            &[ruling_dup("f3", Some("f2")), ruling_dup("f2", Some("f1"))],
        );
        assert_eq!(demoted.len(), 2);
        let canonical: Vec<&Finding> = fs.iter().filter(|f| f.is_canonical()).collect();
        assert_eq!(canonical.len(), 1);
        assert_eq!(canonical[0].id, "f1");
        assert_eq!(canonical[0].corroboration, 3, "three vendors reached one defect");
    }

    #[test]
    fn no_duplicate_claims_means_no_work_and_no_change() {
        let mut fs = vec![
            finding("f1", "a", "x.rs", Some(1), "alpha claim here", 0.9),
            finding("f2", "b", "y.rs", Some(1), "beta claim there", 0.8),
        ];
        cluster(&mut fs);
        let before: Vec<_> = fs.iter().map(|f| (f.cluster.clone(), f.corroboration)).collect();
        let demoted = apply_judge_duplicates(&mut fs, &[ruling_dup("f1", None)]);
        assert!(demoted.is_empty());
        let after: Vec<_> = fs.iter().map(|f| (f.cluster.clone(), f.corroboration)).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn every_finding_gets_a_cluster() {
        let mut fs = vec![
            finding("f1", "a", "a.rs", None, "alpha beta gamma", 0.5),
            finding("f2", "b", "b.rs", None, "delta epsilon zeta", 0.5),
            finding("f3", "c", "a.rs", None, "alpha beta gamma delta", 0.4),
        ];
        cluster(&mut fs);
        assert!(fs.iter().all(|f| !f.cluster.is_empty()));
        assert_eq!(fs[2].duplicate_of.as_deref(), Some("f1"));
    }
}
