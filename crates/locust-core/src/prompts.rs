//! Prompt construction for each phase.
//!
//! The adversarial structure lives here. Three things do the real work:
//! attackers must state falsifiable claims (a claim that cannot be refuted
//! cannot be scored), defenders are rewarded for killing findings rather than
//! agreeing, and the judge never sees who raised what.

use crate::config::Config;
use crate::context::ContextPack;
use crate::model::{Altitude, Finding, Fitness, Judge, Persona, Rebuttal, Role, Ruling, Verdict};
use crate::traits::Voice;
use std::fmt::Write as _;

/// Rules every attacker obeys regardless of persona.
const ATTACK_RULES: &str = "\
You are one of several independent reviewers competing on the same change. \
Other reviewers are looking from different angles, and a defender will try to \
destroy everything you submit. You are scored on findings that survive that \
defender and an impartial judge; every finding of yours that is refuted costs \
you more than an unraised finding would have.

This means:

- Raise the finding you can prove, not the finding you suspect. Two solid \
findings beat nine speculative ones.
- State each one as a claim that could be shown false. \"This could be \
clearer\" cannot be refuted and scores nothing. \"If `shutdown()` runs while \
`flush()` is between lines 40 and 44, the buffered batch is dropped\" can be.
- Give the concrete failure: the inputs, the interleaving, the sequence of \
calls. If you cannot describe how it goes wrong, you do not have a finding.
- Quote the code you are talking about. Your evidence is what the judge reads \
first.
- Do not report formatting, naming, or anything a linter or formatter would \
catch. Do not restate what the change does. Do not praise it.
- If the change is sound from your angle, return an empty findings list. That \
is a perfectly good result and scores better than noise.";

const FINDING_SCHEMA: &str = r#"Respond with JSON only, no prose around it:

{
  "findings": [
    {
      "file": "path/as/it/appears/in/the/diff.rs",
      "line": 128,
      "category": "concurrency",
      "title": "one line, under 80 chars, the claim itself",
      "claim": "What is wrong, stated as a falsifiable assertion about this code.",
      "failure_scenario": "Concrete inputs or interleaving -> the wrong outcome.",
      "evidence": "Quoted code and/or the call site that proves it.",
      "severity": "low | medium | high | critical",
      "altitude": "line | local | module | architectural",
      "confidence": 0.0
    }
  ]
}

`altitude` is how far from a single line the problem lives:
  line           - visible in the line itself (off-by-one, wrong operator)
  local          - needs the surrounding function to see
  module         - spans files or an interface contract
  architectural  - the code is locally correct but the design is wrong

Be honest about `confidence` (0.0-1.0). The judge is told what you claimed."#;

/// How hard this generation pushes reviewers away from line-level work.
fn altitude_directive(generation: u32, total: u32, pressure: f64) -> String {
    if total <= 1 {
        return String::new();
    }
    let weights = Altitude::all()
        .iter()
        .map(|a| format!("{a} ×{:.2}", a.weight(pressure)))
        .collect::<Vec<_>>()
        .join(", ");

    if generation == 0 {
        format!(
            "This is round 1 of {total}. Everything true is worth raising; later rounds will \
             push upward. Current scoring weight by altitude: {weights}."
        )
    } else {
        format!(
            "This is round {} of {total}, and the scoring weight has shifted sharply toward \
             structural problems: {weights}.\n\nThe obvious line-level defects have already been \
             raised and settled by earlier rounds (see below) — restating them is penalised, and \
             at this weight a line-level finding barely scores even when it is correct. What is \
             wanted now is the problem that is only visible once you stop reading the diff and \
             start reading the system: a responsibility in the wrong place, two sources of truth, \
             an invariant nobody owns, a pattern this change entrenches that the next three \
             changes will have to work around.",
            generation + 1
        )
    }
}

/// A built system/user pair, ready to hand to a provider. Every phase returns
/// one of these.
pub struct Prompt {
    pub system: String,
    pub user: String,
}

