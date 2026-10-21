//! Temperament, as five numbers.
//!
//! A hand-written prompt says *what* an agent looks for. Traits say *how it
//! holds a position* — how much proof it demands, whether it gives the benefit
//! of the doubt, whether an unlikely failure path worries it. Those are
//! orthogonal, which is why traits overlay the prose rather than replacing it.
//!
//! The payoff is on the judging side. One judge's verdict is unfalsifiable: you
//! cannot tell a finding that is genuinely solid from one that caught a lenient
//! judge on a good day. Rule the same finding with a stickler, a pragmatist and
//! an architect, and the *spread* tells you which it was. A finding only the
//! anxious judge confirms is a nitpick; one all three confirm survived three
//! different standards of proof.
//!
//! OCEAN is used because it is a vocabulary people already have. Four of the
//! five map cleanly onto judging; extraversion is the weak one, and is read
//! here as how much weight the agent gives the debate in front of it versus
//! going back to the source.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Big Five, each in `0.0..=1.0`. `0.5` is neutral and contributes nothing to
/// the prompt — a persona that sets no traits reads exactly as it did before.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Traits {
    pub openness: f64,
    pub conscientiousness: f64,
    pub extraversion: f64,
    pub agreeableness: f64,
    pub neuroticism: f64,
}

impl Default for Traits {
    fn default() -> Self {
        Self::NEUTRAL
    }
}

impl Traits {
    pub const NEUTRAL: Self = Self {
        openness: 0.5,
        conscientiousness: 0.5,
        extraversion: 0.5,
        agreeableness: 0.5,
        neuroticism: 0.5,
    };

    pub fn clamped(self) -> Self {
        Self {
            openness: self.openness.clamp(0.0, 1.0),
            conscientiousness: self.conscientiousness.clamp(0.0, 1.0),
            extraversion: self.extraversion.clamp(0.0, 1.0),
            agreeableness: self.agreeableness.clamp(0.0, 1.0),
            neuroticism: self.neuroticism.clamp(0.0, 1.0),
        }
    }

    pub fn is_neutral(&self) -> bool {
        Axis::ALL
            .iter()
            .all(|a| Band::of(self.get(*a)) == Band::Neutral)
    }

    pub fn get(&self, axis: Axis) -> f64 {
        match axis {
            Axis::Openness => self.openness,
            Axis::Conscientiousness => self.conscientiousness,
            Axis::Extraversion => self.extraversion,
            Axis::Agreeableness => self.agreeableness,
            Axis::Neuroticism => self.neuroticism,
        }
    }

    pub fn set(&mut self, axis: Axis, value: f64) {
        let slot = match axis {
            Axis::Openness => &mut self.openness,
            Axis::Conscientiousness => &mut self.conscientiousness,
            Axis::Extraversion => &mut self.extraversion,
            Axis::Agreeableness => &mut self.agreeableness,
            Axis::Neuroticism => &mut self.neuroticism,
        };
        *slot = value.clamp(0.0, 1.0);
    }

    /// Named archetypes, so nobody has to guess five floats to get started.
    pub fn preset(name: &str) -> Option<Self> {
        let t = match name.trim().to_ascii_lowercase().as_str() {
            "neutral" => Self::NEUTRAL,
            // Nits details, demands the evidence be quoted, assumes the
            // reviewer is wrong until shown otherwise.
            "stickler" => Self {
                openness: 0.25,
                conscientiousness: 0.95,
                extraversion: 0.30,
                agreeableness: 0.15,
                neuroticism: 0.50,
            },
            // "Will this actually hurt anyone?" Judges on plausibility rather
            // than auditing, and dismisses what needs an improbable sequence.
            "pragmatist" => Self {
                openness: 0.50,
                conscientiousness: 0.35,
                extraversion: 0.50,
                agreeableness: 0.50,
                neuroticism: 0.10,
            },
            // Takes a structural argument seriously without a reproduction.
            "architect" => Self {
                openness: 0.95,
                conscientiousness: 0.50,
                extraversion: 0.30,
                agreeableness: 0.50,
                neuroticism: 0.35,
            },
            // Treats a possible failure as an inevitable one.
            "paranoid" => Self {
                openness: 0.50,
                conscientiousness: 0.70,
                extraversion: 0.35,
                agreeableness: 0.50,
                neuroticism: 0.95,
            },
            // Refutes by default; the burden of proof is entirely on the claim.
            "skeptic" => Self {
                openness: 0.30,
                conscientiousness: 0.80,
                extraversion: 0.20,
                agreeableness: 0.05,
                neuroticism: 0.25,
            },
            // Would rather ship a false positive than miss a real defect.
            "advocate" => Self {
                openness: 0.70,
                conscientiousness: 0.50,
                extraversion: 0.50,
                agreeableness: 0.90,
                neuroticism: 0.65,
            },
            _ => return None,
        };
        Some(t)
    }

