//! Breeding the next generation of reviewers.
//!
//! Selection acts on prompts, not weights. Each generation the population is
//! rebuilt from four sources: elites carried over unchanged, mutants rewritten
//! by a meta-prompt in light of their scorecard, crossovers blending two
//! winners, and immigrants drawn from the seed library so the population does
//! not collapse onto one point of view.
//!
//! Breeding failures are never fatal. If the meta-provider errors or returns
//! nonsense, the parent is carried forward — a run that dies because a prompt
//! rewrite failed is worse than one that evolves a little less.

use crate::config::{Config, EvolutionConfig};
use crate::model::{Finding, Fitness, Origin, Persona, PersonaSpec, Role, Ruling, Verdict};
use crate::prompts;
use anyhow::Result;
use locust_provider::{extract_json, Request, SharedProvider, Usage};
use rand::seq::SliceRandom;
use std::collections::HashMap;
use std::fmt::Write as _;

pub struct Brood {
    pub personas: Vec<Persona>,
    pub usage: Usage,
}

// Eight parameters, all of them distinct inputs the meta-prompt needs; bundling
// them into a struct would add a type whose only purpose is to satisfy a lint.
#[allow(clippy::too_many_arguments)]
pub async fn breed(
    cfg: &Config,
    meta: &SharedProvider,
    current: &[Persona],
    fitness: &[Fitness],
    findings: &[Finding],
    rulings: &[Ruling],
    next_generation: u32,
    pressure: f64,
) -> Result<Brood> {
    let ev = &cfg.evolution;
    let target = cfg.arena.attackers.max(1);
    let mut usage = Usage::default();

    // Attackers are the only role under selection: defenders are scored
    // implicitly (a defender that refutes well shrinks everyone's score) but
    // their fitness is not separable from the attackers they faced, so
    // evolving them on this signal would be fitting noise.
    let attackers: Vec<&Persona> = current.iter().filter(|p| p.role == Role::Attacker).collect();
    let fit_by_id: HashMap<&str, &Fitness> =
        fitness.iter().map(|f| (f.persona_id.as_str(), f)).collect();

    let mut ranked: Vec<(&Persona, Fitness)> = attackers
        .iter()
        .map(|p| {
            let f = fit_by_id
                .get(p.id.as_str())
                .cloned()
                .cloned()
                .unwrap_or_else(|| Fitness {
                    persona_id: p.id.clone(),
                    persona_name: p.name.clone(),
                    generation: p.generation,
                    ..Default::default()
                });
            (*p, f)
        })
        .collect();
    ranked.sort_by(|a, b| b.1.score.partial_cmp(&a.1.score).unwrap_or(std::cmp::Ordering::Equal));

    let survivors: Vec<&(&Persona, Fitness)> =
        ranked.iter().filter(|(_, f)| f.score > ev.cull_below).collect();
    // If the cull would empty the population, keep the best regardless: a
    // generation where every persona scored badly is information, not a reason
    // to stop reviewing.
    let pool: Vec<&(&Persona, Fitness)> =
        if survivors.is_empty() { ranked.iter().take(2).collect() } else { survivors };

    let mut next: Vec<Persona> = Vec::new();
    let mut names: Vec<String> = Vec::new();

    // Two reviewers with the same prompt are one reviewer that costs twice as
    // much, so names must stay distinct. How a collision is resolved depends on
    // what collided: an elite or immigrant with a taken name really is the same
    // genome and is dropped, but a mutant or crossover is a different prompt
    // whose author merely reused the parent's name — that one is worth keeping,
    // so it is renamed instead of discarded.
    let admit = |next: &mut Vec<Persona>, names: &mut Vec<String>, mut p: Persona| -> bool {
        if next.len() >= target {
            return false;
        }
        if names.contains(&p.name) {
            let bred = matches!(p.origin, Origin::Mutant | Origin::Crossover);
            if !bred {
                return false;
            }
            let base = p.name.clone();
            let mut n = 2;
            while names.contains(&p.name) {
                p.name = format!("{base} v{n}");
                n += 1;
            }
            p.id = format!("{}-{}", p.id, crate::config::slug(&p.name));
        }
        names.push(p.name.clone());
        next.push(p);
        true
    };

    // 1. Elites, unchanged.
    for (p, _) in pool.iter().take(ev.elite) {
        let mut e = (*p).clone();
        e.generation = next_generation;
        e.origin = Origin::Elite;
        e.parent = Some(p.id.clone());
        admit(&mut next, &mut names, e);
    }

    // 2. Mutants, rewritten against their own scorecard. Drawn from below the
    // elites: mutating the persona you just carried over verbatim spends the
    // slot re-deriving a genome you already have.
    for k in 0..ev.mutants {
        if next.len() >= target {
            break;
        }
        let (p, f) = pool[(ev.elite + k) % pool.len()];
        let evidence = evidence_for(p, findings, rulings);
        let prompt = prompts::mutate(p, f, &evidence, pressure);
        match call_meta(meta, &prompt, &format!("gen{next_generation}/mutate/{}", p.id)).await {
            Ok((spec, u)) => {
                usage.merge(&u);
                let child = from_spec(
                    spec,
                    Role::Attacker,
                    next_generation,
                    Origin::Mutant,
                    Some(p.id.clone()),
                    p.temperature,
                );
                admit(&mut next, &mut names, child);
            }
            // Leave the slot empty rather than cloning the parent into it; the
            // immigrant and backfill steps below will fill it with something
            // that is at least a different point of view.
            Err(e) => tracing::warn!(persona = %p.name, "mutation failed, leaving the slot to backfill: {e}"),
        }
    }

    // 3. Crossovers between distinct winners.
    if pool.len() >= 2 {
        for i in 0..ev.crossovers {
            if next.len() >= target {
                break;
            }
            let a = pool[i % pool.len()];
            let b = pool[(i + 1) % pool.len()];
            let pair = ((*a.0).clone(), a.1.clone());
            let pair_b = ((*b.0).clone(), b.1.clone());
            let prompt = prompts::crossover(&pair, &pair_b);
            match call_meta(meta, &prompt, &format!("gen{next_generation}/crossover/{i}")).await {
                Ok((spec, u)) => {
                    usage.merge(&u);
                    let child = from_spec(
                        spec,
                        Role::Attacker,
                        next_generation,
                        Origin::Crossover,
                        Some(a.0.id.clone()),
                        (a.0.temperature + b.0.temperature) / 2.0,
                    );
                    admit(&mut next, &mut names, child);
                }
                Err(e) => tracing::warn!("crossover failed, skipping: {e}"),
            }
        }
    }

    // 4. Immigrants: unrelated seeds, to keep the population from inbreeding.
    {
        let mut rng = rand::thread_rng();
        for _ in 0..ev.immigrants {
            if next.len() >= target {
                break;
            }
            if let Some(mut p) = crate::seeds::immigrant(Role::Attacker, &names, &mut rng) {
                p.generation = next_generation;
                admit(&mut next, &mut names, p);
            }
        }
    }

    // 5. Backfill to the configured population size from whatever is left,
    // including slots a failed mutation or crossover gave up.
    if next.len() < target {
        let mut rng = rand::thread_rng();
        let mut spare: Vec<Persona> = crate::seeds::library(Role::Attacker)
            .into_iter()
            .filter(|p| !names.contains(&p.name))
            .collect();
        spare.shuffle(&mut rng);
        for mut p in spare {
            if next.len() >= target {
                break;
            }
            p.generation = next_generation;
            p.origin = Origin::Immigrant;
            admit(&mut next, &mut names, p);
        }
    }
    // Still short — a seed library smaller than the configured population.
    // Recycle the best, but name the copies apart so reports and fitness
    // scorecards do not silently merge two different agents.
    let mut copy = 1;
    while next.len() < target && !ranked.is_empty() {
        let mut p = ranked[next.len() % ranked.len()].0.clone();
        p.generation = next_generation;
        p.origin = Origin::Elite;
        p.name = format!("{} #{copy}", p.name);
        p.id = format!("{}-r{copy}", p.id);
        copy += 1;
        next.push(p);
    }

    next.extend(carry_defenders(cfg, current, next_generation));
    dedupe_ids(&mut next);

    Ok(Brood { personas: next, usage })
}