/// Build an attacker's prompt for one generation.
///
/// Composed in a fixed order so the pieces cannot fight each other: the
/// persona's own prose first, then its temperament, then the rules every
/// attacker obeys, then the output schema. The persona says *what* to hunt and
/// the rules say *what counts as a finding*; putting the rules last means a
/// persona cannot talk its way out of needing falsifiable claims.
///
/// `settled` is every finding earlier generations already argued out, handed
/// over as a do-not-raise list. Together with `pressure` — which drives the
/// altitude directive — this is what stops later generations re-reporting the
/// same easy defects.
///
/// `has_workspace` switches the context pack between carrying full file
/// contents (for API providers, which cannot open anything) and telling the
/// agent to go read the repository itself.
pub fn attack(
    persona: &Persona,
    pack: &ContextPack,
    settled: &[SettledFinding],
    generation: u32,
    total_generations: u32,
    pressure: f64,
    has_workspace: bool,
) -> Prompt {
    let system = format!(
        "{}{}\n\n---\n\n{ATTACK_RULES}\n\n---\n\n{FINDING_SCHEMA}",
        persona.prompt.trim(),
        temperament(&persona.traits.render(Voice::Reviewer))
    );

    let mut user = String::new();
    let _ = writeln!(user, "{}", pack.render(!has_workspace));

    if has_workspace {
        let _ = writeln!(
            user,
            "\n### Your workspace\n\nYou are running inside a private checkout of the target \
             revision. Read any file you need and grep for call sites — the context pack above is \
             a summary, not the whole repository, and the guard that decides whether a finding is \
             real is often outside it. If you have been granted a tool that can execute, running \
             the suite to demonstrate a failure makes a finding very hard to refute. Do not modify \
             tracked files."
        );
    }

    let directive = altitude_directive(generation, total_generations, pressure);
    if !directive.is_empty() {
        let _ = writeln!(user, "\n### This round\n\n{directive}");
    }

    if !settled.is_empty() {
        let _ = writeln!(
            user,
            "\n### Already settled — do not raise these again\n\nEarlier rounds argued these out. \
             Repeating one, or a restatement of one in different words, is scored as stale and \
             costs you points. Use them as a map of where the obvious ground has been covered."
        );
        for s in settled {
            let _ = writeln!(
                user,
                "- [{}] `{}` — {} _(ruled {})_",
                s.verdict, s.location, s.title, s.verdict
            );
        }
    }

    let _ = writeln!(
        user,
        "\nReview the change from your angle and return the JSON object. Nothing else."
    );

    Prompt { system, user }
}

/// Compact record of a finding earlier generations already argued out.
pub struct SettledFinding {
    pub location: String,
    pub title: String,
    pub verdict: Verdict,
}

const DEFEND_RULES: &str = "\
Several reviewers have attacked this change. You are scored on refutations the \
judge agrees with — so a rebuttal that overstates its case loses you more than \
staying quiet would.

For each finding, pick exactly one stance:

  refute  - the claim is false, unreachable, already prevented elsewhere, or \
not actually a failure. Say precisely which, and quote what proves it.
  narrow  - the underlying problem is real but the claim overstates its \
severity or blast radius. Give the accurate version.
  concede - the claim is correct as stated. Say so in one line and move on.

Conceding a true finding costs you nothing. Bluffing on a true one costs you \
the judge's trust on every other rebuttal in this round. Quote code; an \
unsupported assertion that 'this is handled upstream' will be read as a bluff \
unless you show the upstream handling.";

const REBUTTAL_SCHEMA: &str = r#"Respond with JSON only:

{
  "rebuttals": [
    {
      "finding_id": "exactly as given",
      "stance": "refute | narrow | concede",
      "argument": "Why, in a few sentences.",
      "evidence": "Quoted code, call site, or test output supporting it.",
      "proposed_severity": "low | medium | high | critical"
    }
  ]
}

Include an entry for every finding you were given."#;

