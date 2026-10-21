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
pub fn verify(vcs: &dyn Vcs, rev: &str, changed: &[ChangedFile], findings: &mut [Finding]) {
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
            return Grounding::LineOutOfRange {
                cited: line,
                file_lines: lines.len(),
            };
        }
    }

    let evidence = finding.raw.evidence.trim();
    if evidence.is_empty() {
        // Nothing was quoted, so there is nothing to disprove. Not a failure:
        // plenty of real structural findings quote nothing.
        return Grounding::Verified;
    }

    // Two passes, because models quote loosely. The first catches a clean
    // single-line quote; the second catches the common case of several source
    // lines collapsed into one, reindented, or interrupted by an ellipsis.
    let by_line: Vec<String> = lines.iter().map(|l| squash(l)).collect();
    let quoted: Vec<String> = evidence
        .lines()
        .flat_map(unprefixed)
        .map(|l| squash(&l))
        .filter(|l| l.len() >= MIN_EVIDENCE_LINE)
        // Fences and ellipses are formatting the model added, not claims.
        .filter(|l| !l.starts_with("```") && !l.starts_with("..."))
        .collect();

    if quoted.is_empty() {
        return Grounding::Verified;
    }

    if quoted.iter().any(|q| by_line.iter().any(|h| h.contains(q))) {
        return Grounding::Verified;
    }

    // Whitespace carries no meaning in most languages and none at all in a
    // reformatted quote, so compare with it removed entirely and ask only for
    // one distinctive run in common. A reviewer that read the code will share
    // a long run with it; one that invented the code will not.
    let dense_file = strip_ws(text);
    if quoted
        .iter()
        .any(|q| shares_a_run(&strip_ws(q), &dense_file))
    {
        return Grounding::Verified;
    }

    Grounding::EvidenceNotFound {
        quoted_lines: quoted.len(),
    }
}

/// Shortest run of identical, whitespace-free source that counts as having
/// actually read the file. Long enough that boilerplate does not match by
/// accident, short enough to survive an elision mid-quote.
const MIN_SHARED_RUN: usize = 40;

/// Whether `needle` and `hay` share enough contiguous source to show the
/// reviewer read the file.
///
/// A quote shorter than the threshold has no room for an elision, so it has to
/// appear in full. A longer one only needs one distinctive run in common,
/// which is what survives reformatting and a `...` through the middle.
fn shares_a_run(needle: &str, hay: &str) -> bool {
    let chars: Vec<char> = needle.chars().collect();
    if chars.len() < MIN_SHARED_RUN {
        return hay.contains(needle);
    }
    (0..=chars.len() - MIN_SHARED_RUN).any(|i| {
        let window: String = chars[i..i + MIN_SHARED_RUN].iter().collect();
        hay.contains(&window)
    })
}

