//! Models are asked for JSON and frequently wrap it in prose or a fence.
//! Recovering the payload is worth doing properly: a dropped response is a
//! wasted agent, and in an arena that silently skews the fitness scores.

use anyhow::{bail, Result};
use serde::de::DeserializeOwned;

/// Pull the first well-formed JSON value of the expected shape out of `text`.
///
/// Tries, in order: the whole string, each fenced code block, then a brace- and
/// string-aware scan for balanced `{...}` / `[...]` spans.
pub fn extract_json<T: DeserializeOwned>(text: &str) -> Result<T> {
    let trimmed = text.trim();
    if let Ok(v) = serde_json::from_str::<T>(trimmed) {
        return Ok(v);
    }

    for block in fenced_blocks(trimmed) {
        if let Ok(v) = serde_json::from_str::<T>(block.trim()) {
            return Ok(v);
        }
    }

    for span in balanced_spans(trimmed) {
        if let Ok(v) = serde_json::from_str::<T>(span) {
            return Ok(v);
        }
    }

    bail!(
        "no JSON of the expected shape in model output (first 400 chars): {}",
        trimmed.chars().take(400).collect::<String>()
    )
}

fn fenced_blocks(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        // Skip an optional language tag on the fence line.
        let body_start = after.find('\n').map(|i| i + 1).unwrap_or(after.len());
        let body = &after[body_start..];
        match body.find("```") {
            Some(close) => {
                blocks.push(&body[..close]);
                rest = &body[close + 3..];
            }
            None => {
                blocks.push(body);
                break;
            }
        }
    }
    blocks
}

/// Every balanced `{...}` or `[...]` region, outermost first, ignoring braces
/// that appear inside JSON strings.
fn balanced_spans(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();

    for (start, &b) in bytes.iter().enumerate() {
        if b != b'{' && b != b'[' {
            continue;
        }
        // Only start a scan at a top-level opener to keep this O(n) in practice.
        if spans.iter().any(|s: &&str| {
            let off = s.as_ptr() as usize - text.as_ptr() as usize;
            start > off && start < off + s.len()
        }) {
            continue;
        }

        let mut depth = 0i32;
        let mut in_string = false;
        let mut escaped = false;

        for (i, &c) in bytes.iter().enumerate().skip(start) {
            if in_string {
                if escaped {
                    escaped = false;
                } else if c == b'\\' {
                    escaped = true;
                } else if c == b'"' {
                    in_string = false;
                }
                continue;
            }
            match c {
                b'"' => in_string = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        if let Some(s) = text.get(start..=i) {
                            spans.push(s);
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize, Debug, PartialEq)]
    struct Finding {
        file: String,
        claim: String,
    }

    #[derive(Deserialize, Debug, PartialEq)]
    struct Wrapper {
        findings: Vec<Finding>,
    }

    #[test]
    fn plain_json() {
        let v: Finding = extract_json(r#"{"file":"a.rs","claim":"x"}"#).unwrap();
        assert_eq!(v.file, "a.rs");
    }

    #[test]
    fn fenced_with_preamble() {
        let text = "Sure, here is my review.\n\n```json\n{\"file\":\"a.rs\",\"claim\":\"x\"}\n```\n\nHope that helps!";
        let v: Finding = extract_json(text).unwrap();
        assert_eq!(v.claim, "x");
    }

    #[test]
    fn braces_inside_strings_do_not_confuse_the_scanner() {
        let text =
            r#"Prose. {"file":"a.rs","claim":"the macro expands to vec!{1} \" here"} trailing"#;
        let v: Finding = extract_json(text).unwrap();
        assert_eq!(v.claim, r#"the macro expands to vec!{1} " here"#);
    }

    #[test]
    fn picks_the_span_matching_the_target_shape() {
        let text = r#"{"note":"ignore me"} then {"findings":[{"file":"a.rs","claim":"x"}]}"#;
        let v: Wrapper = extract_json(text).unwrap();
        assert_eq!(v.findings.len(), 1);
    }

    #[test]
    fn no_json_is_an_error_not_a_panic() {
        assert!(extract_json::<Finding>("I could not find any issues.").is_err());
    }
}