/// Build a defender's prompt for one batch of findings.
///
/// Defenders are scored on refutations the judge agrees with, so the rules
/// push hard against bluffing: conceding a true finding is free, and an
/// unsupported "this is handled upstream" costs credibility on every other
/// rebuttal in the same response.
///
/// Batching matters. A defender given forty findings skims; one given eight
/// reasons. The batch size is `[arena] judge_batch`.
pub fn defend(
    persona: &Persona,
    pack: &ContextPack,
    findings: &[&Finding],
    has_workspace: bool,
) -> Prompt {
    let system = format!(
        "{}{}\n\n---\n\n{DEFEND_RULES}\n\n---\n\n{REBUTTAL_SCHEMA}",
        persona.prompt.trim(),
        temperament(&persona.traits.render(Voice::Reviewer))
    );

    let mut user = String::new();
    let _ = writeln!(user, "{}", pack.render(!has_workspace));

    if has_workspace {
        let _ = writeln!(
            user,
            "\n### Your workspace\n\nYou are in a private checkout of the target revision. The \
             strongest refutation is one you verified rather than argued: grep for the guard the \
             attacker missed, or read the caller that makes their scenario unreachable."
        );
    }

    let _ = writeln!(user, "\n### Findings to answer\n");
    for f in findings {
        let _ = writeln!(user, "---\n");
        let _ = writeln!(user, "**{}** — `{}`", f.id, f.location());
        let _ = writeln!(user, "\n_{}_ (claimed {}, {} altitude, confidence {:.2})\n", f.raw.title, f.raw.severity, f.raw.altitude, f.raw.confidence);
        let _ = writeln!(user, "Claim: {}", f.raw.claim);
        if !f.raw.failure_scenario.trim().is_empty() {
            let _ = writeln!(user, "\nFailure scenario: {}", f.raw.failure_scenario);
        }
        if !f.raw.evidence.trim().is_empty() {
            let _ = writeln!(user, "\nEvidence offered:\n```\n{}\n```", f.raw.evidence.trim());
        }
        if let Some(note) = f.grounding.note() {
            let _ = writeln!(
                user,
                "\n_Automated check: {note}._ Confirm this against the file yourself — a \
                 citation that does not hold up is the easiest refutation there is, but a \
                 drifted line number can still sit on top of a real defect."
            );
        }
    }

    let _ = writeln!(user, "\n---\n\nAnswer every finding above. Return the JSON object only.");
    Prompt { system, user }
}

const JUDGE_RULES: &str = "\
You are adjudicating a code review conducted as a contest. Reviewers raised \
findings; defenders tried to destroy them. You decide what an engineer should \
actually be shown.

You are not a participant and you have no stake in either side winning. You do \
not know which model or reviewer produced which text, and you must not try to \
infer it.

For each finding, rule:

  confirmed - the claim is true of this code, reachable, and worth an \
engineer's attention. The defence failed to dispose of it.
  refuted   - the claim is false, unreachable, already handled, or not a real \
failure. Also use this for true-but-worthless findings: style dressed up as a \
defect, or a hypothetical that this system's actual constraints rule out.
  undecided - genuinely unresolvable without information neither side had. Use \
this sparingly; it is not a way to avoid deciding between two confident \
arguments.

Then set these yourself — do not inherit the attacker's self-assessment:

- `severity`: the real consequence if this ships, not what was claimed.
- `altitude`: where the problem actually lives. Be strict. A finding dressed \
in architectural language that reduces to one wrong line is `line`. Promote to \
`architectural` only when the code could be locally correct and the design \
would still be wrong.
- `novel`: false if this restates something in the already-settled list, even \
in different words.
- `duplicate_of`: if two cases in this batch are the same defect described from \
different angles, rule on the better-argued one and set `duplicate_of` on the \
other to its id. Reviewers work independently and routinely reach one bug by \
different routes — 'X deletes from A but not B' and 'C reads B and assumes A' \
can be one defect. Leave it null unless you are confident; merging two distinct \
defects hides one of them completely.

An unsupported refutation ('this is handled upstream', with no upstream shown) \
does not defeat a specific, evidenced finding. An attacker's confident tone is \
not evidence either. Weigh what was actually demonstrated.";

const RULING_SCHEMA: &str = r#"Respond with JSON only:

{
  "rulings": [
    {
      "finding_id": "exactly as given",
      "verdict": "confirmed | refuted | undecided",
      "severity": "low | medium | high | critical",
      "altitude": "line | local | module | architectural",
      "reasoning": "Two or three sentences. What decided it.",
      "novel": true,
      "duplicate_of": null
    }
  ]
}

Rule on every finding you were given, exactly once each."#;

