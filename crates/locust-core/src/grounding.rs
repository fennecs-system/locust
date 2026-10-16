//! Checking that a finding refers to code that actually exists.
//!
//! A reviewer under pressure to produce findings will confidently cite a line
//! number that is past the end of the file, a path that was never touched, or
//! a snippet of "quoted" code it composed rather than read. Those are the most
//! expensive findings in the system: they read exactly like real ones, and the
//! only thing distinguishing them is whether the citation holds.
//!
//! So locust checks the citation against the target revision before anyone
//! argues about it, and puts the result in front of the defender and the
//! judge. This is deliberately a *verification* step, not a filter — a finding
//! whose line number drifted by two is usually still a real finding, and
//! throwing it away would cost more than flagging it.

use crate::model::{Finding, Grounding};
use locust_vcs::{ChangedFile, Vcs};
use std::collections::HashMap;

/// Shortest quoted line worth matching. Below this, a "match" is noise — every
/// Rust file contains `}` and `let mut x = 0;`.
const MIN_EVIDENCE_LINE: usize = 12;

/// Check every finding against the code it cites.
pub fn verify(
    vcs: &dyn Vcs,
    rev: &str,
    changed: &[ChangedFile],
    findings: &mut [Finding],
) {
    let deleted: Vec<&str> = changed
        .iter()
        .filter(|f| f.status.is_deleted())
        .map(|f| f.path.as_str())
        .collect();

    // One read per distinct file, not per finding.
    let mut cache: HashMap<String, Option<String>> = HashMap::new();

    for finding in findings.iter_mut() {
        let path = normalize(&finding.raw.file);

        let contents = cache
            .entry(path.clone())
            .or_insert_with(|| vcs.show_file(rev, &path).ok().flatten())
            .clone();

        finding.grounding = match contents {
            None => {
                // A finding about a file the change deleted is legitimate.
                if deleted.iter().any(|d| normalize(d) == path) {
                    Grounding::Verified
                } else {
                    Grounding::FileNotFound
                }
            }
            Some(text) => check(&text, finding),
        };
    }
}

fn check(text: &str, finding: &Finding) -> Grounding {
    let lines: Vec<&str> = text.lines().collect();

    if let Some(line) = finding.raw.line {
        // Line numbers are 1-based; 0 means "the model did not really know".
        if line == 0 || line as usize > lines.len() {
            return Grounding::LineOutOfRange { cited: line, file_lines: lines.len() };
        }
    }

    let evidence = finding.raw.evidence.trim();
    if evidence.is_empty() {
        // Nothing was quoted, so there is nothing to disprove. Not a failure:
        // plenty of real structural findings quote nothing.
        return Grounding::Verified;
    }

    let haystack: Vec<String> = lines.iter().map(|l| squash(l)).collect();
    let quoted: Vec<String> = evidence
        .lines()
        .map(squash)
        .filter(|l| l.len() >= MIN_EVIDENCE_LINE)
        // Fences and ellipses are formatting the model added, not claims.
        .filter(|l| !l.starts_with("```") && !l.starts_with("..."))
        .collect();

    if quoted.is_empty() {
        return Grounding::Verified;
    }

    let matched = quoted.iter().filter(|q| haystack.iter().any(|h| h.contains(*q))).count();
    if matched == 0 {
        Grounding::EvidenceNotFound { quoted_lines: quoted.len() }
    } else {
        Grounding::Verified
    }
}

fn normalize(path: &str) -> String {
    path.trim().trim_start_matches("./").trim_start_matches('/').to_string()
}

/// Collapse whitespace so indentation and wrapping do not defeat a match that
/// is otherwise exact.
fn squash(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RawFinding, Severity};

    fn finding(file: &str, line: Option<u32>, evidence: &str) -> Finding {
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
                title: "t".into(),
                claim: "c".into(),
                failure_scenario: String::new(),
                evidence: evidence.into(),
                severity: Severity::Medium,
                altitude: Default::default(),
                confidence: 0.8,
            },
            cluster: "f1".into(),
            duplicate_of: None,
            corroboration: 1,
            grounding: Grounding::Unchecked,
        }
    }

    const SRC: &str = "fn take(n: usize) -> usize {\n    let scaled = n * 2;\n    scaled\n}\n";

    #[test]
    fn a_real_citation_verifies() {
        let f = finding("a.rs", Some(2), "let scaled = n * 2;");
        assert_eq!(check(SRC, &f), Grounding::Verified);
    }

    #[test]
    fn indentation_and_wrapping_do_not_break_a_genuine_quote() {
        let f = finding("a.rs", Some(2), "```rust\n        let   scaled = n * 2;\n```");
        assert_eq!(check(SRC, &f), Grounding::Verified);
    }

    #[test]
    fn a_line_past_the_end_of_the_file_is_caught() {
        let f = finding("a.rs", Some(900), "let scaled = n * 2;");
        assert_eq!(
            check(SRC, &f),
            Grounding::LineOutOfRange { cited: 900, file_lines: 4 }
        );
    }

    #[test]
    fn line_zero_is_not_a_real_citation() {
        let f = finding("a.rs", Some(0), "");
        assert!(matches!(check(SRC, &f), Grounding::LineOutOfRange { .. }));
    }

    #[test]
    fn invented_code_is_caught() {
        let f = finding("a.rs", Some(2), "self.cache.insert(key, value.clone());");
        assert_eq!(check(SRC, &f), Grounding::EvidenceNotFound { quoted_lines: 1 });
    }

    #[test]
    fn quoting_nothing_is_not_held_against_a_structural_finding() {
        let f = finding("a.rs", None, "");
        assert_eq!(check(SRC, &f), Grounding::Verified);
        // Nor is quoting only punctuation and ellipses.
        let f = finding("a.rs", None, "```\n}\n...\n```");
        assert_eq!(check(SRC, &f), Grounding::Verified);
    }

    #[test]
    fn one_genuine_quote_among_elisions_is_enough() {
        // Models routinely interleave real lines with "..." and paraphrase.
        let f = finding(
            "a.rs",
            Some(2),
            "fn take(n: usize) -> usize {\n...\n    // paraphrased by the reviewer\n",
        );
        assert_eq!(check(SRC, &f), Grounding::Verified);
    }

    #[test]
    fn short_fragments_are_not_treated_as_evidence() {
        // `scaled` appears, but matching on it would verify almost anything.
        let f = finding("a.rs", Some(2), "scaled");
        assert_eq!(check(SRC, &f), Grounding::Verified, "too short to judge either way");
    }
}
