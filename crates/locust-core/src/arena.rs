//! The arena: attack, defend, judge, score, breed — once per generation.

use crate::config::{Config, WorkspacePolicy};
use crate::context::ContextPack;
use crate::model::*;
use crate::prompts::{self, SettledFinding};
use crate::scoring;
use crate::store::{self, Store};
use anyhow::{bail, Context, Result};
use locust_provider::{extract_json, Request, SharedProvider, Usage};
use locust_vcs::{Vcs, Workspace};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

/// Progress, for whatever is driving the run.
#[derive(Debug, Clone)]
pub enum Event {
    RunStarted { run_id: String, files: usize, generations: u32 },
    GenerationStarted { generation: u32, total: u32, pressure: f64, personas: Vec<String> },
    AgentStarted { label: String },
    AgentFinished { label: String, produced: usize },
    AgentFailed { label: String, error: String },
    PhaseFinished { generation: u32, phase: &'static str, count: usize },
    GenerationFinished {
        generation: u32,
        confirmed: usize,
        refuted: usize,
        /// Rulings the panel did not agree on. Zero with a single judge.
        contested: usize,
        top: Vec<Fitness>,
    },
    Breeding { generation: u32 },
    Synthesizing,
    /// Something that undermines the validity of the run's verdicts, raised
    /// while there is still time to act on it.
    Warning { message: String },
    BudgetReached { spent: f64, budget: f64 },
    RunFinished { run_id: String, usage: Usage },
}

pub type Reporter = Arc<dyn Fn(Event) + Send + Sync>;

/// A generation in which nothing could be reviewed at all. Carried as a typed
/// error so the run can decide whether that is fatal (the first generation,
/// with nothing to show) or simply the end of the run (a later one, with
/// earlier generations worth reporting).
#[derive(Debug, thiserror::Error)]
#[error("every {phase} failed in generation {} ({launched}/{launched}); no {produced} was \
         performed. See .locust/runs/{run_id}/transcripts/ for the provider errors",
        generation + 1)]
struct GenerationAborted {
    generation: u32,
    /// What failed, as a noun: `reviewer`, `judge`.
    phase: &'static str,
    /// What therefore did not happen: `review`, `judging`.
    produced: &'static str,
    launched: usize,
    run_id: String,
}

/// How many judge calls were made and how many failed. Nothing being
/// confirmed and nothing being judged look identical in the output, so the
/// counts have to leave the phase.
#[derive(Default)]
struct JudgeCalls(std::cell::Cell<(usize, usize)>);

impl JudgeCalls {
    fn set(&self, calls: usize, failed: usize) {
        self.0.set((calls, failed));
    }
    fn get(&self) -> (usize, usize) {
        self.0.get()
    }
}

struct Attacked {
    findings: Vec<Finding>,
    failed: usize,
    launched: usize,
}

pub struct Arena {
    cfg: Config,
    vcs: Arc<dyn Vcs>,
    providers: Vec<SharedProvider>,
    judge: SharedProvider,
    panel: Vec<Judge>,
    meta: SharedProvider,
    synth: SharedProvider,
    store: Store,
    gate: Arc<Semaphore>,
    usage: Arc<Mutex<Usage>>,
    /// Every warning raised, kept so the durable record carries the
    /// qualifications the run itself made. A verdict whose panel lost a judge
    /// is not the same verdict, and a reader of `report.md` three weeks later
    /// has no other way to learn that.
    warnings: Arc<Mutex<Vec<String>>>,
    report: Reporter,
}

impl Arena {
    pub fn new(cfg: Config, vcs: Arc<dyn Vcs>, report: Reporter) -> Result<Self> {
        cfg.validate()?;
        let mut providers = Vec::new();
        for pc in cfg.active_providers() {
            providers.push(
                pc.build_with(&cfg.sandbox)
                    .with_context(|| format!("configuring provider `{}`", pc.name))?,
            );
        }
        Self::with_providers(cfg, vcs, providers, report)
    }