/// Build one judge's prompt for a batch of contested findings.
///
/// The judge never learns who raised what, or which model did — authorship is
/// stripped from the case file because models prefer their own output and
/// would otherwise be grading a vendor rather than a claim.
///
/// Each judge on the panel gets an identical case file and is not told the
/// others exist. That is deliberate: the panel's value is three independent
/// readings, and a judge that knew it was one of three could defer ("the
/// others will catch it") or posture. Their temperament is appended *after*
/// the shared rules, so it reads as calibration applied to a common standard
/// rather than a different job.
pub fn judge(
    pack: &ContextPack,
    cases: &[(&Finding, Vec<&Rebuttal>)],
    settled: &[SettledFinding],
    judge: &Judge,
) -> Prompt {
    // The temperament goes *after* the shared rules: it is calibration applied
    // to a standard the whole panel works from, not a different job. A neutral
    // judge gets the rules verbatim and nothing else.
    let system = format!(
        "{JUDGE_RULES}{}\n\n---\n\n{RULING_SCHEMA}",
        temperament(&judge.traits.render(Voice::Judge))
    );

    let mut user = String::new();
    // The judge always gets sources: it has no workspace and must be able to
    // check a quoted guard for itself.
    let _ = writeln!(user, "{}", pack.render(true));

    if !settled.is_empty() {
        let _ = writeln!(user, "\n### Already settled in earlier rounds\n");
        for s in settled {
            let _ = writeln!(user, "- [{}] `{}` — {}", s.verdict, s.location, s.title);
        }
        let _ = writeln!(user, "\nMark a finding `novel: false` if it restates one of these.");
    }

    let _ = writeln!(user, "\n### Cases\n");
    for (f, rebuttals) in cases {
        let _ = writeln!(user, "---\n");
        let _ = writeln!(user, "#### {} — `{}`\n", f.id, f.location());
        let _ = writeln!(user, "**Finding:** {}\n", f.raw.title);
        let _ = writeln!(user, "Claim: {}", f.raw.claim);
        if !f.raw.failure_scenario.trim().is_empty() {
            let _ = writeln!(user, "\nFailure scenario: {}", f.raw.failure_scenario);
        }
        if !f.raw.evidence.trim().is_empty() {
            let _ = writeln!(user, "\nEvidence:\n```\n{}\n```", f.raw.evidence.trim());
        }
        let _ = writeln!(
            user,
            "\nClaimed severity {}, claimed altitude {}, reviewer confidence {:.2}.",
            f.raw.severity, f.raw.altitude, f.raw.confidence
        );
        if let Some(note) = f.grounding.note() {
            let _ = writeln!(
                user,
                "\n**Automated citation check: {note}.** This was checked mechanically against \
                 the target revision, so it is fact rather than argument. Weigh it as evidence \
                 about how carefully this reviewer read, not as an automatic refutation: the \
                 underlying defect may still be real and described in the wrong place."
            );
        }
        if f.corroboration > 1 {
            let _ = writeln!(
                user,
                "\n_Raised independently by {} different providers._ Independent rediscovery is \
                 weak evidence that it is real, but it is not proof — several models share the \
                 same blind spots and the same false alarms.",
                f.corroboration
            );
        }

        if rebuttals.is_empty() {
            let _ = writeln!(user, "\n**Defence:** none offered.");
        } else {
            for r in rebuttals {
                let _ = writeln!(user, "\n**Defence ({:?}):** {}", r.raw.stance, r.raw.argument);
                if !r.raw.evidence.trim().is_empty() {
                    let _ = writeln!(user, "\nSupporting:\n```\n{}\n```", r.raw.evidence.trim());
                }
            }
        }
    }

    let _ = writeln!(user, "\n---\n\nRule on every case. Return the JSON object only.");
    Prompt { system, user }
}

const MUTATE_RULES: &str = "\
You are tuning the reviewer population for an adversarial code-review system.

A reviewer is nothing but a system prompt — that prompt is its entire genome. \
You are given one reviewer's prompt and the scorecard it earned: findings it \
raised, how many survived an adversarial defender and an impartial judge, and \
which specific claims were confirmed or refuted.

Write a revised prompt that would have scored better on this evidence. Reason \
from what actually happened:

- Findings refuted as false or unreachable mean the reviewer needs to verify \
before claiming. Tell it what to check.
- Findings refuted as trivial mean it is aiming too low. Push its attention \
toward problems that need more of the system in view.
- A high refutation rate with few findings means it is guessing; narrow it to \
the territory where it was actually right.
- Strong performance means sharpen what worked rather than broadening. Do not \
turn a specialist into a generalist — the population already has other \
reviewers covering other ground, and a prompt that hunts everything finds \
nothing.

Keep it a distinct point of view with its own obsession. Write it in second \
person, addressed to the reviewer. Do not include output-format instructions, \
JSON schemas, or generic advice about being careful — those are supplied \
separately. 120-250 words.";

