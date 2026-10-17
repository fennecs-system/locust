//! The domain: what agents produce, how it is adjudicated, and how the
//! personas that produced it are scored.

use crate::traits::Traits;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn weight(self) -> f64 {
        match self {
            Self::Low => 0.5,
            Self::Medium => 1.0,
            Self::High => 2.0,
            Self::Critical => 3.5,
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        };
        f.write_str(s)
    }
}

/// How far from a single line the problem lives. This is the lever that drags
/// the arena away from nitpicking: its weight in the fitness function ramps up
/// each generation, so line-level findings stop paying rent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Altitude {
    /// A defect visible in the line itself: typo, off-by-one, wrong operator.
    #[default]
    Line,
    /// Needs the surrounding function or file to see.
    Local,
    /// Spans several files or an interface contract within one module.
    Module,
    /// A design problem: the change is locally correct but the shape is wrong.
    Architectural,
}

impl Altitude {
    pub fn all() -> [Altitude; 4] {
        [Self::Line, Self::Local, Self::Module, Self::Architectural]
    }

    fn index(self) -> usize {
        match self {
            Self::Line => 0,
            Self::Local => 1,
            Self::Module => 2,
            Self::Architectural => 3,
        }
    }

    /// Weight at a given point in the run. `pressure` runs 0.0 (first
    /// generation) to 1.0 (last). Early on, anything true is worth finding;
    /// by the end only structural claims score meaningfully.
    pub fn weight(self, pressure: f64) -> f64 {
        const FLAT: [f64; 4] = [1.00, 1.20, 1.50, 1.80];
        const STEEP: [f64; 4] = [0.15, 0.60, 2.00, 4.00];
        let i = self.index();
        let p = pressure.clamp(0.0, 1.0);
        FLAT[i] * (1.0 - p) + STEEP[i] * p
    }
}

impl fmt::Display for Altitude {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Line => "line",
            Self::Local => "local",
            Self::Module => "module",
            Self::Architectural => "architectural",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// The defender failed to dispose of it and the judge believes it.
    Confirmed,
    /// Wrong, already handled elsewhere, or not a real failure.
    Refuted,
    /// Plausible but unproven without information neither side had.
    #[default]
    Undecided,
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Confirmed => "confirmed",
            Self::Refuted => "refuted",
            Self::Undecided => "undecided",
        };
        f.write_str(s)
    }
}

/// What an attacker emits, before locust assigns identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawFinding {
    pub file: String,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub category: String,
    pub title: String,
    /// The defect, stated as a claim that can be shown false.
    pub claim: String,
    /// Concrete inputs or interleaving that produce the wrong behaviour.
    #[serde(default)]
    pub failure_scenario: String,
    /// Quoted code, a test run, a call site: why the attacker believes it.
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub severity: Severity,
    #[serde(default)]
    pub altitude: Altitude,
    #[serde(default = "half")]
    pub confidence: f32,
}

fn half() -> f32 {
    0.5
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AttackReport {
    // Deliberately *not* `#[serde(default)]`. With a default, every JSON
    // object in the output deserializes into this type successfully as an
    // empty report -- so a stray `{"status":"scanning"}` emitted before the
    // real answer wins the span scan and silently discards it. Requiring the
    // key is what makes shape-matching mean anything.
    pub findings: Vec<RawFinding>,
}

/// A finding with identity, provenance and (once judged) a verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub generation: u32,
    pub persona: String,
    pub persona_id: String,
    pub provider: String,
    #[serde(flatten)]
    pub raw: RawFinding,
    /// Cluster of near-identical findings; the canonical member carries the
    /// cluster's id as its own.
    pub cluster: String,
    /// True when another agent in the same cluster was picked as canonical.
    #[serde(default)]
    pub duplicate_of: Option<String>,
    /// Distinct providers that independently raised this cluster. Agreement
    /// across vendors is the main thing multiple providers buys you.
    #[serde(default)]
    pub corroboration: usize,
    /// Whether the code this finding cites actually exists as described.
    #[serde(default)]
    pub grounding: Grounding,
}

