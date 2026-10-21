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

    let _ = writeln!(
        s,
        "# Review `{}` → `{}`\n",
        short(&outcome.base),
        short(&outcome.target)
    );
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

    // Before the findings, not after: these are the reasons to trust the
    // findings less, and a reader who stops after the first screen should
    // still have seen them.
    if !outcome.warnings.is_empty() {
        let _ = writeln!(s, "## Caveats from this run\n");
        for w in &outcome.warnings {
            let _ = writeln!(s, "- {w}");
        }
        let _ = writeln!(s);
    }

    let confirmed = outcome.confirmed_findings();

    if confirmed.is_empty() {
        let _ = writeln!(
            s,
            "## No confirmed findings\n\nNo findings received a confirmed ruling. See the \
             caveats and per-generation breakdown for what was raised and judged."
        );
    }

    if !outcome.themes.is_empty() {
        let _ = writeln!(s, "## What to look at\n");
        for (i, t) in outcome.themes.iter().enumerate() {
            let _ = writeln!(
                s,
                "### {}. {} `{}` `{}`\n",
                i + 1,
                t.title,
                t.severity,
                t.altitude
            );
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
            let _ = writeln!(s, "#### `{}` — {}\n", f.location(), f.raw.title);
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
            if let Some(note) = f.grounding.note() {
                let _ = writeln!(
                    s,
                    "> **Citation check:** {note}. The defect may still be real; the reference to \
                     it did not resolve.\n"
                );
            }
            if !r.panel.is_empty() {
                let (backing, total) = r.tally();
                let verdict = r.raw.verdict;
                let votes: Vec<String> = r
                    .panel
                    .iter()
                    .map(|v| {
                        let mark = if v.raw.verdict == verdict { "" } else { " ✗" };
                        format!("{}{mark} ({}, {})", v.judge, v.raw.verdict, v.provider)
                    })
                    .collect();
                let mut notes = Vec::new();
                if r.severity_spread() > 0 {
                    notes.push(format!("split {} steps on severity", r.severity_spread()));
                }
                if r.altitude_spread() > 0 {
                    notes.push(format!("split {} steps on altitude", r.altitude_spread()));
                }
                let suffix = if notes.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", notes.join(", "))
                };
                let headline = if r.unanimous() {
                    format!("**Panel: unanimous on the verdict ({total}/{total}){suffix}.**")
                } else {
                    format!("**Panel: {backing}/{total}, contested{suffix}.**")
                };
                let _ = writeln!(s, "{headline} {}\n", votes.join(" · "));
            }
            if !r.raw.reasoning.trim().is_empty() {
                let _ = writeln!(
                    s,
                    "<sub>{}</sub>\n",
                    r.raw.reasoning.trim().replace('\n', "<br>")
                );
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
            g.rulings
                .iter()
                .filter(|r| r.raw.verdict == Verdict::Refuted)
                .filter_map(move |r| {
                    g.findings
                        .iter()
                        .find(|f| f.id == r.raw.finding_id)
                        .map(|f| (f, r))
                })
        })
        .collect();

    if !refuted.is_empty() {
        let _ = writeln!(
            s,
            "<details>\n<summary>Refuted ({})</summary>\n",
            refuted.len()
        );
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

    let contested: Vec<_> = outcome
        .generations
        .iter()
        .flat_map(|g| {
            g.rulings
                .iter()
                .filter(|r| r.contested())
                .filter_map(move |r| {
                    g.findings
                        .iter()
                        .find(|f| f.id == r.raw.finding_id)
                        .map(|f| (f, r))
                })
        })
        .collect();

    if !contested.is_empty() {
        let _ = writeln!(s, "## Where the panel split\n");
        let _ = writeln!(
            s,
            "Findings the panel did not fully agree on — the verdict, how serious it is, or how \
             far from a single line it lives. A finding only the strictest judge confirmed is \
             usually a nitpick; one only the lenient judge refuted may be worth a second look; \
             and two judges who agree a defect is real while disagreeing two steps on severity \
             are telling you the blast radius is genuinely unclear. A single judge would have \
             hidden all of it behind one confident verdict.\n"
        );
        for (f, r) in &contested {
            let (backing, total) = r.tally();
            let _ = writeln!(
                s,
                "**`{}`** {} — ruled **{}** {backing}/{total}",
                f.location(),
                f.raw.title,
                r.raw.verdict
            );
            for v in &r.panel {
                let _ = writeln!(
                    s,
                    "- `{}` _{}_ — **{}** ({} / {}, via {}) — {}",
                    v.judge,
                    v.traits.summary(),
                    v.raw.verdict,
                    v.raw.severity,
                    v.raw.altitude,
                    v.provider,
                    v.raw.reasoning.trim()
                );
            }
            let _ = writeln!(s);
        }
    }

    let _ = writeln!(s, "## Arena\n");
    let _ = writeln!(
        s,
        "| gen | pressure | raised | confirmed | refuted | undecided | best persona |"
    );
    let _ = writeln!(
        s,
        "|----:|---------:|-------:|----------:|--------:|----------:|--------------|"
    );
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
    let _ = writeln!(
        s,
        "Confirmed findings, by how far from a single line the problem lived.\n"
    );
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

    // Built from the widest panel seen anywhere in the run, not from whichever
    // ruling happens to be first: a ruling whose judges partly failed would
    // otherwise be reported as the panel's composition.
    {
        let widest = outcome
            .generations
            .iter()
            .flat_map(|g| g.rulings.iter())
            .max_by_key(|r| r.panel.len());
        if let Some(ruling) = widest {
            if ruling.panel.len() > 1 {
                let _ = writeln!(s, "\n### Judging panel\n");
                for v in &ruling.panel {
                    let _ = writeln!(
                        s,
                        "- **{}** `{}` — weight {:.1}",
                        v.judge, v.traits, v.weight
                    );
                }
                let unanimous: usize = outcome
                    .generations
                    .iter()
                    .flat_map(|g| g.rulings.iter())
                    .filter(|r| !r.panel.is_empty() && !r.contested())
                    .count();
                let total: usize = outcome
                    .generations
                    .iter()
                    .flat_map(|g| g.rulings.iter())
                    .filter(|r| !r.panel.is_empty())
                    .count();
                if total > 0 {
                    let _ = writeln!(
                        s,
                        "\n{unanimous} of {total} rulings had the panel in full agreement \
                         ({:.0}%) — same verdict, same severity, same altitude.",
                        unanimous as f64 / total as f64 * 100.0
                    );
                }
            }
        }
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