fn strip_ws(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// A quoted line, plus the same line with a unified-diff marker removed.
///
/// Reviewers are handed a diff and quote straight out of it, so their evidence
/// arrives as `+    let x = 1;` while the file contains `    let x = 1;`.
/// Matching only the literal text reports genuine quotes as invented. Both
/// forms are kept, because a leading `-` is also valid code.
fn unprefixed(line: &str) -> Vec<String> {
    let trimmed = line.trim_start();
    let mut out = vec![line.to_string()];
    if let Some(rest) = trimmed.strip_prefix(['+', '-', '>']) {
        // Not a diff hunk header, which is prose about the file, not from it.
        if !trimmed.starts_with("---") && !trimmed.starts_with("+++") {
            out.push(rest.to_string());
        }
    }
    out
}

fn normalize(path: &str) -> String {
    path.trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
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
        let f = finding(
            "a.rs",
            Some(2),
            "```rust\n        let   scaled = n * 2;\n```",
        );
        assert_eq!(check(SRC, &f), Grounding::Verified);
    }

    #[test]
    fn a_line_past_the_end_of_the_file_is_caught() {
        let f = finding("a.rs", Some(900), "let scaled = n * 2;");
        assert_eq!(
            check(SRC, &f),
            Grounding::LineOutOfRange {
                cited: 900,
                file_lines: 4
            }
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
        assert_eq!(
            check(SRC, &f),
            Grounding::EvidenceNotFound { quoted_lines: 1 }
        );
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

    /// A false positive from the first two-provider run: both reviewers quoted
    /// real code, but collapsed several source lines into one and elided the
    /// middle with `...`. Line-by-line matching saw nothing and the panel was
    /// told the evidence was invented.
    #[test]
    fn a_genuine_quote_reformatted_onto_one_line_is_not_called_invented() {
        let src = "\
            Extract::JsonLinesWhere { path, key, equals } => jsonl(&stdout)\n\
                .filter(|v| dig(v, key).and_then(|k| k.as_str()) == Some(equals.as_str()))\n\
                .filter_map(|v| dig(&v, path).and_then(|v| v.as_str().map(str::to_string)))\n\
                .collect::<Vec<_>>()\n";

        let quoted = "cli.rs: `Extract::JsonLinesWhere { path, key, equals } => \
                      jsonl(&stdout).filter(|v| dig(v, key).and_then(|k| k.as_str()) == \
                      Some(equals.as_str())).filter_map(...).collect::<Vec<_>>()` has no check";

        let f = finding("a.rs", Some(1), quoted);
        assert_eq!(check(src, &f), Grounding::Verified);
    }

    #[test]
    fn invented_code_is_still_caught_by_the_looser_check() {
        let src = "Extract::JsonLinesWhere { path, key, equals } => jsonl(&stdout)\n";
        // Plausible, same vocabulary, but not what the file says.
        let f = finding(
            "a.rs",
            Some(1),
            "self.registry.lookup(&key).and_then(|entry| entry.resolve(path)).unwrap_or_default()",
        );
        assert!(matches!(check(src, &f), Grounding::EvidenceNotFound { .. }));
    }

    /// Raised by two reviewers in the two-provider run, independently: they
    /// are given a unified diff and quote out of it, so their evidence carries
    /// `+` and `-` markers the file does not have.
    #[test]
    fn evidence_quoted_straight_out_of_the_diff_is_not_called_invented() {
        let src = "fn take(n: usize) -> usize {\n    let scaled = n * 2;\n    scaled\n}\n";

        for quoted in [
            "+    let scaled = n * 2;",
            "-    let scaled = n * 2;",
            "> let scaled = n * 2;",
            "```diff\n+    let scaled = n * 2;\n```",
        ] {
            let f = finding("a.rs", Some(2), quoted);
            assert_eq!(check(src, &f), Grounding::Verified, "rejected: {quoted:?}");
        }

        // The marker is not a licence to match anything: invented code with a
        // `+` in front is still invented.
        let f = finding(
            "a.rs",
            Some(2),
            "+    self.registry.insert(key, value.clone());",
        );
        assert!(matches!(check(src, &f), Grounding::EvidenceNotFound { .. }));
    }

    #[test]
    fn short_quotes_must_match_in_full_but_long_ones_may_be_elided() {
        // Under the run threshold: nothing to elide, so it has to be there.
        let short = strip_ws("cache.insert(key, value);");
        assert!(shares_a_run(
            &short,
            &strip_ws("self.cache.insert(key, value); // ok")
        ));
        assert!(!shares_a_run(&short, &strip_ws("self.cache.remove(&key);")));

        // Over the threshold: one distinctive run in common is enough, even
        // though the tail of the quote was never in the file.
        let long = strip_ws(
            "fn reconcile_pending_sessions(&mut self, deadline: Instant) -> Result<usize> { ... }",
        );
        let file = strip_ws(
            "// leading\nfn reconcile_pending_sessions(&mut self, deadline: Instant) -> Result<usize> {\n    todo!()\n}",
        );
        assert!(shares_a_run(&long, &file));
        assert!(!shares_a_run(
            &long,
            &strip_ws("fn something_entirely_different() -> () {}")
        ));
    }

    #[test]
    fn short_fragments_are_not_treated_as_evidence() {
        // `scaled` appears, but matching on it would verify almost anything.
        let f = finding("a.rs", Some(2), "scaled");
        assert_eq!(
            check(SRC, &f),
            Grounding::Verified,
            "too short to judge either way"
        );
    }
}