/// Build the meta-prompt that rewrites one reviewer's prose.
///
/// The reviewer's system prompt is its genome, and this is the mutation
/// operator. It is given the persona's own scorecard and the specific claims
/// that were confirmed or refuted, so the rewrite is argued from evidence
/// rather than taste — a high refutation rate means "narrow it to where it was
/// right", not "try harder".
///
/// Temperament is deliberately *not* in scope here: traits are inherited
/// unchanged so a reviewer calibrated by hand keeps that calibration across
/// generations instead of drifting under selection.
pub fn mutate(persona: &Persona, fitness: &Fitness, evidence: &str, pressure: f64) -> Prompt {
    let mut user = String::new();
    let _ = writeln!(user, "## Reviewer: {}\n", persona.name);
    let _ = writeln!(user, "Focus on record: {}\n", persona.focus);
    let _ = writeln!(user, "### Current prompt\n\n{}\n", persona.prompt.trim());
    let _ = writeln!(
        user,
        "### Scorecard\n\nraised {}, confirmed {}, refuted {}, undecided {}, duplicates {}\n\
         precision {:.0}%, mean altitude {:.2}, score {:.2}",
        fitness.raised,
        fitness.confirmed,
        fitness.refuted,
        fitness.undecided,
        fitness.duplicates,
        fitness.precision * 100.0,
        fitness.mean_altitude,
        fitness.score
    );
    let _ = writeln!(user, "\n### What it actually claimed\n\n{evidence}");
    let _ = writeln!(
        user,
        "\n### Where scoring is heading\n\nAltitude weights for the next round: {}. \
         Line-level work is worth {:.2}× and architectural work {:.2}×.",
        Altitude::all()
            .iter()
            .map(|a| format!("{a} ×{:.2}", a.weight(pressure)))
            .collect::<Vec<_>>()
            .join(", "),
        Altitude::Line.weight(pressure),
        Altitude::Architectural.weight(pressure),
    );
    let _ = writeln!(
        user,
        "\nReturn JSON only:\n\n{{\n  \"name\": \"two or three words, lowercase, and \
         different from `{}` — this is a new reviewer, not a new version of that one\",\n  \
         \"focus\": \"one line on what this reviewer hunts\",\n  \
         \"prompt\": \"the revised system prompt\"\n}}",
        persona.name
    );

    Prompt { system: MUTATE_RULES.to_string(), user }
}

const CROSSOVER_RULES: &str = "\
You are breeding reviewers for an adversarial code-review system. A reviewer is \
a system prompt, and you are given two that both scored well on the same change.

Write one new reviewer that inherits the sharpest instinct from each. This is \
not a summary of both and it is not a generalist — a prompt that hunts \
everything finds nothing. Find the angle where their two concerns intersect and \
make that the new reviewer's obsession. If they have nothing in common, take \
the stronger one's method and point it at the weaker one's territory.

Write it in second person, addressed to the reviewer, with its own distinct \
voice. No output-format instructions or JSON schemas — those are supplied \
separately. 120-250 words.";

/// Build the meta-prompt that breeds one reviewer from two winners.
///
/// The instruction is to find where the two parents' concerns *intersect* and
/// make that the child's obsession, rather than summarising both. A prompt
/// that hunts everything finds nothing, and the population already has other
/// reviewers covering other ground.
pub fn crossover(a: &(Persona, Fitness), b: &(Persona, Fitness)) -> Prompt {
    let mut user = String::new();
    for (p, f) in [a, b] {
        let _ = writeln!(
            user,
            "## {} (score {:.2}, precision {:.0}%, mean altitude {:.2})\n\n{}\n",
            p.name,
            f.score,
            f.precision * 100.0,
            f.mean_altitude,
            p.prompt.trim()
        );
    }
    let _ = writeln!(
        user,
        "Return JSON only:\n\n{{\n  \"name\": \"two or three words, lowercase\",\n  \
         \"focus\": \"one line\",\n  \"prompt\": \"the new system prompt\"\n}}"
    );
    Prompt { system: CROSSOVER_RULES.to_string(), user }
}

const SYNTHESIS_RULES: &str = "\
Several rounds of adversarial review produced the confirmed findings below. \
Each one survived a defender trying to destroy it and an impartial judge. Your \
job is to turn that list into something worth a senior engineer's attention.

Findings are symptoms. Group the ones that share a cause, and name the cause. \
Three confirmed findings about forgotten cleanup in three different error paths \
are not three issues — they are one issue about where this code puts its \
cleanup, and that is the thing to report. A single finding that stands alone \
and matters is also a theme; do not force everything into groups.

