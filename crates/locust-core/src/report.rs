//! Rendering a run for humans.
//!
//! The report leads with themes, because that is the output the tool exists to
//! produce. Individual findings follow as evidence, and the arena's own
//! statistics come last — useful for tuning the config, not for reviewing code.

use crate::config::Config;
use crate::model::{Altitude, Origin, RunOutcome, Verdict};
use std::fmt::Write as _;

pub fn markdown(outcome: &RunOutcome, cfg: &Config) -> String {
    let mut s = String::new();

    let _ = writeln!(s, "# Review `{}` → `{}`\n", short(&outcome.base), short(&outcome.target));
    let _ = writeln!(
        s,
        "_{} · {} generations · {:.1}s · {} in / {} out tokens{}_\n",
        outcome.started_at.format("%Y-%m-%d %H:%M UTC"),
        outcome.generations.len(),
        outcome.elapsed_secs,
        outcome.usage.input_tokens,
        outcome.usage.output_tokens,
        if outcome.usage.cost_usd > 0.0 {
            format!(" · ${:.2}", outcome.usage.cost_usd)
        } else {
            String::new()
        }
    );

    let confirmed = outcome.confirmed_findings();

    if confirmed.is_empty() {
        let _ = writeln!(
            s,
            "## No confirmed findings\n\nEvery finding raised was refuted by a defender and the \
             judge agreed. That is a real result, not an empty one — see the per-generation \
             breakdown below for what was argued."
        );
    }

    if !outcome.themes.is_empty() {
        let _ = writeln!(s, "## What to look at\n");
        for (i, t) in outcome.themes.iter().enumerate() {
            let _ = writeln!(s, "### {}. {} `{}` `{}`\n", i + 1, t.title, t.severity, t.altitude);
            let _ = writeln!(s, "{}\n", t.summary.trim());
            if !t.root_cause.trim().is_empty() {
                let _ = writeln!(s, "**Root cause.** {}\n", t.root_cause.trim());
            }
            if !t.recommendation.trim().is_empty() {
                let _ = writeln!(s, "**Do.** {}\n", t.recommendation.trim());
            }
            if !t.finding_ids.is_empty() {
                let _ = writeln!(s, "<sub>Evidence: {}</sub>\n", t.finding_ids.join(", "));
            }
        }
    }

    if !confirmed.is_empty() {
        let _ = writeln!(s, "## Confirmed findings\n");
        for (f, r) in &confirmed {
            let _ = writeln!(
                s,
                "#### `{}` — {}\n",
                f.location(),
                f.raw.title
            );
            let _ = writeln!(
                s,
                "`{}` · `{}` · raised by _{}_ via `{}`{}\n",
                r.raw.severity,
                r.raw.altitude,
                f.persona,
                f.provider,
                if f.corroboration > 1 {
                    format!(" · found independently by {} providers", f.corroboration)
                } else {
                    String::new()
                }
            );
            let _ = writeln!(s, "{}\n", f.raw.claim.trim());
            if !f.raw.failure_scenario.trim().is_empty() {
                let _ = writeln!(s, "**How it fails.** {}\n", f.raw.failure_scenario.trim());
            }
            if !f.raw.evidence.trim().is_empty() {
                let _ = writeln!(s, "```\n{}\n```\n", f.raw.evidence.trim());
            }
            if !r.raw.reasoning.trim().is_empty() {
                let _ = writeln!(s, "<sub>Judge ({}): {}</sub>\n", r.judge_provider, r.raw.reasoning.trim());
            }
            let _ = writeln!(s, "<sub>`{}`</sub>\n", f.id);
        }
    }

    // What the defenders killed. Worth keeping visible: a reviewer who
    // disagrees with a refutation has found a judge-calibration problem, which
    // is the main thing you tune in this tool.
    let refuted: Vec<_> = outcome
        .generations
        .iter()
        .flat_map(|g| {
            g.rulings.iter().filter(|r| r.raw.verdict == Verdict::Refuted).filter_map(move |r| {
                g.findings.iter().find(|f| f.id == r.raw.finding_id).map(|f| (f, r))
            })
        })
        .collect();

    if !refuted.is_empty() {
        let _ = writeln!(s, "<details>\n<summary>Refuted ({})</summary>\n", refuted.len());
        for (f, r) in &refuted {
            let _ = writeln!(
                s,
                "- `{}` **{}** — {}\n  <sub>{}</sub>",
                f.location(),
                f.raw.title,
                f.persona,
                r.raw.reasoning.trim()
            );
        }
        let _ = writeln!(s, "\n</details>\n");
    }

    let _ = writeln!(s, "## Arena\n");
    let _ = writeln!(s, "| gen | pressure | raised | confirmed | refuted | undecided | best persona |");
    let _ = writeln!(s, "|----:|---------:|-------:|----------:|--------:|----------:|--------------|");
    for g in &outcome.generations {
        let count = |v: Verdict| g.rulings.iter().filter(|r| r.raw.verdict == v).count();
        let best = g
            .fitness
            .first()
            .map(|f| format!("{} ({:.1})", f.persona_name, f.score))
            .unwrap_or_else(|| "—".into());
        let _ = writeln!(
            s,
            "| {} | {:.2} | {} | {} | {} | {} | {best} |",
            g.generation,
            g.pressure,
            g.findings.len(),
            count(Verdict::Confirmed),
            count(Verdict::Refuted),
            count(Verdict::Undecided),
        );
    }

    let _ = writeln!(s, "\n### Altitude by generation\n");
    let _ = writeln!(s, "Confirmed findings, by how far from a single line the problem lived.\n");
    let _ = writeln!(s, "| gen | line | local | module | architectural |");
    let _ = writeln!(s, "|----:|-----:|------:|-------:|--------------:|");
    for g in &outcome.generations {
        let at = |a: Altitude| {
            g.rulings
                .iter()
                .filter(|r| r.raw.verdict == Verdict::Confirmed && r.raw.altitude == a)
                .count()
        };
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} |",
            g.generation,
            at(Altitude::Line),
            at(Altitude::Local),
            at(Altitude::Module),
            at(Altitude::Architectural),
        );
    }

    if cfg.evolution.enabled && outcome.generations.len() > 1 {
        let _ = writeln!(s, "\n### Population\n");
        for g in &outcome.generations {
            let names: Vec<String> = g
                .personas
                .iter()
                .filter(|p| p.role == crate::model::Role::Attacker)
                .map(|p| match p.origin {
                    Origin::Seed => p.name.clone(),
                    o => format!("{} _{:?}_", p.name, o),
                })
                .collect();
            let _ = writeln!(s, "- **gen {}**: {}", g.generation, names.join(", "));
        }
    }

    let _ = writeln!(
        s,
        "\n<sub>Produced by locust. Full transcripts: `.locust/runs/{}/transcripts/`.</sub>",
        outcome.run_id
    );
    s
}

fn short(rev: &str) -> &str {
    &rev[..rev.len().min(12)]
}