    /// Build an arena over providers that are already constructed. Lets a
    /// caller substitute its own implementations — which is how the arena loop
    /// is tested without spending money or depending on a vendor being up.
    pub fn with_providers(
        cfg: Config,
        vcs: Arc<dyn Vcs>,
        providers: Vec<SharedProvider>,
        report: Reporter,
    ) -> Result<Self> {
        cfg.validate()?;
        if providers.is_empty() {
            bail!("no providers available for this run");
        }

        let pick = |name: &Option<String>| -> SharedProvider {
            name.as_ref()
                .and_then(|n| providers.iter().find(|p| p.name() == n).cloned())
                // Default the adjudicating roles to the first configured
                // provider rather than a random one, so runs are reproducible.
                .unwrap_or_else(|| providers[0].clone())
        };
        let judge = pick(&cfg.arena.judge);
        let meta = pick(&cfg.evolution.meta_provider.clone().or(cfg.arena.judge.clone()));
        let synth = pick(&cfg.synthesis.provider.clone().or(cfg.arena.judge.clone()));

        let panel = cfg.panel()?;
        let store = Store::create(vcs.root(), &store::new_run_id())?;
        let gate = Arc::new(Semaphore::new(cfg.run.parallelism));

        Ok(Self {
            cfg,
            panel,
            vcs,
            providers,
            judge,
            meta,
            synth,
            store,
            gate,
            usage: Arc::new(Mutex::new(Usage::default())),
            warnings: Arc::new(Mutex::new(Vec::new())),
            report,
        })
    }

    pub fn run_dir(&self) -> &std::path::Path {
        &self.store.run_dir
    }

    pub async fn run(&self) -> Result<RunOutcome> {
        let started = std::time::Instant::now();
        let started_at = chrono::Utc::now();

        let range = self
            .vcs
            .range(&self.cfg.run.base, &self.cfg.run.target)
            .with_context(|| {
                format!("resolving `{}`...`{}`", self.cfg.run.base, self.cfg.run.target)
            })?;

        let pack = Arc::new(ContextPack::build(self.vcs.as_ref(), &range, &self.cfg.context)?);
        if pack.is_empty() {
            bail!(
                "no reviewable changes between `{}` and `{}`",
                self.cfg.run.base,
                self.cfg.run.target
            );
        }

        let total = self.cfg.run.generations;
        (self.report)(Event::RunStarted {
            run_id: self.store.run_id.clone(),
            files: pack.files.len(),
            generations: total,
        });

        for message in self.validity_warnings() {
            self.warn(message).await;
        }

        self.store.write_text("config.toml", &toml::to_string_pretty(&self.cfg)?)?;
        self.store.write_text("context.md", &pack.render(true))?;

        let mut attackers = self.cfg.seed_personas(Role::Attacker)?;
        attackers.truncate(self.cfg.arena.attackers.max(1));
        let mut defenders = self.cfg.seed_personas(Role::Defender)?;
        defenders.truncate(self.cfg.arena.defenders.max(1));

        let mut generations: Vec<GenerationOutcome> = Vec::new();
        let mut settled: Vec<SettledFinding> = Vec::new();
        let mut judging_aborted = false;

        for generation in 0..total {
            let pressure = scoring::pressure(generation, total);
            let population: Vec<Persona> =
                attackers.iter().chain(defenders.iter()).cloned().collect();

            (self.report)(Event::GenerationStarted {
                generation,
                total,
                pressure,
                personas: attackers.iter().map(|p| p.name.clone()).collect(),
            });

            let outcome = match self
                .generation(generation, total, pressure, &pack, &population, &settled)
                .await
            {
                Ok(o) => o,
                Err(e) => match e.downcast::<GenerationAborted>() {
                    // Nothing was ever reviewed: there is no report to make.
                    Ok(aborted) if generations.is_empty() => return Err(aborted.into()),
                    // Earlier generations stand; stop here and report them.
                    Ok(aborted) => {
                        judging_aborted = aborted.phase == "judge";
                        self.warn(format!("{aborted}; reporting the {} generation(s) that completed", generations.len())).await;
                        break;
                    }
                    Err(other) => return Err(other),
                },
            };

            for ruling in &outcome.rulings {
                if ruling.raw.verdict == Verdict::Undecided {
                    continue;
                }
                if let Some(f) = outcome.findings.iter().find(|f| f.id == ruling.raw.finding_id) {
                    settled.push(SettledFinding {
                        location: f.location(),
                        title: f.raw.title.clone(),
                        verdict: ruling.raw.verdict,
                    });
                }
            }

            let confirmed = outcome.confirmed().count();
            let refuted = outcome
                .rulings
                .iter()
                .filter(|r| r.raw.verdict == Verdict::Refuted)
                .count();
            // A judge that refutes nothing is not discriminating, it is
            // rubber-stamping -- and every fitness score that generation is
            // measuring the attackers' volume rather than their accuracy.
            if refuted == 0 && confirmed >= 4 {
                self.warn(format!(
                        "the panel refuted none of {confirmed} findings in generation {}. \
                         Either the change is genuinely this bad, or judging is not \
                         discriminating -- check a refutation you disagree with in the \
                         transcripts before trusting this generation's scores",
                        generation + 1
                    )).await;
            }

            (self.report)(Event::GenerationFinished {
                generation,
                confirmed: outcome.confirmed().count(),
                refuted: outcome
                    .rulings
                    .iter()
                    .filter(|r| r.raw.verdict == Verdict::Refuted)
                    .count(),
                contested: outcome.rulings.iter().filter(|r| r.contested()).count(),
                top: outcome.fitness.iter().take(3).cloned().collect(),
            });

            let is_last = generation + 1 >= total;

            // Checked at the generation boundary rather than mid-flight: a run
            // halted between phases would have paid for findings nobody judged,
            // which is the worst of both outcomes.
            if !is_last && self.over_budget().await {
                let spent = self.usage.lock().await.cost_usd;
                (self.report)(Event::BudgetReached { spent, budget: self.cfg.run.budget_usd });
                generations.push(outcome);
                break;
            }

            if !is_last && self.cfg.evolution.enabled {
                (self.report)(Event::Breeding { generation });
                let next_pressure = scoring::pressure(generation + 1, total);
                let label = format!("gen{generation}-breed");
                let wanted = prompts::wants_workspace(&self.cfg, self.meta.needs_workspace());
                let ws = self.workspace(&label, wanted).await;
                let brood = crate::evolution::breed(
                    &self.cfg,
                    &self.meta,
                    &population,
                    &outcome.fitness,
                    &outcome.findings,
                    &outcome.rulings,
                    generation + 1,
                    next_pressure,
                    ws.as_ref().map(|w| w.path.as_path()).unwrap_or(self.vcs.root()),
                ).await;
                self.release(ws).await;
                match brood {
                    Ok(brood) => {
                        self.add_usage(&brood.usage).await;
                        attackers = brood
                            .personas
                            .iter()
                            .filter(|p| p.role == Role::Attacker)
                            .cloned()
                            .collect();
                        defenders = brood
                            .personas
                            .iter()
                            .filter(|p| p.role == Role::Defender)
                            .cloned()
                            .collect();
                    }
                    Err(e) => {
                        tracing::warn!("breeding failed, reusing this generation's personas: {e}");
                    }
                }
            }

            generations.push(outcome);
        }

        let mut outcome = RunOutcome {
            run_id: self.store.run_id.clone(),
            started_at,
            base: range.base.clone(),
            target: range.target.clone(),
            vcs: self.vcs.kind().to_string(),
            generations,
            themes: Vec::new(),
            warnings: Vec::new(),
            usage: self.usage.lock().await.clone(),
            elapsed_secs: started.elapsed().as_secs_f64(),
        };

        if self.cfg.synthesis.enabled {
            (self.report)(Event::Synthesizing);
            match self.synthesize(&pack, &outcome).await {
                Ok(themes) => outcome.themes = themes,
                Err(e) => tracing::warn!("synthesis failed: {e}"),
            }
        }

        outcome.usage = self.usage.lock().await.clone();
        outcome.warnings = self.warnings.lock().await.clone();
        outcome.elapsed_secs = started.elapsed().as_secs_f64();

        self.store.write_json("report.json", &outcome)?;
        self.store.write_text("report.md", &crate::report::markdown(&outcome, &self.cfg))?;

        (self.report)(Event::RunFinished {
            run_id: self.store.run_id.clone(),
            usage: outcome.usage.clone(),
        });

        // Agent checkouts are large and worthless once the run is over.
        if let Err(e) = self.vcs.prune_all() {
            tracing::warn!("could not clean up workspaces: {e}");
        }

        if judging_aborted {
            bail!("incomplete judgment; completed generations saved in {}", self.store.run_dir.display());
        }
        Ok(outcome)
    }