    pub const PRESETS: &'static [&'static str] = &[
        "neutral",
        "stickler",
        "pragmatist",
        "architect",
        "paranoid",
        "skeptic",
        "advocate",
    ];

    /// Prompt text for the non-neutral traits, in the given voice. Returns an
    /// empty string for a neutral temperament: an agent that sets no traits
    /// must read exactly as it did before, with no filler diluting a prompt
    /// somebody wrote by hand.
    pub fn render(&self, voice: Voice) -> String {
        let mut out = String::new();
        for axis in Axis::ALL {
            let band = Band::of(self.get(axis));
            if band == Band::Neutral {
                continue;
            }
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(describe(axis, band, voice));
        }
        out
    }

    /// Sampling temperature for this temperament.
    ///
    /// A modest, deliberately small effect — the prompt above does the real
    /// work. Openness widens the search, conscientiousness narrows it, which is
    /// the closest honest analogue sampling has to either trait.
    pub fn temperature(&self, base: f32) -> f32 {
        let bias = (self.openness - 0.5) * 0.3 - (self.conscientiousness - 0.5) * 0.3;
        (base + bias as f32).clamp(0.0, 1.0)
    }

    /// One-line summary for reports and `locust personas`.
    pub fn summary(&self) -> String {
        let parts: Vec<String> = Axis::ALL
            .iter()
            .filter(|a| Band::of(self.get(**a)) != Band::Neutral)
            .map(|a| format!("{}{:.0}", a.letter(), self.get(*a) * 100.0))
            .collect();
        if parts.is_empty() {
            "neutral".into()
        } else {
            parts.join(" ")
        }
    }
}

impl fmt::Display for Traits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Openness,
    Conscientiousness,
    Extraversion,
    Agreeableness,
    Neuroticism,
}

impl Axis {
    pub const ALL: [Axis; 5] = [
        Axis::Openness,
        Axis::Conscientiousness,
        Axis::Extraversion,
        Axis::Agreeableness,
        Axis::Neuroticism,
    ];

    pub fn letter(self) -> char {
        match self {
            Axis::Openness => 'O',
            Axis::Conscientiousness => 'C',
            Axis::Extraversion => 'E',
            Axis::Agreeableness => 'A',
            Axis::Neuroticism => 'N',
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Axis::Openness => "openness",
            Axis::Conscientiousness => "conscientiousness",
            Axis::Extraversion => "extraversion",
            Axis::Agreeableness => "agreeableness",
            Axis::Neuroticism => "neuroticism",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    VeryLow,
    Low,
    Neutral,
    High,
    VeryHigh,
}

impl Band {
    /// The neutral band is wide on purpose. A trait set to 0.6 because it felt
    /// vaguely right should not inject a paragraph of personality.
    pub fn of(v: f64) -> Band {
        match v {
            v if v <= 0.20 => Band::VeryLow,
            v if v <= 0.35 => Band::Low,
            v if v >= 0.80 => Band::VeryHigh,
            v if v >= 0.65 => Band::High,
            _ => Band::Neutral,
        }
    }
}

/// Which side of the arena the trait text is written for. The same trait means
/// different things to someone raising a claim and someone ruling on one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Voice {
    Judge,
    Reviewer,
}

fn describe(axis: Axis, band: Band, voice: Voice) -> &'static str {
    match voice {
        Voice::Judge => judge_text(axis, band),
        Voice::Reviewer => reviewer_text(axis, band),
    }
}