/// Result of checking a finding's citation against the target revision.
///
/// Not a verdict on the finding: a drifted line number often accompanies a
/// real defect. It is evidence about how carefully the reviewer read, handed
/// to the defender and the judge so they can weigh it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum Grounding {
    /// Not checked — no revision was available to check against.
    #[default]
    Unchecked,
    /// The cited file, line and quoted code all hold up.
    Verified,
    /// The cited path does not exist at the target revision and was not
    /// deleted by the change.
    FileNotFound,
    /// The cited line is past the end of the file, or is zero.
    LineOutOfRange { cited: u32, file_lines: usize },
    /// None of the substantial lines the reviewer quoted appear in the file.
    EvidenceNotFound { quoted_lines: usize },
}

impl Grounding {
    pub fn is_sound(self) -> bool {
        matches!(self, Grounding::Verified | Grounding::Unchecked)
    }

    /// One line for a prompt or a report, or `None` when there is nothing to
    /// say.
    pub fn note(self) -> Option<String> {
        match self {
            Grounding::Verified | Grounding::Unchecked => None,
            Grounding::FileNotFound => Some(
                "the cited file does not exist at this revision and was not deleted by this change"
                    .into(),
            ),
            Grounding::LineOutOfRange { cited, file_lines } => Some(format!(
                "cites line {cited}, but the file is {file_lines} lines long"
            )),
            Grounding::EvidenceNotFound { quoted_lines } => Some(format!(
                "none of the {quoted_lines} line(s) of code quoted as evidence appear in the file"
            )),
        }
    }
}

impl fmt::Display for Grounding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Grounding::Unchecked => f.write_str("unchecked"),
            Grounding::Verified => f.write_str("verified"),
            Grounding::FileNotFound => f.write_str("file not found"),
            Grounding::LineOutOfRange { .. } => f.write_str("line out of range"),
            Grounding::EvidenceNotFound { .. } => f.write_str("evidence not found"),
        }
    }
}

impl Finding {
    pub fn is_canonical(&self) -> bool {
        self.duplicate_of.is_none()
    }

