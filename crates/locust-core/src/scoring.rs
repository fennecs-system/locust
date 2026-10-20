//! The fitness function.
//!
//! A persona's score is the sum over the findings it raised, weighted by the
//! judge's severity rating and by the altitude weight *at this point in the
//! run*. The altitude ramp is what makes later generations produce different
//! work rather than more of the same: a line-level finding that scores 1.0 in
//! the first generation scores 0.15 in the last.
//!
//! Refuted findings score negative on purpose. Without a cost for noise, the
//! optimal strategy is to spray low-confidence guesses, and the population
//! converges on exactly the behaviour this tool exists to eliminate.

use crate::config::ScoringConfig;
use crate::model::{Finding, Fitness, Persona, Ruling, Verdict};
use std::collections::HashMap;

pub fn score_generation(
    cfg: &ScoringConfig,
    personas: &[Persona],
    findings: &[Finding],
    rulings: &[Ruling],
    pressure: f64,
) -> Vec<Fitness> {
    let by_id: HashMap<&str, &Ruling> =
        rulings.iter().map(|r| (r.raw.finding_id.as_str(), r)).collect();

    let mut out: Vec<Fitness> = personas
        .iter()
        .map(|p| Fitness {
            persona_id: p.id.clone(),
            persona_name: p.name.clone(),
            generation: p.generation,
            ..Default::default()
        })
        .collect();
    let mut index: HashMap<&str, usize> =
        personas.iter().enumerate().map(|(i, p)| (p.id.as_str(), i)).collect();
    // Defenders raise nothing, so they will not appear below; that is fine.
    index.shrink_to_fit();

    let mut altitude_sum: Vec<f64> = vec![0.0; out.len()];
    let mut altitude_n: Vec<usize> = vec![0; out.len()];

    for f in findings {
        let Some(&i) = index.get(f.persona_id.as_str()) else { continue };
        out[i].raised += 1;

        // Charged whatever the verdict: citing code that is not there is a
        // failure of reading, and a reviewer should not break even on it just
        // because the defect it gestured at happened to be real.
        if !f.grounding.is_sound() {
            out[i].ungrounded += 1;
            out[i].score += cfg.ungrounded_penalty;
        }

        // A duplicate is not judged separately — the judge ruled on the
        // canonical member — but it inherits that verdict. Independently
        // rediscovering something true is worth a little; independently making
        // the same false claim costs the same as making it first. Without the
        // second half, corroborating noise is free while originating it is
        // punished, and the cheapest strategy becomes echoing other reviewers.
        if let Some(canonical) = &f.duplicate_of {
            out[i].duplicates += 1;
            match by_id.get(canonical.as_str()).map(|r| r.raw.verdict) {
                Some(Verdict::Confirmed) => {
                    out[i].confirmed += 1;
                    out[i].score += if by_id[canonical.as_str()].raw.novel {
                        cfg.duplicate
                    } else {
                        cfg.stale_penalty.min(0.0)
                    };
                }
                Some(Verdict::Refuted) => {
                    out[i].refuted += 1;
                    out[i].score += cfg.refuted;
                }
                Some(Verdict::Undecided) => {
                    out[i].undecided += 1;
                    out[i].score += cfg.undecided;
                }
                // Canonical went unjudged; scoring a guess here would let an
                // infrastructure failure shape selection.
                None => {}
            }
            continue;
        }

        let Some(ruling) = by_id.get(f.id.as_str()) else {
            // Unjudged (e.g. a judge call failed). Score nothing rather than
            // guessing, so an infrastructure failure cannot shape selection.
            continue;
        };

        let altitude_w = ruling.raw.altitude.weight(pressure);
        altitude_sum[i] += altitude_w;
        altitude_n[i] += 1;

        match ruling.raw.verdict {
            Verdict::Confirmed => {
                out[i].confirmed += 1;
                let severity_w =
                    if cfg.severity_weighted { ruling.raw.severity.weight() } else { 1.0 };
                // A finding two of three judges refused is worth less than one
                // the whole panel backed. Without this the panel is just a
                // majority vote and the spread it measures is thrown away.
                let consensus = 1.0 - ruling.dissent * cfg.consensus_weight;
                let mut points = cfg.confirmed * severity_w * altitude_w * consensus.max(0.0);
                // Extra providers beyond the first that found it independently.
                points += cfg.corroboration_bonus * f.corroboration.saturating_sub(1) as f64;
                if !ruling.raw.novel {
                    points = cfg.stale_penalty.min(0.0);
                }
                out[i].score += points;
            }
            Verdict::Refuted => {
                out[i].refuted += 1;
                out[i].score += cfg.refuted;
            }
            Verdict::Undecided => {
                out[i].undecided += 1;
                out[i].score += cfg.undecided;
            }
        }
    }

    for (i, f) in out.iter_mut().enumerate() {
        let decided = f.confirmed + f.refuted;
        f.precision = if decided == 0 { 0.0 } else { f.confirmed as f64 / decided as f64 };
        f.mean_altitude =
            if altitude_n[i] == 0 { 0.0 } else { altitude_sum[i] / altitude_n[i] as f64 };
    }

    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// How far through the run we are: 0.0 in the first generation, 1.0 in the
/// last. Drives the altitude ramp.
pub fn pressure(generation: u32, total: u32) -> f64 {
    if total <= 1 {
        1.0
    } else {
        generation as f64 / (total - 1) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Altitude, Origin, RawFinding, RawRuling, Role, Severity};

    fn persona(id: &str) -> Persona {
        Persona {
            id: id.into(),
            name: id.into(),
            role: Role::Attacker,
            prompt: String::new(),
            focus: String::new(),
            temperature: 0.8,
            traits: Default::default(),
            generation: 0,
            parent: None,
            origin: Origin::Seed,
        }
    }

    fn finding(id: &str, persona_id: &str, altitude: Altitude) -> Finding {
        Finding {
            id: id.into(),
            generation: 0,
            persona: persona_id.into(),
            persona_id: persona_id.into(),
            provider: "p".into(),
            raw: RawFinding {
                file: "a.rs".into(),
                line: Some(1),
                category: String::new(),
                title: id.into(),
                claim: String::new(),
                failure_scenario: String::new(),
                evidence: String::new(),
                severity: Severity::Medium,
                altitude,
                confidence: 0.8,
            },
            cluster: id.into(),
            duplicate_of: None,
            corroboration: 1,
            grounding: crate::model::Grounding::Verified,
        }
    }

    fn ruling(id: &str, verdict: Verdict, altitude: Altitude) -> Ruling {
        Ruling {
            raw: RawRuling {
                finding_id: id.into(),
                verdict,
                severity: Severity::Medium,
                altitude,
                reasoning: String::new(),
                novel: true,
                duplicate_of: None,
            },
            judge_provider: "j".into(),
            generation: 0,
            panel: Vec::new(),
            dissent: 0.0,
        }
    }

    fn contested(id: &str, verdict: Verdict, altitude: Altitude, dissent: f64) -> Ruling {
        Ruling { dissent, ..ruling(id, verdict, altitude) }
    }

    #[test]
    fn noise_is_punished_harder_than_silence() {
        let cfg = ScoringConfig::default();
        let ps = vec![persona("noisy"), persona("quiet")];
        let findings = vec![
            finding("f1", "noisy", Altitude::Line),
            finding("f2", "noisy", Altitude::Line),
            finding("f3", "noisy", Altitude::Line),
        ];
        let rulings = vec![
            ruling("f1", Verdict::Refuted, Altitude::Line),
            ruling("f2", Verdict::Refuted, Altitude::Line),
            ruling("f3", Verdict::Refuted, Altitude::Line),
        ];
        let scores = score_generation(&cfg, &ps, &findings, &rulings, 0.0);
        let noisy = scores.iter().find(|f| f.persona_id == "noisy").unwrap();
        let quiet = scores.iter().find(|f| f.persona_id == "quiet").unwrap();
        assert!(noisy.score < quiet.score);
        assert_eq!(quiet.score, 0.0);
    }

    #[test]
    fn altitude_ramp_reverses_who_wins_between_first_and_last_generation() {
        let cfg = ScoringConfig::default();
        let ps = vec![persona("nitpicker"), persona("architect")];
        // The nitpicker lands three true line-level findings; the architect
        // lands one true structural one.
        let findings = vec![
            finding("n1", "nitpicker", Altitude::Line),
            finding("n2", "nitpicker", Altitude::Line),
            finding("n3", "nitpicker", Altitude::Line),
            finding("a1", "architect", Altitude::Architectural),
        ];
        let rulings = vec![
            ruling("n1", Verdict::Confirmed, Altitude::Line),
            ruling("n2", Verdict::Confirmed, Altitude::Line),
            ruling("n3", Verdict::Confirmed, Altitude::Line),
            ruling("a1", Verdict::Confirmed, Altitude::Architectural),
        ];

        let first = score_generation(&cfg, &ps, &findings, &rulings, 0.0);
        assert_eq!(first[0].persona_id, "nitpicker", "volume should win early");

        let last = score_generation(&cfg, &ps, &findings, &rulings, 1.0);
        assert_eq!(last[0].persona_id, "architect", "altitude should win late");
    }

    #[test]
    fn restating_a_settled_finding_is_penalised() {
        let cfg = ScoringConfig::default();
        let ps = vec![persona("p")];
        let findings = vec![finding("f1", "p", Altitude::Local)];
        let mut stale = ruling("f1", Verdict::Confirmed, Altitude::Local);
        stale.raw.novel = false;
        let fresh = ruling("f1", Verdict::Confirmed, Altitude::Local);

        let with_stale = score_generation(&cfg, &ps, &findings, &[stale], 0.5);
        let with_fresh = score_generation(&cfg, &ps, &findings, &[fresh], 0.5);
        assert!(with_stale[0].score < with_fresh[0].score);
    }

    #[test]
    fn settled_findings_never_pay_even_at_maximum_altitude() {
        let cfg = ScoringConfig::default();
        let ps = vec![persona("p"), persona("echo")];
        let mut original = finding("f1", "p", Altitude::Architectural);
        original.corroboration = 3;
        let mut echo = finding("f2", "echo", Altitude::Architectural);
        echo.duplicate_of = Some("f1".into());
        let mut stale = ruling("f1", Verdict::Confirmed, Altitude::Architectural);
        stale.raw.severity = Severity::High;
        stale.raw.novel = false;
        for fitness in score_generation(&cfg, &ps, &[original, echo], &[stale], 1.0) {
            assert!(fitness.score <= 0.0, "{fitness:?}");
            assert_eq!(fitness.confirmed, 1);
        }
    }

    #[test]
    fn lexical_duplicates_follow_judge_merges_for_credit_and_penalties() {
        let cfg = ScoringConfig::default();
        let ps = vec![persona("a"), persona("b"), persona("c")];
        for verdict in [Verdict::Confirmed, Verdict::Refuted] {
            let mut fs = vec![finding("a", "a", Altitude::Local),
                finding("b", "b", Altitude::Local), finding("c", "c", Altitude::Local)];
            fs[0].raw.title = "identical claim about a shared resource".into();
            fs[1].raw.title = fs[0].raw.title.clone();
            fs[2].raw.title = "distinct wording describing another symptom".into();
            crate::dedup::cluster(&mut fs);
            assert_eq!(fs[1].duplicate_of.as_deref(), Some("a"));
            let mut merged = ruling("a", verdict, Altitude::Local);
            merged.raw.duplicate_of = Some("c".into());
            let mut rulings = vec![merged, ruling("c", verdict, Altitude::Local)];
            let demoted = crate::dedup::apply_judge_duplicates(&mut fs, &rulings);
            rulings.retain(|r| !demoted.contains(&r.raw.finding_id));
            assert_eq!(fs[1].duplicate_of.as_deref(), Some("c"));
            let scored = score_generation(&cfg, &ps, &fs, &rulings, 1.0);
            let echo = scored.iter().find(|f| f.persona_id == "b").unwrap();
            match verdict {
                Verdict::Confirmed => assert_eq!(echo.score, cfg.duplicate),
                Verdict::Refuted => assert_eq!(echo.score, cfg.refuted),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn echoing_a_refuted_finding_costs_as_much_as_raising_it() {
        let cfg = ScoringConfig::default();
        let ps = vec![persona("first"), persona("echo")];
        let mut dup = finding("f2", "echo", Altitude::Line);
        dup.duplicate_of = Some("f1".into());
        let findings = vec![finding("f1", "first", Altitude::Line), dup];
        let rulings = vec![ruling("f1", Verdict::Refuted, Altitude::Line)];

        let scores = score_generation(&cfg, &ps, &findings, &rulings, 0.0);
        let first = scores.iter().find(|f| f.persona_id == "first").unwrap();
        let echo = scores.iter().find(|f| f.persona_id == "echo").unwrap();

        assert_eq!(echo.score, first.score);
        assert!(echo.score < 0.0);
        assert_eq!(echo.refuted, 1, "the echo counts against its precision too");
    }

    #[test]
    fn echoing_a_confirmed_finding_pays_less_than_finding_it_first() {
        let cfg = ScoringConfig::default();
        let ps = vec![persona("first"), persona("echo")];
        let mut dup = finding("f2", "echo", Altitude::Local);
        dup.duplicate_of = Some("f1".into());
        let findings = vec![finding("f1", "first", Altitude::Local), dup];
        let rulings = vec![ruling("f1", Verdict::Confirmed, Altitude::Local)];

        let scores = score_generation(&cfg, &ps, &findings, &rulings, 0.0);
        let first = scores.iter().find(|f| f.persona_id == "first").unwrap();
        let echo = scores.iter().find(|f| f.persona_id == "echo").unwrap();

        assert!(echo.score > 0.0);
        assert!(echo.score < first.score, "originating must beat echoing");
    }

    #[test]
    fn unjudged_findings_do_not_move_the_score() {
        let cfg = ScoringConfig::default();
        let ps = vec![persona("p")];
        let findings = vec![finding("f1", "p", Altitude::Local)];
        let scores = score_generation(&cfg, &ps, &findings, &[], 0.5);
        assert_eq!(scores[0].score, 0.0);
        assert_eq!(scores[0].raised, 1);
    }

    #[test]
    fn precision_ignores_undecided() {
        let cfg = ScoringConfig::default();
        let ps = vec![persona("p")];
        let findings = vec![
            finding("f1", "p", Altitude::Local),
            finding("f2", "p", Altitude::Local),
            finding("f3", "p", Altitude::Local),
        ];
        let rulings = vec![
            ruling("f1", Verdict::Confirmed, Altitude::Local),
            ruling("f2", Verdict::Refuted, Altitude::Local),
            ruling("f3", Verdict::Undecided, Altitude::Local),
        ];
        let scores = score_generation(&cfg, &ps, &findings, &rulings, 0.0);
        assert!((scores[0].precision - 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_split_panel_is_worth_less_than_a_unanimous_one() {
        let cfg = ScoringConfig::default();
        let ps = vec![persona("p")];
        let findings = vec![finding("f1", "p", Altitude::Local)];

        let unanimous =
            score_generation(&cfg, &ps, &findings, &[ruling("f1", Verdict::Confirmed, Altitude::Local)], 0.5);
        let split = score_generation(
            &cfg,
            &ps,
            &findings,
            &[contested("f1", Verdict::Confirmed, Altitude::Local, 1.0 / 3.0)],
            0.5,
        );
        assert!(split[0].score < unanimous[0].score);
        assert!(split[0].score > 0.0, "a contested confirmation still counts for something");
    }

    #[test]
    fn consensus_weight_zero_restores_single_judge_behaviour() {
        let cfg = ScoringConfig { consensus_weight: 0.0, ..ScoringConfig::default() };
        let ps = vec![persona("p")];
        let findings = vec![finding("f1", "p", Altitude::Local)];
        let unanimous =
            score_generation(&cfg, &ps, &findings, &[ruling("f1", Verdict::Confirmed, Altitude::Local)], 0.5);
        let split = score_generation(
            &cfg,
            &ps,
            &findings,
            &[contested("f1", Verdict::Confirmed, Altitude::Local, 0.5)],
            0.5,
        );
        assert_eq!(split[0].score, unanimous[0].score);
    }

    #[test]
    fn an_unsubstantiated_citation_is_free_until_you_ask_for_it_to_cost() {
        use crate::model::Grounding;
        let ps = vec![persona("p")];
        let mut f = finding("f1", "p", Altitude::Local);
        f.grounding = Grounding::EvidenceNotFound { quoted_lines: 3 };
        let findings = vec![f];
        let rulings = vec![ruling("f1", Verdict::Confirmed, Altitude::Local)];

        // Off by default: the defender and panel already see the check, and a
        // refuted finding is charged through `refuted`.
        let off = ScoringConfig::default();
        let base = score_generation(&off, &ps, &findings, &rulings, 0.5);
        assert_eq!(base[0].ungrounded, 1, "still counted and reported");

        let on = ScoringConfig { ungrounded_penalty: -2.0, ..ScoringConfig::default() };
        let charged = score_generation(&on, &ps, &findings, &rulings, 0.5);
        assert!(charged[0].score < base[0].score);
        assert!((base[0].score - charged[0].score - 2.0).abs() < 1e-9);
    }

    #[test]
    fn a_sound_citation_is_never_charged() {
        let cfg = ScoringConfig { ungrounded_penalty: -5.0, ..ScoringConfig::default() };
        let ps = vec![persona("p")];
        let findings = vec![finding("f1", "p", Altitude::Local)];
        let rulings = vec![ruling("f1", Verdict::Confirmed, Altitude::Local)];
        let scores = score_generation(&cfg, &ps, &findings, &rulings, 0.5);
        assert_eq!(scores[0].ungrounded, 0);
        assert!(scores[0].score > 0.0);
    }

    #[test]
    fn pressure_spans_the_run() {
        assert_eq!(pressure(0, 3), 0.0);
        assert_eq!(pressure(2, 3), 1.0);
        assert_eq!(pressure(0, 1), 1.0);
    }
}
