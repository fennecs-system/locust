//! The domain: what agents produce, how it is adjudicated, and how the
//! personas that produced it are scored.

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
    #[serde(default)]
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
    #[serde(default)]
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
    #[serde(default)]
    pub rulings: Vec<RawRuling>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ruling {
    #[serde(flatten)]
    pub raw: RawRuling,
    pub judge_provider: String,
    pub generation: u32,
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
    #[serde(default)]
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