    /// Conditions that make the run's verdicts less trustworthy than they look.
    ///
    /// These are all knowable before any money is spent, and all of them are
    /// easy to miss in a config that otherwise looks fine.
    fn validity_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();

        if self.providers.len() == 1 {
            out.push(format!(
                "every agent and every judge is `{}`. Findings are still defended and judged, \
                 but a model grades its own output generously and corroboration across \
                 providers cannot mean anything with one provider. Verdicts from this run are \
                 weaker evidence than they appear",
                self.providers[0].name()
            ));
        } else if let Some(pinned) = &self.cfg.arena.judge {
            // Pinning the panel to one provider is legitimate, but it silently
            // disables the author-exclusion that makes judging neutral.
            out.push(format!(
                "`[arena] judge` pins the whole panel to `{pinned}`, so judges will rule on \
                 findings their own provider raised. Unset it to rotate judging across \
                 providers that authored nothing in the batch"
            ));
        }

        if self.panel.len() == 1 {
            out.push(
                "a single judge: its verdict is final and there is no disagreement to measure. \
                 `consensus_weight` and the panel-split report do nothing"
                    .to_string(),
            );
        }

        out
    }

    async fn generation(
        &self,
        generation: u32,
        total: u32,
        pressure: f64,
        pack: &Arc<ContextPack>,
        population: &[Persona],
        settled: &[SettledFinding],
    ) -> Result<GenerationOutcome> {
        let started = std::time::Instant::now();
        let dir = self.store.generation_dir(generation);

        let attackers: Vec<&Persona> =
            population.iter().filter(|p| p.role == Role::Attacker).collect();
        let defenders: Vec<&Persona> =
            population.iter().filter(|p| p.role == Role::Defender).collect();

        let Attacked { mut findings, failed, launched } =
            self.attack(generation, total, pressure, pack, &attackers, settled).await;

        // A generation where every reviewer failed is not a clean review, it is
        // no review at all. Returning "0 findings" and exiting 0 would let a CI
        // gate pass on a run that never happened.
        if launched > 0 && failed == launched {
            // Signalled rather than bailed: generations already completed are
            // finished work, and throwing them away to report a later failure
            // loses the whole run for the sake of its last step.
            return Err(GenerationAborted {
                generation,
                phase: "reviewer",
                produced: "review",
                launched,
                run_id: self.store.run_id.clone(),
            }
            .into());
        }
        if failed * 2 >= launched && failed > 0 {
            self.warn(format!(
                    "{failed} of {launched} reviewers failed in generation {}; the population \
                     was smaller than configured and the fitness scores reflect that",
                    generation + 1
                )).await;
        }
        // Check every citation against the real file before anyone argues
        // about it, so the defender and the judge can both see when a reviewer
        // quoted code that is not there.
        crate::grounding::verify(
            self.vcs.as_ref(),
            &pack.range.target,
            &pack.files,
            &mut findings,
        );
        crate::dedup::cluster(&mut findings);

        let ungrounded = findings.iter().filter(|f| !f.grounding.is_sound()).count();
        if ungrounded > 0 {
            self.warn(format!(
                    "{ungrounded} of {} findings cite code that could not be located in the \
                     target revision; they are marked for the defenders and the panel",
                    findings.len()
                )).await;
        }
        (self.report)(Event::PhaseFinished { generation, phase: "attack", count: findings.len() });

        let rebuttals = {
            let canonical: Vec<&Finding> = findings.iter().filter(|f| f.is_canonical()).collect();
            let rebuttals = self.defend(generation, pack, &defenders, &canonical).await;
            (self.report)(Event::PhaseFinished {
                generation,
                phase: "defend",
                count: rebuttals.len(),
            });
            rebuttals
        };

        let judge_calls = JudgeCalls::default();
        let mut rulings = {
            let canonical: Vec<&Finding> = findings.iter().filter(|f| f.is_canonical()).collect();
            self.judge(generation, pack, &canonical, &rebuttals, settled, &judge_calls).await
        };

        // Nothing judged is not the same as nothing confirmed, but the report
        // and `--fail-on` cannot tell them apart: both show no confirmed
        // findings and exit 0. This is the third phase to need the same guard.
        let (calls, failed) = judge_calls.get();
        if calls > 0 && failed == calls {
            return Err(GenerationAborted {
                generation,
                phase: "judge",
                produced: "judging",
                launched: calls,
                run_id: self.store.run_id.clone(),
            }
            .into());
        }

        // The judge read every case in a batch side by side and may have spotted
        // repeats that token overlap could not. Fold those in, then drop the
        // demoted findings' rulings so one defect is not counted twice in the
        // confirmed totals.
        let demoted = crate::dedup::apply_judge_duplicates(&mut findings, &rulings);
        if !demoted.is_empty() {
            rulings.retain(|r| !demoted.contains(&r.raw.finding_id));
            tracing::debug!("judge merged {} finding(s) into existing clusters", demoted.len());
        }
        (self.report)(Event::PhaseFinished { generation, phase: "judge", count: rulings.len() });

        let fitness = scoring::score_generation(
            &self.cfg.scoring,
            population,
            &findings,
            &rulings,
            pressure,
        );

        self.store.write_json(&format!("{dir}/personas.json"), &population)?;
        self.store.write_json(&format!("{dir}/findings.json"), &findings)?;
        self.store.write_json(&format!("{dir}/rebuttals.json"), &rebuttals)?;
        self.store.write_json(&format!("{dir}/rulings.json"), &rulings)?;
        self.store.write_json(&format!("{dir}/fitness.json"), &fitness)?;

        let missing: Vec<&str> = findings.iter().filter(|f| f.is_canonical())
            .filter(|f| !rulings.iter().any(|r| r.raw.finding_id == f.id))
            .map(|f| f.id.as_str()).collect();
        if !missing.is_empty() {
            let message = format!("incomplete judgment in generation {}: no ruling for {}",
                generation + 1, missing.join(", "));
            self.warn(message.clone()).await;
            bail!("{message}");
        }

        Ok(GenerationOutcome {
            generation,
            pressure,
            personas: population.to_vec(),
            findings,
            rebuttals,
            rulings,
            fitness,
            usage: self.usage.lock().await.clone(),
            elapsed_secs: started.elapsed().as_secs_f64(),
        })
    }

    // ---- phases -----------------------------------------------------------

    /// Outcome of one attack phase. The counts matter as much as the findings:
    /// an empty result from four healthy reviewers means the change is clean,
    /// while an empty result from four failures means nothing was reviewed.
    async fn attack(
        &self,
        generation: u32,
        total: u32,
        pressure: f64,
        pack: &Arc<ContextPack>,
        attackers: &[&Persona],
        settled: &[SettledFinding],
    ) -> Attacked {
        let slots = self.cfg.arena.attackers.max(1);
        let mut tasks = Vec::new();

        for i in 0..slots {
            if attackers.is_empty() {
                break;
            }
            let persona = attackers[i % attackers.len()].clone();
            // Spread personas across vendors so the same point of view is
            // expressed by different models; disagreement between them is the
            // signal multiple providers exists to produce.
            let provider = self.providers[i % self.providers.len()].clone();
            let label = prompts::agent_id(generation, Role::Attacker, &persona, provider.name(), i);
            let pack = pack.clone();
            let settled: Vec<SettledFinding> = settled
                .iter()
                .map(|s| SettledFinding {
                    location: s.location.clone(),
                    title: s.title.clone(),
                    verdict: s.verdict,
                })
                .collect();

            tasks.push(async move {
                // Held across the checkout as well as the call: a run with
                // parallelism 2 and six reviewers should have two worktrees on
                // disk, not six.
                let _permit = self.gate.acquire().await.expect("semaphore is never closed");
                let has_ws = prompts::wants_workspace(&self.cfg, provider.needs_workspace());
                let ws = self.workspace(&label, has_ws).await;
                let prompt = prompts::attack(
                    &persona,
                    &pack,
                    &settled,
                    generation,
                    total,
                    pressure,
                    ws.is_some(),
                );
                let out = self
                    .call(
                        &provider,
                        &label,
                        &prompt.system,
                        &prompt.user,
                        persona.temperature,
                        ws.as_ref(),
                    )
                    .await;
                self.release(ws).await;

                let mut ok = true;
                let report: AttackReport = match out {
                    Ok(text) => match extract_json(&text) {
                        Ok(r) => r,
                        Err(e) => {
                            ok = false;
                            (self.report)(Event::AgentFailed {
                                label: label.clone(),
                                error: format!("unparseable output: {e}"),
                            });
                            AttackReport::default()
                        }
                    },
                    Err(e) => {
                        ok = false;
                        (self.report)(Event::AgentFailed {
                            label: label.clone(),
                            error: e.to_string(),
                        });
                        AttackReport::default()
                    }
                };

                let findings: Vec<Finding> = report
                    .findings
                    .into_iter()
                    .enumerate()
                    .map(|(n, raw)| Finding {
                        id: format!("f{generation}-{i}-{n}"),
                        generation,
                        persona: persona.name.clone(),
                        persona_id: persona.id.clone(),
                        provider: provider.name().to_string(),
                        raw,
                        cluster: String::new(),
                        duplicate_of: None,
                        corroboration: 1,
                        grounding: Grounding::Unchecked,
                    })
                    .collect();

                if ok {
                    (self.report)(Event::AgentFinished {
                        label,
                        produced: findings.len(),
                    });
                }
                (findings, ok)
            });
        }

        let results = futures::future::join_all(tasks).await;
        let launched = results.len();
        let failed = results.iter().filter(|(_, ok)| !ok).count();
        let findings = results.into_iter().flat_map(|(f, _)| f).collect();
        Attacked { findings, failed, launched }
    }

    async fn defend(
        &self,
        generation: u32,
        pack: &Arc<ContextPack>,
        defenders: &[&Persona],
        canonical: &[&Finding],
    ) -> Vec<Rebuttal> {
        if canonical.is_empty() || defenders.is_empty() {
            return Vec::new();
        }
        // Batch so a defender reasons about a handful of findings rather than
        // skimming forty.
        let batch_size = self.cfg.arena.judge_batch.max(1);
        let batches: Vec<Vec<&Finding>> =
            canonical.chunks(batch_size).map(|c| c.to_vec()).collect();

        let mut tasks = Vec::new();
        for (d, persona) in defenders.iter().enumerate() {
            for (b, batch) in batches.iter().enumerate() {
                let persona = (*persona).clone();
                // Rotate so a defender does not always answer the same vendor.
                let provider = self.providers[(d + b) % self.providers.len()].clone();
                let label = format!(
                    "gen{generation}-def-{}-{}-b{b}",
                    crate::config::slug(&persona.name),
                    crate::config::slug(provider.name())
                );
                let pack = pack.clone();
                let batch: Vec<Finding> = batch.iter().map(|f| (*f).clone()).collect();

                tasks.push(async move {
                    let _permit = self.gate.acquire().await.expect("semaphore is never closed");
                    let has_ws = prompts::wants_workspace(&self.cfg, provider.needs_workspace());
                    let ws = self.workspace(&label, has_ws).await;
                    let refs: Vec<&Finding> = batch.iter().collect();
                    let prompt = prompts::defend(&persona, &pack, &refs, ws.is_some());
                    let out = self
                        .call(
                            &provider,
                            &label,
                            &prompt.system,
                            &prompt.user,
                            persona.temperature,
                            ws.as_ref(),
                        )
                        .await;
                    self.release(ws).await;

                    let report: DefenseReport = match out.and_then(|t| extract_json(&t)) {
                        Ok(r) => r,
                        Err(e) => {
                            (self.report)(Event::AgentFailed {
                                label: label.clone(),
                                error: e.to_string(),
                            });
                            DefenseReport::default()
                        }
                    };

                    let valid: std::collections::HashSet<&str> =
                        batch.iter().map(|f| f.id.as_str()).collect();
                    let rebuttals: Vec<Rebuttal> = report
                        .rebuttals
                        .into_iter()
                        // Drop rebuttals against ids that were not in the
                        // batch: a hallucinated id would otherwise attach a
                        // defence to an unrelated finding.
                        .filter(|r| valid.contains(r.finding_id.as_str()))
                        .map(|raw| Rebuttal {
                            persona: persona.name.clone(),
                            persona_id: persona.id.clone(),
                            provider: provider.name().to_string(),
                            raw,
                        })
                        .collect();

                    (self.report)(Event::AgentFinished {
                        label,
                        produced: rebuttals.len(),
                    });
                    rebuttals
                });
            }
        }

        futures::future::join_all(tasks).await.into_iter().flatten().collect()
    }

    /// Rule on each batch with every judge on the panel, then aggregate.
    ///
    /// Each judge sees the identical case file and does not know who else is on
    /// the panel — the point is three independent readings, not a discussion
    /// that converges on whoever is most assertive.
    async fn judge(
        &self,
        generation: u32,
        pack: &Arc<ContextPack>,
        canonical: &[&Finding],
        rebuttals: &[Rebuttal],
        settled: &[SettledFinding],
        judge_calls: &JudgeCalls,
    ) -> Vec<Ruling> {
        if canonical.is_empty() || self.panel.is_empty() {
            return Vec::new();
        }
        let by_finding: HashMap<&str, Vec<&Rebuttal>> =
            rebuttals.iter().fold(HashMap::new(), |mut acc, r| {
                acc.entry(r.raw.finding_id.as_str()).or_default().push(r);
                acc
            });

        let batch_size = self.cfg.arena.judge_batch.max(1);
        let mut tasks = Vec::new();

        for (b, batch) in canonical.chunks(batch_size).enumerate() {
            for (j, judge) in self.panel.iter().enumerate() {
                let provider = self.provider_for_judge(judge, batch, b + j);
                // `j` disambiguates judges whose names slug to the same thing
                // ("fast reviewer" and "fast-reviewer"), which would otherwise
                // overwrite each other's transcripts.
                let label = format!(
                    "gen{generation}-judge{j}-{}-{}-b{b}",
                    crate::config::slug(&judge.name),
                    crate::config::slug(provider.name())
                );
                let pack = pack.clone();
                let judge = judge.clone();
                let cases: Vec<(Finding, Vec<Rebuttal>)> = batch
                    .iter()
                    .map(|f| {
                        let rs = by_finding
                            .get(f.id.as_str())
                            .map(|v| v.iter().map(|r| (*r).clone()).collect())
                            .unwrap_or_default();
                        ((*f).clone(), rs)
                    })
                    .collect();
                let settled: Vec<SettledFinding> = settled
                    .iter()
                    .map(|s| SettledFinding {
                        location: s.location.clone(),
                        title: s.title.clone(),
                        verdict: s.verdict,
                    })
                    .collect();

                tasks.push(async move {
                    let _permit =
                        self.gate.acquire().await.expect("semaphore is never closed");
                    let borrowed: Vec<(&Finding, Vec<&Rebuttal>)> =
                        cases.iter().map(|(f, rs)| (f, rs.iter().collect())).collect();
                    let prompt = prompts::judge(&pack, &borrowed, &settled, &judge);
                    // Adjudication is not ideation: cold by default, nudged only
                    // by the judge's own temperament.
                    let temperature = judge.traits.temperature(0.1);
                    let wanted = prompts::wants_workspace(&self.cfg, provider.needs_workspace());
                    let ws = self.workspace(&label, wanted).await;
                    let out = self
                        .call(&provider, &label, &prompt.system, &prompt.user, temperature, ws.as_ref())
                        .await;
                    self.release(ws).await;

                    let mut ok = true;
                    let report: JudgeReport = match out.and_then(|t| extract_json(&t)) {
                        Ok(r) => r,
                        Err(e) => {
                            ok = false;
                            (self.report)(Event::AgentFailed {
                                label: label.clone(),
                                error: e.to_string(),
                            });
                            JudgeReport::default()
                        }
                    };

                    let valid: std::collections::HashSet<&str> =
                        cases.iter().map(|(f, _)| f.id.as_str()).collect();
                    let mut seen = std::collections::HashSet::new();
                    let mut votes: Vec<JudgeVote> = report
                        .rulings
                        .into_iter()
                        .filter(|r| valid.contains(r.finding_id.as_str()))
                        // One vote per finding per judge; a repeated id would
                        // let a single judge outvote the rest of the panel.
                        .filter(|r| seen.insert(r.finding_id.clone()))
                        .map(|raw| JudgeVote {
                            judge: judge.name.clone(),
                            provider: provider.name().to_string(),
                            traits: judge.traits,
                            weight: judge.weight,
                            raw,
                        })
                        .collect();

                    if ok && seen.len() != valid.len() {
                        ok = false;
                        let mut missing: Vec<&str> = valid.iter().copied()
                            .filter(|id| !seen.contains(*id)).collect();
                        missing.sort_unstable();
                        let error = format!("incomplete judgment: missing {}", missing.join(", "));
                        self.warn(format!("{label}: {error}")).await;
                        (self.report)(Event::AgentFailed { label: label.clone(), error });
                        // A partial batch is a failed call, not a smaller panel
                        // selected separately by the judge for each finding.
                        votes.clear();
                    }
                    if ok {
                        (self.report)(Event::AgentFinished { label, produced: votes.len() });
                    }
                    (votes, ok)
                });
            }
        }

        let results = futures::future::join_all(tasks).await;
        let calls = results.len();
        let failed = results.iter().filter(|(_, ok)| !ok).count();
        let votes: Vec<JudgeVote> =
            results.into_iter().flat_map(|(v, _)| v).collect();
        judge_calls.set(calls, failed);

        // A judge that errored is absent, not agreeing. Left unsaid, a panel of
        // three that lost one reports its verdict as unanimous 2/2 and the
        // dissent figure -- the whole reason to run a panel -- quietly becomes
        // a measurement of a smaller panel than was configured.
        if failed > 0 {
            self.warn(format!(
                    "{failed} of {calls} judge calls failed in generation {}; affected findings \
                     were ruled by a smaller panel than configured, so their agreement figures \
                     are over the judges that answered",
                    generation + 1
                )).await;
        }

        // Group by finding, preserving the order the findings came in so the
        // report is stable across runs.
        let mut grouped: HashMap<&str, Vec<JudgeVote>> = HashMap::new();
        for v in &votes {
            grouped.entry(v.raw.finding_id.as_str()).or_default().push(v.clone());
        }

        let mut rulings = Vec::new();
        for f in canonical {
            let Some(votes) = grouped.remove(f.id.as_str()) else { continue };
            if let Some(ruling) = Ruling::from_panel(&f.id, generation, votes) {
                rulings.push(ruling);
            }
        }
        rulings
    }

    /// Which provider carries this judge's vote.
    ///
    /// A pinned provider wins. Otherwise the judge rotates across providers
    /// that authored none of the findings in the batch — models prefer their
    /// own output, and a judge ruling on its own agents' findings turns the
    /// tournament into a popularity contest between vendors. The rotation is
    /// offset per judge, so a three-judge panel over three providers gets
    /// temperament diversity and model diversity from one configuration.
    fn provider_for_judge(
        &self,
        judge: &Judge,
        batch: &[&Finding],
        rotation: usize,
    ) -> SharedProvider {
        if let Some(name) = &judge.provider {
            if let Some(p) = self.providers.iter().find(|p| p.name() == name) {
                return p.clone();
            }
        }
        if self.cfg.arena.judge.is_some() || !self.cfg.arena.judge_excludes_author {
            return self.judge.clone();
        }
        let authors: std::collections::HashSet<&str> =
            batch.iter().map(|f| f.provider.as_str()).collect();
        let neutral: Vec<&SharedProvider> =
            self.providers.iter().filter(|p| !authors.contains(p.name())).collect();
        match neutral.len() {
            0 => self.providers[rotation % self.providers.len()].clone(),
            n => neutral[rotation % n].clone(),
        }
    }

    async fn synthesize(&self, pack: &ContextPack, outcome: &RunOutcome) -> Result<Vec<Theme>> {
        let confirmed = outcome.confirmed_findings();
        if confirmed.is_empty() {
            return Ok(Vec::new());
        }
        let prompt = prompts::synthesize(pack, &confirmed, self.cfg.synthesis.max_themes);
        let _permit = self.gate.acquire().await.expect("semaphore is never closed");
        let wanted = prompts::wants_workspace(&self.cfg, self.synth.needs_workspace());
        let ws = self.workspace("synthesis", wanted).await;
        let text = self
            .call(&self.synth, "synthesis", &prompt.system, &prompt.user, 0.3, ws.as_ref())
            .await;
        self.release(ws).await;
        let text = text?;
        let report: SynthesisReport = extract_json(&text)?;
        Ok(report.themes)
    }

    // ---- plumbing ---------------------------------------------------------

    /// One model call: records the transcript and folds token usage into the
    /// run total. The caller holds the concurrency permit, because for
    /// workspace-backed agents the checkout must be inside it too.
    async fn call(
        &self,
        provider: &SharedProvider,
        label: &str,
        system: &str,
        user: &str,
        temperature: f32,
        workspace: Option<&Workspace>,
    ) -> Result<String> {
        (self.report)(Event::AgentStarted { label: label.to_string() });

        let req = Request::new(label, system, user)
            .temperature(temperature)
            .in_workspace(Some(workspace.map(|w| w.path.clone())
                .unwrap_or_else(|| self.vcs.root().to_path_buf())));

        let started = std::time::Instant::now();
        let result = provider.complete(req).await;

        match result {
            Ok(resp) => {
                self.add_usage(&resp.usage).await;
                self.store.transcript(
                    label,
                    system,
                    user,
                    &resp.text,
                    &format!(
                        "provider `{}`, model `{}`, {} in / {} out, {:.1}s",
                        resp.provider,
                        resp.model,
                        resp.usage.input_tokens,
                        resp.usage.output_tokens,
                        started.elapsed().as_secs_f64()
                    ),
                );
                Ok(resp.text)
            }
            Err(e) => {
                self.store.transcript(
                    label,
                    system,
                    user,
                    &format!("ERROR: {e:?}"),
                    &format!("provider `{}` failed", provider.name()),
                );
                Err(e)
            }
        }
    }

    /// Raise a warning: recorded for the report, then shown live.
    async fn warn(&self, message: String) {
        let mut warnings = self.warnings.lock().await;
        warnings.push(message.clone());
        // Failed runs may never reach report.json.
        if let Err(e) = self.store.write_json("warnings.json", &*warnings) {
            tracing::warn!("could not persist run warnings: {e}");
        }
        drop(warnings);
        (self.report)(Event::Warning { message });
    }

    async fn add_usage(&self, u: &Usage) {
        self.usage.lock().await.merge(u);
    }

    /// True once spend has passed `[run] budget_usd`. Always false when the
    /// budget is zero (disabled) or when no provider has per-token costs
    /// configured, since a cost of 0.0 there means "unknown", not "free".
    async fn over_budget(&self) -> bool {
        let budget = self.cfg.run.budget_usd;
        budget > 0.0 && self.usage.lock().await.cost_usd >= budget
    }

    async fn workspace(&self, id: &str, wanted: bool) -> Option<Workspace> {
        if !wanted || self.cfg.run.workspaces == WorkspacePolicy::Never {
            return None;
        }
        let vcs = self.vcs.clone();
        let id = id.to_string();
        let rev = self.cfg.run.target.clone();
        let rev = vcs.resolve(&rev).unwrap_or(rev);

        // Worktree creation is blocking filesystem and subprocess work.
        match tokio::task::spawn_blocking(move || vcs.create_workspace(&id, &rev)).await {
            Ok(Ok(ws)) => Some(ws),
            Ok(Err(e)) => {
                tracing::warn!("workspace unavailable, falling back to context pack: {e}");
                None
            }
            Err(e) => {
                tracing::warn!("workspace task panicked: {e}");
                None
            }
        }
    }

    async fn release(&self, ws: Option<Workspace>) {
        let Some(ws) = ws else { return };
        let vcs = self.vcs.clone();
        let _ = tokio::task::spawn_blocking(move || vcs.destroy_workspace(&ws)).await;
    }
}