fn judge_text(axis: Axis, band: Band) -> &'static str {
    use Axis::*;
    use Band::*;
    match (axis, band) {
        (Openness, VeryHigh) => "You take structural and design arguments seriously even when no single input demonstrates the failure. A claim that the shape is wrong can be true without a reproduction, and the strength of the reasoning is enough to confirm it. Promote altitude readily — if a finding is really about where responsibility sits, say so even when the reviewer framed it as a line.",
        (Openness, High) => "Give real weight to arguments about design and structure, not only to defects you can watch happen.",
        (Openness, Low) => "You want a concrete reproduction. A design critique with no named failure is an opinion, and opinions are refuted.",
        (Openness, VeryLow) => "Rule only on what the code demonstrably does. If no input, interleaving or sequence of calls produces a wrong outcome, there is no finding, however elegant the argument. Keep altitude low: most claims dressed in architectural language are one wrong line wearing a costume.",

        (Conscientiousness, VeryHigh) => "Check every claim against the quoted code yourself, line by line. Evidence that is gestured at rather than shown does not count, and a finding whose quoted code does not actually say what the reviewer claims it says is refuted on that ground alone. Be exact about severity and altitude; an approximation is an error.",
        (Conscientiousness, High) => "Verify claims against the code rather than accepting them, and insist that evidence is actually quoted rather than described.",
        (Conscientiousness, Low) => "Judge on overall plausibility rather than auditing each claim. A finding that is broadly right matters more than one that is precisely argued and irrelevant.",
        (Conscientiousness, VeryLow) => "Go on impression. Does this look like a real problem in a real system? Do not be drawn into verifying details; that is not where the value is.",

        (Extraversion, VeryHigh) => "Engage with the argument as it was made. The debate in front of you is the evidence: a case argued forcefully and answered weakly has been won, and a side that failed to respond to the strongest point against it has lost.",
        (Extraversion, High) => "Weigh how well each side argued its case, not only what the code says.",
        (Extraversion, Low) => "Ignore how confidently either side wrote. Go back to the code and decide for yourself.",
        (Extraversion, VeryLow) => "Discount the debate almost entirely. Rhetoric, confidence and volume are noise. Re-derive the answer from the source and rule on what you find there, even when both sides agree on something else.",

        (Agreeableness, VeryHigh) => "Give the reviewer the benefit of the doubt. Where you are genuinely torn, rule for the finding rather than against it: a false positive costs an engineer five minutes, a false negative ships the bug. Do not sharpen severity to look rigorous.",
        (Agreeableness, High) => "Where the evidence is balanced, lean toward the reviewer rather than the defence.",
        (Agreeableness, Low) => "Assume a finding is wrong until it is shown to be right. The burden sits with the reviewer, and a claim that has not been carried is refuted, not undecided.",
        (Agreeableness, VeryLow) => "Be adversarial. Most findings are wrong and your job is to say so. Confirm nothing that has not been demonstrated beyond argument, and do not soften severity out of politeness.",

        (Neuroticism, VeryHigh) => "Treat uncertainty as danger. If a failure path exists, assume it will be taken — at the worst possible moment, in production, under load. Rare paths cause incidents precisely because nobody thought about them. Rate severity for the bad case, not the typical one.",
        (Neuroticism, High) => "Weight unlikely failure modes more heavily than their raw probability suggests, and lean toward the higher severity when torn.",
        (Neuroticism, Low) => "Stay calm. A failure that needs an improbable sequence to trigger is not an emergency. Rate severity by expected real-world impact.",
        (Neuroticism, VeryLow) => "Dismiss hypotheticals. If a finding depends on a sequence nobody has observed and nothing in the code makes likely, it does not deserve an engineer's attention regardless of whether it is technically possible. Deflate inflated severity aggressively.",

        (_, Neutral) => "",
    }
}