/// Defenders are not under selection, so they are simply carried forward.
fn carry_defenders(cfg: &Config, current: &[Persona], generation: u32) -> Vec<Persona> {
    let mut defenders: Vec<Persona> = current
        .iter()
        .filter(|p| p.role == Role::Defender)
        .map(|p| {
            let mut d = p.clone();
            d.generation = generation;
            d
        })
        .collect();
    if defenders.is_empty() {
        defenders = cfg.seed_personas(Role::Defender);
        for d in &mut defenders {
            d.generation = generation;
        }
    }
    defenders.truncate(cfg.arena.defenders.max(1));
    defenders
}

fn from_spec(
    spec: PersonaSpec,
    role: Role,
    generation: u32,
    origin: Origin,
    parent: Option<String>,
    temperature: f32,
) -> Persona {
    Persona {
        id: format!("{}-{}-g{generation}", role.short(), crate::config::slug(&spec.name)),
        name: spec.name,
        role,
        prompt: spec.prompt,
        focus: spec.focus,
        temperature,
        generation,
        parent,
        origin,
    }
}

fn dedupe_ids(personas: &mut [Persona]) {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for p in personas.iter_mut() {
        let n = seen.entry(p.id.clone()).or_insert(0);
        if *n > 0 {
            p.id = format!("{}-{}", p.id, n);
        }
        *n += 1;
    }
}