Lead with the structural problems. Order by what would change an engineer's \
decision about whether to merge, not by how many findings each one absorbed. \
Where a theme has a clear fix, say what it is in one sentence — concretely, \
naming the code, not 'consider refactoring'.

Be willing to say the change is sound: if the confirmed findings are all minor \
and unrelated, report them plainly as a short list rather than inventing a \
narrative that connects them.";

/// Build the final pass that turns surviving findings into themes.
///
/// This is the step that produces what an engineer actually reads. Individual
/// findings are symptoms; three confirmed findings about forgotten cleanup in
/// three error paths are one issue about where this code puts its cleanup.
///
/// Where the run used a panel, each finding carries its tally, so a theme
/// resting mostly on contested findings can be reported as contested instead
/// of being presented as settled.
pub fn synthesize(
    pack: &ContextPack,
    confirmed: &[(&Finding, &Ruling)],
    max_themes: usize,
) -> Prompt {
    let mut user = String::new();
    let _ = writeln!(user, "{}", pack.render(false));
    let _ = writeln!(user, "\n### Confirmed findings\n");
    let mut any_panel = false;
    for (f, r) in confirmed {
        let panel = if r.panel.len() > 1 {
            any_panel = true;
            let (backing, total) = r.tally();
            let dissenters: Vec<&str> = r
                .panel
                .iter()
                .filter(|v| v.raw.verdict != r.raw.verdict)
                .map(|v| v.judge.as_str())
                .collect();
            if dissenters.is_empty() {
                format!(" _(panel {backing}/{total}, unanimous)_")
            } else {
                format!(" _(panel {backing}/{total}; {} disagreed)_", dissenters.join(", "))
            }
        } else {
            String::new()
        };
        let _ = writeln!(
            user,
            "- **{}** `{}` [{} / {}]{panel} — {}\n  {}\n  _judges:_ {}",
            f.id,
            f.location(),
            r.raw.severity,
            r.raw.altitude,
            f.raw.title,
            f.raw.claim,
            r.raw.reasoning.replace('\n', " / ")
        );
    }

    if any_panel {
        let _ = writeln!(
            user,
            "\nEach finding was ruled by a panel of judges with different standards of proof. \
             A unanimous finding cleared all of them; one a judge disagreed about cleared some \
             and not others. Lead with what the panel agreed on, and where a theme rests mostly \
             on contested findings, say so in its summary rather than presenting it as settled."
        );
    }
    let _ = writeln!(
        user,
        "\nReturn at most {max_themes} themes, as JSON only:\n\n\
         {{\n  \"themes\": [\n    {{\n      \"title\": \"one line\",\n      \
         \"severity\": \"low | medium | high | critical\",\n      \
         \"altitude\": \"line | local | module | architectural\",\n      \
         \"summary\": \"what is wrong, one paragraph an engineer can act on\",\n      \
         \"root_cause\": \"the cause these findings are symptoms of\",\n      \
         \"recommendation\": \"one concrete sentence\",\n      \
         \"finding_ids\": [\"f-...\"]\n    }}\n  ]\n}}"
    );
    Prompt { system: SYNTHESIS_RULES.to_string(), user }
}

/// Wrap rendered trait text in a header, or produce nothing at all when the
/// temperament is neutral. A persona that sets no traits must read exactly as
/// it did before traits existed.
fn temperament(rendered: &str) -> String {
    if rendered.trim().is_empty() {
        String::new()
    } else {
        format!("\n\n---\n\n### Your temperament\n\n{rendered}")
    }
}

/// Whether this agent should be given its own checkout, honouring
/// `[run] workspaces`. Under `auto`, only providers that can actually use one
/// get one — handing a worktree to an HTTP provider costs a `git worktree add`
/// and buys nothing.
pub fn wants_workspace(cfg: &Config, provider_needs: bool) -> bool {
    match cfg.run.workspaces {
        crate::config::WorkspacePolicy::Always => true,
        crate::config::WorkspacePolicy::Never => false,
        crate::config::WorkspacePolicy::Auto => provider_needs,
    }
}

/// Stable agent id, used for the worktree directory and run artifacts.
pub fn agent_id(generation: u32, role: Role, persona: &Persona, provider: &str, index: usize) -> String {
    format!(
        "gen{generation}-{}-{}-{}-{index}",
        role.short(),
        crate::config::slug(&persona.name),
        crate::config::slug(provider)
    )
}