fn reviewer_text(axis: Axis, band: Band) -> &'static str {
    use Axis::*;
    use Band::*;
    match (axis, band) {
        (Openness, VeryHigh) => "Follow the speculative thread. The most valuable thing you can find is the problem nobody has a name for yet, and one uncertain structural claim is worth more than three safe ones.",
        (Openness, High) => "Look past the demonstrable defect to the shape of the change.",
        (Openness, Low) => "Raise only what you can point at in the code.",
        (Openness, VeryLow) => "Claim nothing you cannot demonstrate with a specific input or sequence of calls. Speculation wastes everyone's time, including yours.",

        (Conscientiousness, VeryHigh) => "Be exhaustive and exact. Read every line of the change and every call site it touches. Quote precisely, get the line numbers right, and do not approximate.",
        (Conscientiousness, High) => "Work through the change methodically rather than scanning for what stands out.",
        (Conscientiousness, Low) => "Follow what stands out rather than working through everything in order.",
        (Conscientiousness, VeryLow) => "Trust your first instinct about where the problem is and go straight there.",

        (Extraversion, VeryHigh) => "State your findings with force. Commit to a position and argue it rather than hedging.",
        (Extraversion, High) => "Argue your case rather than qualifying it into vagueness.",
        (Extraversion, Low) => "Understate rather than overstate. Let the evidence carry the claim.",
        (Extraversion, VeryLow) => "Say as little as possible beyond the evidence itself. If the quoted code makes the point, the point is made.",

        (Agreeableness, VeryHigh) => "Assume competence. The author probably had a reason; look for it before concluding something is wrong, and raise the finding only when you genuinely cannot find one.",
        (Agreeableness, High) => "Give the author the benefit of the doubt before raising something.",
        (Agreeableness, Low) => "Assume nothing here was thought through. Every convenience is a shortcut until shown otherwise.",
        (Agreeableness, VeryLow) => "Be hostile to this code. Read every line as though it were written carelessly and in a hurry — the ones that were are exactly what you are looking for.",

        (Neuroticism, VeryHigh) => "Catastrophize deliberately. For every part of this change, ask what the worst thing that could happen is and whether anything actually prevents it. The 3am incident is your subject matter.",
        (Neuroticism, High) => "Attend to what could go wrong more than to what usually does.",
        (Neuroticism, Low) => "Stay grounded in what will realistically happen to this system.",
        (Neuroticism, VeryLow) => "Ignore remote possibilities entirely. Raise only what will actually bite someone.",

        (_, Neutral) => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_traits_say_nothing_at_all() {
        let t = Traits::NEUTRAL;
        assert!(t.is_neutral());
        assert_eq!(t.render(Voice::Judge), "");
        assert_eq!(t.render(Voice::Reviewer), "");
        assert_eq!(t.summary(), "neutral");
    }

    #[test]
    fn a_wide_neutral_band_tolerates_imprecise_tuning() {
        // Someone typing 0.6 because it felt about right should not get a
        // paragraph of personality they did not ask for.
        let mut t = Traits::NEUTRAL;
        t.agreeableness = 0.6;
        t.openness = 0.4;
        assert!(t.is_neutral());
        assert_eq!(t.render(Voice::Judge), "");
    }

    #[test]
    fn only_the_non_neutral_axes_are_rendered() {
        let mut t = Traits::NEUTRAL;
        t.neuroticism = 0.95;
        let text = t.render(Voice::Judge);
        assert!(text.contains("Treat uncertainty as danger"));
        assert!(
            !text.contains("benefit of the doubt"),
            "agreeableness is neutral: {text}"
        );
        assert_eq!(t.summary(), "N95");
    }

    #[test]
    fn judge_and_reviewer_voices_differ_on_the_same_trait() {
        let mut t = Traits::NEUTRAL;
        t.agreeableness = 0.02;
        let j = t.render(Voice::Judge);
        let r = t.render(Voice::Reviewer);
        assert!(!j.is_empty() && !r.is_empty());
        assert_ne!(j, r);
        assert!(j.contains("Confirm nothing"));
        assert!(r.contains("hostile to this code"));
    }

    #[test]
    fn presets_span_the_space_the_user_asked_for() {
        let stickler = Traits::preset("stickler").unwrap();
        let architect = Traits::preset("architect").unwrap();
        // A stubborn judge that nits details.
        assert!(stickler.conscientiousness > 0.8);
        assert!(stickler.agreeableness < 0.3);
        // A lenient, open one.
        assert!(architect.openness > 0.8);
        assert!(architect.openness > stickler.openness);
        assert!(Traits::preset("nonexistent").is_none());
        for name in Traits::PRESETS {
            assert!(Traits::preset(name).is_some(), "{name} must resolve");
        }
    }

    /// A preset carrying a value inside the dead neutral band claims an effect
    /// it does not have. Every axis a preset moves off 0.5 must actually fire.
    #[test]
    fn presets_contain_no_values_that_silently_do_nothing() {
        for name in Traits::PRESETS {
            let t = Traits::preset(name).unwrap();
            for axis in Axis::ALL {
                let v = t.get(axis);
                if v != 0.5 {
                    assert_ne!(
                        Band::of(v),
                        Band::Neutral,
                        "{name}.{} = {v} sits in the neutral band and has no effect; \
                         set it to exactly 0.5 or push it past the band",
                        axis.name()
                    );
                }
            }
        }
    }

    /// Each non-neutral preset has to be distinguishable from the others, or
    /// a panel built from them measures nothing.
    #[test]
    fn presets_are_distinct_from_each_other() {
        let named: Vec<(&str, Traits)> = Traits::PRESETS
            .iter()
            .map(|n| (*n, Traits::preset(n).unwrap()))
            .collect();
        for (i, (na, a)) in named.iter().enumerate() {
            for (nb, b) in named.iter().skip(i + 1) {
                assert_ne!(a, b, "presets `{na}` and `{nb}` are identical");
            }
        }
    }

    #[test]
    fn every_non_neutral_band_has_text_in_both_voices() {
        for axis in Axis::ALL {
            for band in [Band::VeryLow, Band::Low, Band::High, Band::VeryHigh] {
                assert!(
                    !judge_text(axis, band).is_empty(),
                    "judge {axis:?} {band:?}"
                );
                assert!(
                    !reviewer_text(axis, band).is_empty(),
                    "reviewer {axis:?} {band:?}"
                );
            }
        }
    }

    #[test]
    fn bands_are_ordered_and_cover_the_range() {
        assert_eq!(Band::of(0.0), Band::VeryLow);
        assert_eq!(Band::of(0.3), Band::Low);
        assert_eq!(Band::of(0.5), Band::Neutral);
        assert_eq!(Band::of(0.7), Band::High);
        assert_eq!(Band::of(1.0), Band::VeryHigh);
    }

    #[test]
    fn temperature_moves_with_openness_and_against_rigour() {
        let base = 0.5;
        let open = Traits {
            openness: 1.0,
            ..Traits::NEUTRAL
        };
        let rigorous = Traits {
            conscientiousness: 1.0,
            ..Traits::NEUTRAL
        };
        assert!(open.temperature(base) > base);
        assert!(rigorous.temperature(base) < base);
        assert_eq!(Traits::NEUTRAL.temperature(base), base);
        // Never escapes the valid sampling range.
        assert!(open.temperature(1.0) <= 1.0);
        assert!(rigorous.temperature(0.0) >= 0.0);
    }

    #[test]
    fn values_outside_the_range_are_clamped_not_trusted() {
        let t = Traits {
            openness: 5.0,
            neuroticism: -3.0,
            ..Traits::NEUTRAL
        }
        .clamped();
        assert_eq!(t.openness, 1.0);
        assert_eq!(t.neuroticism, 0.0);
    }
}