async fn call_meta(
    meta: &SharedProvider,
    prompt: &prompts::AttackPrompt,
    label: &str,
) -> Result<(PersonaSpec, Usage)> {
    let resp = meta
        .complete(Request::new(label, &prompt.system, &prompt.user).temperature(1.0))
        .await?;
    let spec: PersonaSpec = extract_json(&resp.text)?;
    anyhow::ensure!(
        spec.prompt.trim().len() > 80,
        "meta-provider returned a prompt too short to be a reviewer"
    );
    Ok((spec, resp.usage))
}

/// The persona's own record, as the meta-prompt needs to see it: what it
/// claimed and what the judge did with each claim.
fn evidence_for(persona: &Persona, findings: &[Finding], rulings: &[Ruling]) -> String {
    let by_id: HashMap<&str, &Ruling> =
        rulings.iter().map(|r| (r.raw.finding_id.as_str(), r)).collect();

    let mut s = String::new();
    let mut any = false;
    for f in findings.iter().filter(|f| f.persona_id == persona.id) {
        any = true;
        match by_id.get(f.id.as_str()) {
            Some(r) => {
                let _ = writeln!(
                    s,
                    "- [{}] `{}` {} — {}\n  judge: {}",
                    r.raw.verdict,
                    f.location(),
                    f.raw.title,
                    match r.raw.verdict {
                        Verdict::Confirmed => format!("(ruled {} / {})", r.raw.severity, r.raw.altitude),
                        _ => String::new(),
                    },
                    r.raw.reasoning
                );
            }
            None if f.duplicate_of.is_some() => {
                let _ = writeln!(s, "- [duplicate] `{}` {}", f.location(), f.raw.title);
            }
            None => {
                let _ = writeln!(s, "- [unjudged] `{}` {}", f.location(), f.raw.title);
            }
        }
    }
    if !any {
        s.push_str("This reviewer raised nothing at all. Either its territory was genuinely clean, or its prompt is too narrow or too demanding to produce a claim.");
    }
    s
}

/// Population size sanity: used by the CLI to warn before spending money.
pub fn describe(ev: &EvolutionConfig) -> String {
    if !ev.enabled {
        return "disabled (same personas every generation)".into();
    }
    format!(
        "{} elite, {} mutant, {} crossover, {} immigrant per generation",
        ev.elite, ev.mutants, ev.crossovers, ev.immigrants
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_ids_are_disambiguated() {
        let base = Persona {
            id: "atk-x".into(),
            name: "x".into(),
            role: Role::Attacker,
            prompt: String::new(),
            focus: String::new(),
            temperature: 0.8,
            generation: 1,
            parent: None,
            origin: Origin::Elite,
        };
        let mut ps = vec![base.clone(), base.clone(), base];
        dedupe_ids(&mut ps);
        let ids: std::collections::HashSet<_> = ps.iter().map(|p| &p.id).collect();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn a_persona_that_found_nothing_is_described_as_such() {
        let p = Persona {
            id: "atk-quiet".into(),
            name: "quiet".into(),
            role: Role::Attacker,
            prompt: String::new(),
            focus: String::new(),
            temperature: 0.8,
            generation: 0,
            parent: None,
            origin: Origin::Seed,
        };
        let s = evidence_for(&p, &[], &[]);
        assert!(s.contains("raised nothing"));
    }
}