    pub fn location(&self) -> String {
        match self.raw.line {
            Some(l) => format!("{}:{}", self.raw.file, l),
            None => self.raw.file.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Stance {
    /// The finding is wrong; here is why.
    #[default]
    Refute,
    /// Real, but smaller than claimed.
    Narrow,
    /// Real as stated.
    Concede,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawRebuttal {
    pub finding_id: String,
    #[serde(default)]
    pub stance: Stance,
    pub argument: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub proposed_severity: Option<Severity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DefenseReport {
    // Deliberately *not* `#[serde(default)]`. With a default, every JSON
    // object in the output deserializes into this type successfully as an
    // empty report -- so a stray `{"status":"scanning"}` emitted before the
    // real answer wins the span scan and silently discards it. Requiring the
    // key is what makes shape-matching mean anything.
    pub rebuttals: Vec<RawRebuttal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rebuttal {
    pub persona: String,
    pub persona_id: String,
    pub provider: String,
    #[serde(flatten)]
    pub raw: RawRebuttal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawRuling {
    pub finding_id: String,
    pub verdict: Verdict,
    #[serde(default)]
    pub severity: Severity,
    #[serde(default)]
    pub altitude: Altitude,
    #[serde(default)]
    pub reasoning: String,
    /// False when the finding restates something already settled in an earlier
    /// generation; keeps attackers from farming the same bug forever.
    #[serde(default = "yes")]
    pub novel: bool,
    /// Set when this case restates another case in the same batch. Lexical
    /// clustering collapses obvious repeats before judging, but two reviewers
    /// describing one defect from opposite ends share almost no vocabulary —
    /// the judge is reading both anyway and can see what a token overlap
    /// cannot.
    #[serde(default)]
    pub duplicate_of: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct JudgeReport {
    // Deliberately *not* `#[serde(default)]`. With a default, every JSON
    // object in the output deserializes into this type successfully as an
    // empty report -- so a stray `{"status":"scanning"}` emitted before the
    // real answer wins the span scan and silently discards it. Requiring the
    // key is what makes shape-matching mean anything.
    pub rulings: Vec<RawRuling>,
}

/// One judge's opinion within a panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeVote {
    pub judge: String,
    pub provider: String,
    pub traits: Traits,
    pub weight: f64,
    #[serde(flatten)]
    pub raw: RawRuling,
}

/// The panel's decision on one finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ruling {
    #[serde(flatten)]
    pub raw: RawRuling,
    pub judge_provider: String,
    pub generation: u32,
    /// Every judge's individual vote. A single-judge panel has one entry.
    #[serde(default)]
    pub panel: Vec<JudgeVote>,
    /// Share of panel weight that did **not** back the winning verdict.
    /// `0.0` is unanimous. This is the number that distinguishes a finding
    /// which survived three different standards of proof from one that caught
    /// a single lenient judge.
    #[serde(default)]
    pub dissent: f64,
}

impl Ruling {
    /// Decide a finding from the panel's votes.
    ///
    /// Plurality by weight, ties falling to `undecided` — if the panel is
    /// evenly split between confirming and refuting, that *is* the finding's
    /// status, and pretending otherwise throws away the only thing a panel
    /// buys over a single judge.
    pub fn from_panel(finding_id: &str, generation: u32, votes: Vec<JudgeVote>) -> Option<Self> {
        if votes.is_empty() {
            return None;
        }
        let total: f64 = votes.iter().map(|v| v.weight.max(0.0)).sum();
        if total <= 0.0 {
            return None;
        }

        let weight_for = |verdict: Verdict| -> f64 {
            votes
                .iter()
                .filter(|v| v.raw.verdict == verdict)
                .map(|v| v.weight.max(0.0))
                .sum()
        };
        let confirmed = weight_for(Verdict::Confirmed);
        let refuted = weight_for(Verdict::Refuted);
        let undecided = weight_for(Verdict::Undecided);

        let winner = if confirmed > refuted && confirmed > undecided {
            Verdict::Confirmed
        } else if refuted > confirmed && refuted > undecided {
            Verdict::Refuted
        } else {
            Verdict::Undecided
        };
        let winning_weight = match winner {
            Verdict::Confirmed => confirmed,
            Verdict::Refuted => refuted,
            Verdict::Undecided => undecided,
        };

        // Severity and altitude are taken from the judges who actually carried
        // the verdict; a refuting judge's severity rating is meaningless.
        let backers: Vec<&JudgeVote> =
            votes.iter().filter(|v| v.raw.verdict == winner).collect();
        let backers = if backers.is_empty() { votes.iter().collect() } else { backers };

        let severity = weighted_pick(&backers, |v| severity_index(v.raw.severity), |i| {
            [Severity::Low, Severity::Medium, Severity::High, Severity::Critical][i]
        });
        let altitude = weighted_pick(&backers, |v| altitude_index(v.raw.altitude), |i| {
            [Altitude::Line, Altitude::Local, Altitude::Module, Altitude::Architectural][i]
        });

        // Staleness and duplication are factual observations rather than
        // judgements, so any judge spotting one needs a majority to carry it.
        let novel_against: f64 =
            votes.iter().filter(|v| !v.raw.novel).map(|v| v.weight.max(0.0)).sum();
        let novel = novel_against * 2.0 <= total;

        let duplicate_of = majority_duplicate(&votes, total);

        let reasoning = backers
            .iter()
            .map(|v| format!("{}: {}", v.judge, v.raw.reasoning.trim()))
            .collect::<Vec<_>>()
            .join("\n");

        let judge_provider = backers
            .first()
            .map(|v| v.provider.clone())
            .unwrap_or_else(|| votes[0].provider.clone());

        Some(Self {
            raw: RawRuling {
                finding_id: finding_id.to_string(),
                verdict: winner,
                severity,
                altitude,
                reasoning,
                novel,
                duplicate_of,
            },
            judge_provider,
            generation,
            dissent: (1.0 - winning_weight / total).clamp(0.0, 1.0),
            panel: votes,
        })
    }

    /// True when every judge reached the same *verdict*. Says nothing about
    /// whether they agreed on how bad it is — see [`Ruling::severity_spread`].
    pub fn unanimous(&self) -> bool {
        self.dissent <= f64::EPSILON
    }

    /// How far apart the backing judges were on severity, in enum steps.
    ///
    /// Observed live: a lenient and a strict judge routinely agree that a
    /// finding is real and disagree by two steps on what it costs. Verdict
    /// dissent alone reports that as unanimous, which throws away the thing the
    /// panel actually measured.
    pub fn severity_spread(&self) -> usize {
        self.spread(|v| severity_index(v.raw.severity))
    }

    /// Same, for altitude. An open judge calling something `architectural`
    /// where a literal one calls it `module` is the openness axis doing its job.
    pub fn altitude_spread(&self) -> usize {
        self.spread(|v| altitude_index(v.raw.altitude))
    }

    fn spread(&self, index_of: impl Fn(&JudgeVote) -> usize) -> usize {
        let backing: Vec<usize> = self
            .panel
            .iter()
            .filter(|v| v.raw.verdict == self.raw.verdict)
            .map(index_of)
            .collect();
        match (backing.iter().min(), backing.iter().max()) {
            (Some(lo), Some(hi)) => hi - lo,
            _ => 0,
        }
    }

    /// Whether the panel disagreed about anything at all — the verdict, how
    /// serious it is, or how far from a single line it lives.
    pub fn contested(&self) -> bool {
        self.panel.len() > 1
            && (!self.unanimous() || self.severity_spread() > 0 || self.altitude_spread() > 0)
    }

    /// `2/3` style tally of judges backing the winning verdict.
    pub fn tally(&self) -> (usize, usize) {
        let backing =
            self.panel.iter().filter(|v| v.raw.verdict == self.raw.verdict).count();
        (backing, self.panel.len())
    }
}

fn severity_index(s: Severity) -> usize {
    match s {
        Severity::Low => 0,
        Severity::Medium => 1,
        Severity::High => 2,
        Severity::Critical => 3,
    }
}

fn altitude_index(a: Altitude) -> usize {
    match a {
        Altitude::Line => 0,
        Altitude::Local => 1,
        Altitude::Module => 2,
        Altitude::Architectural => 3,
    }
}

/// Weighted mean of an ordinal rating, rounded back to the enum. Mean rather
/// than majority so a lone `critical` among two `medium`s still pulls upward
/// instead of being discarded.
fn weighted_pick<T>(
    votes: &[&JudgeVote],
    index_of: impl Fn(&JudgeVote) -> usize,
    from_index: impl Fn(usize) -> T,
) -> T {
    let total: f64 = votes.iter().map(|v| v.weight.max(0.0)).sum();
    if total <= 0.0 {
        return from_index(index_of(votes[0]));
    }
    let mean: f64 = votes
        .iter()
        .map(|v| index_of(v) as f64 * v.weight.max(0.0))
        .sum::<f64>()
        / total;
    from_index((mean.round() as usize).min(3))
}

/// A duplicate call only carries with a majority of panel weight behind it:
/// merging two distinct defects hides one of them completely, so a lone judge
/// is not enough.
fn majority_duplicate(votes: &[JudgeVote], total: f64) -> Option<String> {
    let mut tally: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();
    for v in votes {
        if let Some(target) = v.raw.duplicate_of.as_deref() {
            *tally.entry(target).or_default() += v.weight.max(0.0);
        }
    }
    tally
        .into_iter()
        .filter(|(_, w)| *w * 2.0 > total)
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(target, _)| target.to_string())
}

/// A judge on the panel: a provider, a temperament, and how much its vote counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Judge {
    pub name: String,
    #[serde(default)]
    pub traits: Traits,
    /// Pinned provider, or `None` to rotate across providers that did not
    /// author the findings being judged.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default = "unit_weight")]
    pub weight: f64,
}

fn unit_weight() -> f64 {
    1.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Attacker,
    Defender,
}

impl Role {
    pub fn short(self) -> &'static str {
        match self {
            Self::Attacker => "atk",
            Self::Defender => "def",
        }
    }
}

/// How a persona entered the population. Useful for telling whether the
/// evolutionary machinery is actually earning its keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Written by hand in `locust.toml`.
    Seed,
    /// Survived unchanged because it scored well.
    Elite,
    /// A winner rewritten by the meta-prompt.
    Mutant,
    /// Two winners blended.
    Crossover,
    /// Fresh blood drawn from the seed library to fight inbreeding.
    Immigrant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persona {
    pub id: String,
    pub name: String,
    pub role: Role,
    /// The system prompt. This is the genome.
    pub prompt: String,
    /// One line on what this persona hunts; shown in reports.
    #[serde(default)]
    pub focus: String,
    #[serde(default = "default_temp")]
    pub temperature: f32,
    /// Temperament overlaying the prompt. Neutral by default, in which case it
    /// contributes nothing.
    #[serde(default)]
    pub traits: Traits,
    pub generation: u32,
    #[serde(default)]
    pub parent: Option<String>,
    pub origin: Origin,
}

fn default_temp() -> f32 {
    0.8
}

/// What the meta-prompt returns when breeding a new persona.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaSpec {
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub focus: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Fitness {
    pub persona_id: String,
    pub persona_name: String,
    pub generation: u32,
    pub raised: usize,
    pub confirmed: usize,
    pub refuted: usize,
    pub undecided: usize,
    pub duplicates: usize,
    /// Findings whose citation could not be located in the target revision.
    #[serde(default)]
    pub ungrounded: usize,
    /// confirmed / (confirmed + refuted); the share of claims that survived.
    pub precision: f64,
    pub mean_altitude: f64,
    pub score: f64,
}

/// A cluster of confirmed findings sharing a root cause, produced by the
/// synthesis pass. This is the thing an engineer should actually read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    pub title: String,
    pub severity: Severity,
    pub altitude: Altitude,
    /// What is actually wrong, one paragraph.
    pub summary: String,
    /// The underlying cause the individual findings are symptoms of.
    #[serde(default)]
    pub root_cause: String,
    #[serde(default)]
    pub recommendation: String,
    #[serde(default)]
    pub finding_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SynthesisReport {
    // Deliberately *not* `#[serde(default)]`. With a default, every JSON
    // object in the output deserializes into this type successfully as an
    // empty report -- so a stray `{"status":"scanning"}` emitted before the
    // real answer wins the span scan and silently discards it. Requiring the
    // key is what makes shape-matching mean anything.
    pub themes: Vec<Theme>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationOutcome {
    pub generation: u32,
    pub pressure: f64,
    pub personas: Vec<Persona>,
    pub findings: Vec<Finding>,
    pub rebuttals: Vec<Rebuttal>,
    pub rulings: Vec<Ruling>,
    pub fitness: Vec<Fitness>,
    pub usage: locust_provider::Usage,
    pub elapsed_secs: f64,
}

impl GenerationOutcome {
    pub fn confirmed(&self) -> impl Iterator<Item = &Ruling> {
        self.rulings.iter().filter(|r| r.raw.verdict == Verdict::Confirmed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunOutcome {
    pub run_id: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub base: String,
    pub target: String,
    pub vcs: String,
    pub generations: Vec<GenerationOutcome>,
    pub themes: Vec<Theme>,
    /// Everything the run raised that qualifies its own verdicts: a panel that
    /// lost a judge, a generation nothing was refuted in, citations that did
    /// not resolve. Kept here so `report.md`, `report.json` and `--json` carry
    /// them, rather than them scrolling past on stderr and being lost.
    #[serde(default)]
    pub warnings: Vec<String>,
    pub usage: locust_provider::Usage,
    pub elapsed_secs: f64,
}

impl RunOutcome {
    /// Every canonical finding the judge confirmed, paired with its ruling,
    /// ordered by what an engineer should look at first.
    pub fn confirmed_findings(&self) -> Vec<(&Finding, &Ruling)> {
        let mut out: Vec<(&Finding, &Ruling)> = Vec::new();
        for gen in &self.generations {
            for ruling in gen.confirmed() {
                if let Some(f) = gen
                    .findings
                    .iter()
                    .find(|f| f.id == ruling.raw.finding_id && f.is_canonical())
                {
                    out.push((f, ruling));
                }
            }
        }
        out.sort_by(|a, b| {
            let key = |(f, r): &(&Finding, &Ruling)| {
                (r.raw.severity, r.raw.altitude, f.corroboration)
            };
            key(b).cmp(&key(a))
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vote(judge: &str, verdict: Verdict, severity: Severity, weight: f64) -> JudgeVote {
        JudgeVote {
            judge: judge.into(),
            provider: format!("{judge}-provider"),
            traits: Traits::NEUTRAL,
            weight,
            raw: RawRuling {
                finding_id: "f1".into(),
                verdict,
                severity,
                altitude: Altitude::Local,
                reasoning: format!("{judge} reasoning"),
                novel: true,
                duplicate_of: None,
            },
        }
    }

    #[test]
    fn a_unanimous_panel_has_no_dissent() {
        let votes = vec![
            vote("stickler", Verdict::Confirmed, Severity::High, 1.0),
            vote("pragmatist", Verdict::Confirmed, Severity::High, 1.0),
            vote("architect", Verdict::Confirmed, Severity::High, 1.0),
        ];
        let r = Ruling::from_panel("f1", 0, votes).unwrap();
        assert_eq!(r.raw.verdict, Verdict::Confirmed);
        assert!(r.unanimous());
        assert_eq!(r.tally(), (3, 3));
    }

    #[test]
    fn a_finding_only_the_strictest_judge_confirms_is_refuted_but_recorded() {
        let votes = vec![
            vote("stickler", Verdict::Confirmed, Severity::Low, 1.0),
            vote("pragmatist", Verdict::Refuted, Severity::Low, 1.0),
            vote("architect", Verdict::Refuted, Severity::Low, 1.0),
        ];
        let r = Ruling::from_panel("f1", 0, votes).unwrap();
        assert_eq!(r.raw.verdict, Verdict::Refuted);
        assert!((r.dissent - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(r.tally(), (2, 3));
        // The dissenting opinion survives into the report rather than being
        // collapsed away by the verdict.
        assert!(r.panel.iter().any(|v| v.judge == "stickler" && v.raw.verdict == Verdict::Confirmed));
    }

    /// Taken from a live run: `pragmatist` and `architect` both confirmed every
    /// finding, so verdict dissent was zero — yet one rated a defect `medium`
    /// and the other `high`, and one called it `module` where the other said
    /// `architectural`. Reporting that as "unanimous" discards the disagreement
    /// the panel exists to surface.
    #[test]
    fn agreement_on_the_verdict_can_still_hide_disagreement_about_the_cost() {
        let mut lenient = vote("pragmatist", Verdict::Confirmed, Severity::Low, 1.0);
        lenient.raw.altitude = Altitude::Module;
        let mut strict = vote("architect", Verdict::Confirmed, Severity::High, 1.0);
        strict.raw.altitude = Altitude::Architectural;

        let r = Ruling::from_panel("f1", 0, vec![lenient, strict]).unwrap();
        assert!(r.unanimous(), "they agree it is real");
        assert_eq!(r.severity_spread(), 2, "low -> high is two steps");
        assert_eq!(r.altitude_spread(), 1);
        assert!(r.contested(), "and that disagreement must not be hidden");
    }

    #[test]
    fn full_agreement_is_not_contested() {
        let votes = vec![
            vote("a", Verdict::Confirmed, Severity::High, 1.0),
            vote("b", Verdict::Confirmed, Severity::High, 1.0),
        ];
        let r = Ruling::from_panel("f1", 0, votes).unwrap();
        assert!(r.unanimous());
        assert_eq!(r.severity_spread(), 0);
        assert_eq!(r.altitude_spread(), 0);
        assert!(!r.contested());
    }

    #[test]
    fn a_dissenters_severity_does_not_count_toward_the_spread() {
        // The refuter rated it low, but it is not backing the verdict, so its
        // rating is meaningless and must not widen the apparent disagreement.
        let votes = vec![
            vote("a", Verdict::Confirmed, Severity::High, 1.0),
            vote("b", Verdict::Confirmed, Severity::High, 1.0),
            vote("c", Verdict::Refuted, Severity::Low, 1.0),
        ];
        let r = Ruling::from_panel("f1", 0, votes).unwrap();
        assert_eq!(r.severity_spread(), 0);
        assert!(r.contested(), "but the refusal itself is still a disagreement");
    }

    #[test]
    fn a_single_judge_is_never_contested() {
        let votes = vec![vote("solo", Verdict::Confirmed, Severity::High, 1.0)];
        let r = Ruling::from_panel("f1", 0, votes).unwrap();
        assert!(!r.contested(), "one judge cannot disagree with itself");
        assert_eq!(r.severity_spread(), 0);
    }

    #[test]
    fn an_evenly_split_panel_is_undecided_rather_than_arbitrary() {
        let votes = vec![
            vote("a", Verdict::Confirmed, Severity::High, 1.0),
            vote("b", Verdict::Refuted, Severity::High, 1.0),
        ];
        let r = Ruling::from_panel("f1", 0, votes).unwrap();
        assert_eq!(r.raw.verdict, Verdict::Undecided, "a tie must not silently pick a side");
    }

    #[test]
    fn a_single_judge_panel_behaves_exactly_as_before() {
        let votes = vec![vote("solo", Verdict::Confirmed, Severity::Critical, 1.0)];
        let r = Ruling::from_panel("f1", 0, votes).unwrap();
        assert_eq!(r.raw.verdict, Verdict::Confirmed);
        assert_eq!(r.raw.severity, Severity::Critical);
        assert_eq!(r.dissent, 0.0);
        assert!(r.unanimous());
        assert_eq!(r.judge_provider, "solo-provider");
    }

    #[test]
    fn weight_decides_when_the_count_does_not() {
        let votes = vec![
            vote("senior", Verdict::Refuted, Severity::High, 3.0),
            vote("a", Verdict::Confirmed, Severity::High, 1.0),
            vote("b", Verdict::Confirmed, Severity::High, 1.0),
        ];
        let r = Ruling::from_panel("f1", 0, votes).unwrap();
        assert_eq!(r.raw.verdict, Verdict::Refuted);
        assert!((r.dissent - 0.4).abs() < 1e-9);
    }

    #[test]
    fn severity_comes_only_from_the_judges_who_carried_the_verdict() {
        // The refuting judge rated it low; that must not drag the confirmed
        // severity down, because a refuter's severity means nothing.
        let votes = vec![
            vote("a", Verdict::Confirmed, Severity::Critical, 1.0),
            vote("b", Verdict::Confirmed, Severity::Critical, 1.0),
            vote("c", Verdict::Refuted, Severity::Low, 1.0),
        ];
        let r = Ruling::from_panel("f1", 0, votes).unwrap();
        assert_eq!(r.raw.verdict, Verdict::Confirmed);
        assert_eq!(r.raw.severity, Severity::Critical);
    }

    #[test]
    fn a_lone_high_severity_rating_still_pulls_the_mean_upward() {
        let votes = vec![
            vote("a", Verdict::Confirmed, Severity::Critical, 1.0),
            vote("b", Verdict::Confirmed, Severity::Medium, 1.0),
        ];
        let r = Ruling::from_panel("f1", 0, votes).unwrap();
        // mean(Medium=1, Critical=3) = 2 -> High
        assert_eq!(r.raw.severity, Severity::High);
    }

    #[test]
    fn merging_two_findings_needs_a_majority_not_one_opinion() {
        let mut lone = vote("a", Verdict::Confirmed, Severity::High, 1.0);
        lone.raw.duplicate_of = Some("f9".into());
        let votes =
            vec![lone, vote("b", Verdict::Confirmed, Severity::High, 1.0), vote("c", Verdict::Confirmed, Severity::High, 1.0)];
        let r = Ruling::from_panel("f1", 0, votes).unwrap();
        assert_eq!(r.raw.duplicate_of, None, "one judge must not be able to hide a finding");

        let mut x = vote("a", Verdict::Confirmed, Severity::High, 1.0);
        let mut y = vote("b", Verdict::Confirmed, Severity::High, 1.0);
        x.raw.duplicate_of = Some("f9".into());
        y.raw.duplicate_of = Some("f9".into());
        let votes = vec![x, y, vote("c", Verdict::Confirmed, Severity::High, 1.0)];
        let r = Ruling::from_panel("f1", 0, votes).unwrap();
        assert_eq!(r.raw.duplicate_of.as_deref(), Some("f9"));
    }

    #[test]
    fn staleness_also_needs_a_majority() {
        let mut lone = vote("a", Verdict::Confirmed, Severity::High, 1.0);
        lone.raw.novel = false;
        let votes =
            vec![lone, vote("b", Verdict::Confirmed, Severity::High, 1.0), vote("c", Verdict::Confirmed, Severity::High, 1.0)];
        assert!(Ruling::from_panel("f1", 0, votes).unwrap().raw.novel);
    }

    #[test]
    fn an_empty_or_weightless_panel_yields_no_ruling() {
        assert!(Ruling::from_panel("f1", 0, vec![]).is_none());
        let votes = vec![vote("a", Verdict::Confirmed, Severity::High, 0.0)];
        assert!(Ruling::from_panel("f1", 0, votes).is_none());
    }

    #[test]
    fn every_judges_reasoning_that_carried_the_verdict_is_preserved() {
        let votes = vec![
            vote("stickler", Verdict::Confirmed, Severity::High, 1.0),
            vote("architect", Verdict::Confirmed, Severity::High, 1.0),
        ];
        let r = Ruling::from_panel("f1", 0, votes).unwrap();
        assert!(r.raw.reasoning.contains("stickler reasoning"));
        assert!(r.raw.reasoning.contains("architect reasoning"));
    }

    #[test]
    fn altitude_pressure_inverts_the_ranking_of_nitpicks() {
        // Early on a line-level truth is worth more than nothing.
        assert!(Altitude::Line.weight(0.0) > 0.9);
        // By the last generation it is worth almost nothing, while an
        // architectural finding is worth a great deal more than at the start.
        assert!(Altitude::Line.weight(1.0) < 0.2);
        assert!(Altitude::Architectural.weight(1.0) > Altitude::Architectural.weight(0.0) * 2.0);
    }

    #[test]
    fn severity_and_altitude_order_for_ranking() {
        assert!(Severity::Critical > Severity::Low);
        assert!(Altitude::Architectural > Altitude::Line);
    }
}
