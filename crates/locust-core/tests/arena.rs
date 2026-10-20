//! End-to-end test of the arena loop against scripted providers.
//!
//! Covers the part that unit tests cannot: that a generation's phases hand off
//! correctly, that duplicate findings across providers collapse before judging,
//! that the altitude ramp changes which persona wins between generations, and
//! that a provider failing mid-run degrades rather than aborting.

use anyhow::{bail, Result};
use async_trait::async_trait;
use locust_core::arena::{Arena, Event};
use locust_core::config::{Config, ProviderConfig, ProviderKind};
use locust_core::model::{Altitude, Grounding, Role, Severity, Verdict};
use locust_provider::{Provider, Request, Response, Usage};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// --- a repository to review ------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git").args(args).current_dir(dir).output()?;
    if !out.status.success() {
        bail!("git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A throwaway repo with one commit on `main` and one change on top.
fn fixture_repo() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("locust-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("src"))?;
    git(&dir, &["init", "-q", "--initial-branch=main"])?;
    git(&dir, &["config", "user.email", "t@example.com"])?;
    git(&dir, &["config", "user.name", "Test"])?;

    // Long enough that the line numbers the scripted reviewers cite are real.
    // Findings here are checked against the file like any other, so a fixture
    // citing line 180 of a 7-line file would (correctly) be reported as
    // ungrounded and the tests would be measuring the wrong thing.
    let filler = (0..200)
        .map(|i| format!("    let _spacer_{i} = {i};"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        dir.join("src/pool.rs"),
        format!("pub fn take() -> u32 {{\n    0\n{filler}\n}}\n"),
    )?;
    git(&dir, &["add", "-A"])?;
    git(&dir, &["commit", "-qm", "initial"])?;

    // The change lands on a branch, so `main` and `HEAD` differ.
    git(&dir, &["checkout", "-q", "-b", "feature"])?;
    std::fs::write(
        dir.join("src/pool.rs"),
        format!(
            "pub fn take() -> u32 {{\n    let n = compute();\n    n * 2\n{filler}\n}}\n\nfn compute() -> u32 {{\n    7\n}}\n"
        ),
    )?;
    git(&dir, &["add", "-A"])?;
    git(&dir, &["commit", "-qm", "change"])?;
    Ok(dir)
}

// --- a provider that answers from a script ---------------------------------

/// Replies are chosen by which phase the system prompt belongs to, so one
/// implementation serves as attacker, defender, judge and synthesiser.
struct Scripted {
    name: String,
    /// Findings this provider's attackers raise, as (file, line, title, altitude).
    findings: Vec<(&'static str, u32, &'static str, &'static str)>,
    verdict: &'static str,
    judge_limit: Option<usize>,
    invalid_judge_ids: bool,
    /// Fail every call after this many, to exercise degradation.
    fail_after: Option<usize>,
    calls: AtomicUsize,
    seen: Mutex<Vec<String>>,
}

impl Scripted {
    fn new(name: &str, findings: Vec<(&'static str, u32, &'static str, &'static str)>) -> Self {
        Self {
            name: name.into(),
            findings,
            verdict: "confirmed",
            judge_limit: None,
            invalid_judge_ids: false,
            fail_after: None,
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn refuting(mut self) -> Self {
        self.verdict = "refuted";
        self
    }

    fn failing_after(mut self, n: usize) -> Self {
        self.fail_after = Some(n);
        self
    }
}

#[async_trait]
impl Provider for Scripted {
    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &str {
        "scripted"
    }

    async fn complete(&self, req: Request) -> Result<Response> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(req.label.clone());
        if self.fail_after.is_some_and(|limit| n >= limit) {
            bail!("scripted failure");
        }

        let text = if req.system.contains("adjudicating a code review") {
            // A judge's temperament reaches the prompt as trait text; use it to
            // make the scripted panel actually disagree.
            let verdict = if req.system.contains("Be adversarial") {
                "refuted"
            } else if req.system.contains("Treat uncertainty as danger") {
                "confirmed"
            } else {
                self.verdict
            };
            // Judge: rule on exactly the ids in the case file.
            let rulings: Vec<String> = ids_in(&req.prompt, "#### ")
                .into_iter()
                .take(self.judge_limit.unwrap_or(usize::MAX))
                .map(|(id, altitude)| {
                    let id = if self.invalid_judge_ids { "nonexistent".to_string() } else { id };
                    format!(
                        r#"{{"finding_id":"{id}","verdict":"{verdict}","severity":"high","altitude":"{altitude}","reasoning":"scripted","novel":true}}"#
                    )
                })
                .collect();
            format!(r#"{{"rulings":[{}]}}"#, rulings.join(","))
        } else if req.system.contains("You are tuning the reviewer population")
            || req.system.contains("You are breeding reviewers")
        {
            r#"{"name":"bred reviewer","focus":"structure","prompt":"You look only at whether the change is shaped correctly, reading well beyond the diff to find responsibilities that have landed in the wrong layer."}"#.to_string()
        } else if req.system.contains("rounds of adversarial review") {
            r#"{"themes":[{"title":"Arithmetic on an unvalidated value","severity":"high","altitude":"module","summary":"s","root_cause":"r","recommendation":"x","finding_ids":[]}]}"#.to_string()
        } else if req.system.contains("You defend this change")
            || req.system.contains("supplying the context")
            || req.system.contains("technically true")
        {
            let rs: Vec<String> = ids_in(&req.prompt, "**")
                .into_iter()
                .map(|(id, _)| {
                    format!(r#"{{"finding_id":"{id}","stance":"refute","argument":"scripted","evidence":"e"}}"#)
                })
                .collect();
            format!(r#"{{"rebuttals":[{}]}}"#, rs.join(","))
        } else {
            let fs: Vec<String> = self
                .findings
                .iter()
                .map(|(file, line, title, altitude)| {
                    format!(
                        r#"{{"file":"{file}","line":{line},"category":"c","title":"{title}","claim":"{title}","failure_scenario":"f","evidence":"e","severity":"high","altitude":"{altitude}","confidence":0.8}}"#
                    )
                })
                .collect();
            // Wrapped in prose on purpose: real models do this constantly, and
            // the extractor has to cope.
            format!("Here is my review.\n\n```json\n{{\"findings\":[{}]}}\n```\n", fs.join(","))
        };

        Ok(Response {
            text,
            usage: Usage { input_tokens: 10, output_tokens: 20, cost_usd: 0.01 },
            provider: self.name.clone(),
            model: "scripted".into(),
        })
    }
}

/// Pull finding ids (and the altitude claimed next to them) out of a prompt.
fn ids_in(text: &str, marker: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix(marker) else { continue };
        let token = rest.trim_start_matches('*').split_whitespace().next().unwrap_or("");
        let id = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '-');
        if id.starts_with('f') && id.contains('-') && !out.iter().any(|(e, _)| e == id) {
            let altitude = if text.contains("architectural") { "architectural" } else { "local" };
            out.push((id.to_string(), altitude.to_string()));
        }
    }
    out
}

// --- config ----------------------------------------------------------------

fn provider_stub(name: &str) -> ProviderConfig {
    let mut pc: ProviderConfig = toml::from_str(&format!(
        "name = \"{name}\"\nkind = \"api\"\nwire = \"anthropic\"\n"
    ))
    .unwrap();
    pc.kind = ProviderKind::Api;
    pc
}

fn config(generations: u32, attackers: usize, names: &[&str]) -> Config {
    let mut cfg = Config::default();
    cfg.run.generations = generations;
    cfg.run.parallelism = 4;
    cfg.run.base = "main".into();
    cfg.run.target = "HEAD".into();
    cfg.run.workspaces = locust_core::config::WorkspacePolicy::Never;
    cfg.arena.attackers = attackers;
    cfg.arena.defenders = 1;
    cfg.arena.judge = Some(names[0].to_string());
    cfg.evolution.enabled = generations > 1;
    cfg.evolution.elite = 1;
    cfg.evolution.mutants = 1;
    cfg.evolution.crossovers = 0;
    cfg.evolution.immigrants = 0;
    cfg.providers = names.iter().map(|n| provider_stub(n)).collect();
    cfg
}

fn silent() -> locust_core::Reporter {
    Arc::new(|_: Event| {})
}

// --- tests -----------------------------------------------------------------

#[tokio::test]
async fn a_confirmed_finding_survives_to_the_report() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let cfg = config(1, 2, &["alpha"]);

    let provider: locust_provider::SharedProvider = Arc::new(Scripted::new(
        "alpha",
        vec![("src/pool.rs", 2, "compute() result is doubled without bounds checking", "local")],
    ));

    let arena = Arena::with_providers(cfg.clone(), vcs, vec![provider], silent())?;
    let outcome = arena.run().await?;

    assert_eq!(outcome.generations.len(), 1);
    let gen = &outcome.generations[0];
    assert_eq!(gen.findings.len(), 2, "two reviewers each raised one finding");
    assert!(!gen.rulings.is_empty(), "the judge ruled");
    assert!(gen.rulings.iter().all(|r| r.raw.verdict == Verdict::Confirmed));

    let confirmed = outcome.confirmed_findings();
    assert!(!confirmed.is_empty(), "a confirmed finding reaches the report");
    assert!(!outcome.themes.is_empty(), "synthesis produced a theme");

    let md = locust_core::report::markdown(&outcome, &cfg);
    assert!(md.contains("Arithmetic on an unvalidated value"));
    assert!(md.contains("src/pool.rs"));

    // Artifacts are on disk and parseable.
    let findings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(arena.run_dir().join("gen-0/findings.json"))?)?;
    assert!(findings.as_array().is_some_and(|a| !a.is_empty()));

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

#[tokio::test]
async fn the_same_finding_from_two_providers_is_judged_once_and_counted_twice() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let cfg = config(1, 2, &["alpha", "beta"]);

    // Both vendors describe the same defect in slightly different words.
    let a: locust_provider::SharedProvider = Arc::new(Scripted::new(
        "alpha",
        vec![("src/pool.rs", 2, "compute result doubled without bounds checking", "local")],
    ));
    let b: locust_provider::SharedProvider = Arc::new(Scripted::new(
        "beta",
        vec![("src/pool.rs", 3, "bounds checking missing before compute result is doubled", "local")],
    ));

    let arena = Arena::with_providers(cfg, vcs, vec![a, b], silent())?;
    let outcome = arena.run().await?;
    let gen = &outcome.generations[0];

    assert_eq!(gen.findings.len(), 2);
    let canonical: Vec<_> = gen.findings.iter().filter(|f| f.is_canonical()).collect();
    assert_eq!(canonical.len(), 1, "the duplicate collapsed before judging");
    assert_eq!(canonical[0].corroboration, 2, "both vendors counted");
    assert_eq!(gen.rulings.len(), 1, "judged once, not twice");

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

#[tokio::test]
async fn a_provider_that_fails_shrinks_the_population_without_killing_the_run() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let mut cfg = config(1, 2, &["alpha", "beta"]);
    // Pin judging to the healthy provider so the failure is isolated to attack.
    cfg.arena.judge = Some("alpha".into());

    let a: locust_provider::SharedProvider = Arc::new(Scripted::new(
        "alpha",
        vec![("src/pool.rs", 2, "compute result doubled without bounds checking", "local")],
    ));
    let b: locust_provider::SharedProvider =
        Arc::new(Scripted::new("beta", vec![("src/pool.rs", 9, "unrelated claim", "local")]).failing_after(0));

    let failures = Arc::new(AtomicUsize::new(0));
    let seen = failures.clone();
    let reporter: locust_core::Reporter = Arc::new(move |e: Event| {
        if matches!(e, Event::AgentFailed { .. }) {
            seen.fetch_add(1, Ordering::SeqCst);
        }
    });

    let arena = Arena::with_providers(cfg, vcs, vec![a, b], reporter)?;
    let outcome = arena.run().await?;

    assert!(failures.load(Ordering::SeqCst) > 0, "the failure was reported, not swallowed");
    assert!(
        !outcome.generations[0].findings.is_empty(),
        "the healthy provider's findings still made it through"
    );

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

#[tokio::test]
async fn refuted_findings_leave_the_reviewer_with_a_negative_score() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let cfg = config(1, 2, &["alpha"]);

    let provider: locust_provider::SharedProvider = Arc::new(
        Scripted::new("alpha", vec![("src/pool.rs", 2, "speculative claim", "line")]).refuting(),
    );

    let arena = Arena::with_providers(cfg.clone(), vcs, vec![provider], silent())?;
    let outcome = arena.run().await?;
    let gen = &outcome.generations[0];

    assert!(gen.rulings.iter().all(|r| r.raw.verdict == Verdict::Refuted));
    let scored: Vec<_> = gen.fitness.iter().filter(|f| f.raised > 0).collect();
    assert!(!scored.is_empty());
    assert!(
        scored.iter().all(|f| f.score < 0.0),
        "raising only refuted findings must cost the reviewer"
    );
    assert!(outcome.confirmed_findings().is_empty());

    let md = locust_core::report::markdown(&outcome, &cfg);
    assert!(md.contains("No confirmed findings"));

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

#[tokio::test]
async fn generations_breed_a_new_population_and_ban_settled_ground() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let cfg = config(2, 2, &["alpha"]);

    let provider = Arc::new(Scripted::new(
        "alpha",
        vec![("src/pool.rs", 2, "compute result doubled without bounds checking", "architectural")],
    ));
    let shared: locust_provider::SharedProvider = provider.clone();

    let arena = Arena::with_providers(cfg, vcs, vec![shared], silent())?;
    let outcome = arena.run().await?;

    assert_eq!(outcome.generations.len(), 2);

    // The second generation's population is not the first's.
    let g0: Vec<&str> =
        outcome.generations[0].personas.iter().map(|p| p.name.as_str()).collect();
    let g1: Vec<&str> =
        outcome.generations[1].personas.iter().map(|p| p.name.as_str()).collect();
    assert!(g1.contains(&"bred reviewer"), "a mutant entered the population: {g1:?}");
    assert_ne!(g0, g1);

    // Pressure rose, and the later attack prompts carried the ban list.
    assert_eq!(outcome.generations[0].pressure, 0.0);
    assert_eq!(outcome.generations[1].pressure, 1.0);

    let labels = provider.seen.lock().unwrap().clone();
    assert!(labels.iter().any(|l| l.starts_with("gen1-atk")), "generation 1 ran: {labels:?}");
    assert!(labels.iter().any(|l| l.contains("mutate")), "breeding called the meta-provider");

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

/// With elitism on and a meta-provider that cannot produce a usable persona,
/// breeding used to carry the parent forward into the mutant slot — yielding a
/// generation of two identically-named reviewers that cost twice as much and
/// saw exactly the same things.
#[tokio::test]
async fn breeding_never_fills_a_generation_with_the_same_reviewer_twice() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let mut cfg = config(2, 2, &["alpha"]);
    cfg.evolution.elite = 1;
    cfg.evolution.mutants = 1;
    cfg.evolution.immigrants = 0;

    // Attacks and judging work; only the meta-prompt is broken.
    struct BrokenMeta(Scripted);

    #[async_trait]
    impl Provider for BrokenMeta {
        fn name(&self) -> &str {
            self.0.name()
        }
        fn model(&self) -> &str {
            "scripted"
        }
        async fn complete(&self, req: Request) -> Result<Response> {
            if req.system.contains("You are tuning the reviewer population")
                || req.system.contains("You are breeding reviewers")
            {
                bail!("meta-provider is down");
            }
            self.0.complete(req).await
        }
    }

    let provider: locust_provider::SharedProvider = Arc::new(BrokenMeta(Scripted::new(
        "alpha",
        vec![("src/pool.rs", 2, "compute result doubled without bounds checking", "local")],
    )));

    let arena = Arena::with_providers(cfg, vcs, vec![provider], silent())?;
    let outcome = arena.run().await?;
    assert_eq!(outcome.generations.len(), 2, "a failed mutation must not abort the run");

    for gen in &outcome.generations {
        let names: Vec<&str> = gen
            .personas
            .iter()
            .filter(|p| p.role == Role::Attacker)
            .map(|p| p.name.as_str())
            .collect();
        let unique: std::collections::HashSet<&&str> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "duplicate reviewer in gen {}: {names:?}", gen.generation);

        let ids: Vec<&str> = gen.personas.iter().map(|p| p.id.as_str()).collect();
        let unique_ids: std::collections::HashSet<&&str> = ids.iter().collect();
        assert_eq!(unique_ids.len(), ids.len(), "duplicate persona id: {ids:?}");
    }

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

/// Observed live: the meta-provider rewrote a reviewer's prompt but reused the
/// parent's name, producing "contract breaker, contract breaker". The genuinely
/// new prompt must be kept under a distinct name, not discarded as a collision.
#[tokio::test]
async fn a_mutant_that_reuses_its_parents_name_is_renamed_not_dropped() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let mut cfg = config(2, 2, &["alpha"]);
    cfg.evolution.elite = 1;
    cfg.evolution.mutants = 1;
    cfg.evolution.immigrants = 0;

    /// Always names the mutant after a seed persona already in the population.
    struct NameColliding(Scripted);

    #[async_trait]
    impl Provider for NameColliding {
        fn name(&self) -> &str {
            self.0.name()
        }
        fn model(&self) -> &str {
            "scripted"
        }
        async fn complete(&self, req: Request) -> Result<Response> {
            if req.system.contains("You are tuning the reviewer population") {
                return Ok(Response {
                    text: r#"{"name":"contract breaker","focus":"f","prompt":"You read a change from the outside in, asking only whether the callers that existed yesterday still work today, and you prove it by naming one."}"#.into(),
                    usage: Usage::default(),
                    provider: "alpha".into(),
                    model: "scripted".into(),
                });
            }
            self.0.complete(req).await
        }
    }

    let provider: locust_provider::SharedProvider = Arc::new(NameColliding(Scripted::new(
        "alpha",
        vec![("src/pool.rs", 2, "compute result doubled without bounds checking", "local")],
    )));

    let arena = Arena::with_providers(cfg, vcs, vec![provider], silent())?;
    let outcome = arena.run().await?;

    let g1: Vec<&locust_core::model::Persona> = outcome.generations[1]
        .personas
        .iter()
        .filter(|p| p.role == Role::Attacker)
        .collect();

    let names: Vec<&str> = g1.iter().map(|p| p.name.as_str()).collect();
    let unique: std::collections::HashSet<&&str> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "names must be distinct: {names:?}");
    assert!(
        g1.iter().any(|p| p.origin == locust_core::model::Origin::Mutant),
        "the rewritten prompt was kept, not discarded: {names:?}"
    );

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

/// The whole point of a panel: one judge's verdict is unfalsifiable, three
/// contrasting temperaments turn the spread into a measurement.
#[tokio::test]
async fn a_split_panel_records_who_dissented_and_why() -> Result<()> {
    use locust_core::config::{JudgeConfig, TraitConfig};

    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let mut cfg = config(1, 2, &["alpha"]);
    cfg.arena.judge = None;
    cfg.judges = vec![
        // Refutes everything (very low agreeableness).
        JudgeConfig {
            name: "skeptic".into(),
            provider: None,
            weight: 1.0,
            traits: TraitConfig { preset: Some("skeptic".into()), ..Default::default() },
        },
        // Confirms everything (very high neuroticism).
        JudgeConfig {
            name: "paranoid".into(),
            provider: None,
            weight: 1.0,
            traits: TraitConfig { preset: Some("paranoid".into()), ..Default::default() },
        },
        // Falls back to the scripted default, which confirms.
        JudgeConfig {
            name: "neutral".into(),
            provider: None,
            weight: 1.0,
            traits: TraitConfig::default(),
        },
    ];

    let provider: locust_provider::SharedProvider = Arc::new(Scripted::new(
        "alpha",
        vec![("src/pool.rs", 2, "compute result doubled without bounds checking", "local")],
    ));

    let arena = Arena::with_providers(cfg.clone(), vcs, vec![provider], silent())?;
    let outcome = arena.run().await?;
    let gen = &outcome.generations[0];

    let ruling = gen.rulings.first().expect("a ruling was produced");
    assert_eq!(ruling.panel.len(), 3, "every judge voted");
    assert_eq!(ruling.raw.verdict, Verdict::Confirmed, "2 of 3 confirmed");
    assert!(!ruling.unanimous(), "the skeptic dissented");
    assert_eq!(ruling.tally(), (2, 3));
    assert!((ruling.dissent - 1.0 / 3.0).abs() < 1e-9);
    assert!(
        ruling.panel.iter().any(|v| v.judge == "skeptic" && v.raw.verdict == Verdict::Refuted),
        "the dissent is attributable to a named judge"
    );

    // A contested finding scores less than a unanimous one would.
    let fit = gen.fitness.iter().find(|f| f.confirmed > 0).expect("someone scored");
    assert!(fit.score > 0.0);

    let md = locust_core::report::markdown(&outcome, &cfg);
    assert!(md.contains("Where the panel split"), "the disagreement is reported");
    assert!(md.contains("skeptic"));

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

/// Dropping to one judge must restore exactly the old behaviour.
#[tokio::test]
async fn a_single_judge_panel_is_unanimous_by_definition() -> Result<()> {
    use locust_core::config::{JudgeConfig, TraitConfig};

    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let mut cfg = config(1, 2, &["alpha"]);
    cfg.judges = vec![JudgeConfig {
        name: "solo".into(),
        provider: Some("alpha".into()),
        weight: 1.0,
        traits: TraitConfig::default(),
    }];

    let provider: locust_provider::SharedProvider = Arc::new(Scripted::new(
        "alpha",
        vec![("src/pool.rs", 2, "compute result doubled without bounds checking", "local")],
    ));

    let arena = Arena::with_providers(cfg.clone(), vcs, vec![provider], silent())?;
    let outcome = arena.run().await?;
    let gen = &outcome.generations[0];

    assert!(!gen.rulings.is_empty());
    for r in &gen.rulings {
        assert_eq!(r.panel.len(), 1);
        assert!(r.unanimous());
        assert_eq!(r.dissent, 0.0);
    }
    let md = locust_core::report::markdown(&outcome, &cfg);
    assert!(!md.contains("Where the panel split"), "nothing to split with one judge");

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

/// Both of these were observed in live runs and neither was surfaced by the
/// tool: a run where every agent and judge is the same provider, and a
/// generation where the panel refuted nothing. Each makes the verdicts weaker
/// than they look, and each is silent unless the tool says so.
#[tokio::test]
async fn a_run_that_cannot_judge_itself_neutrally_says_so() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let mut cfg = config(1, 2, &["alpha"]);
    cfg.arena.judge = None;

    // Enough confirmed findings, none refuted: the rubber-stamp case.
    let provider: locust_provider::SharedProvider = Arc::new(Scripted::new(
        "alpha",
        vec![
            ("src/pool.rs", 2, "first distinct defect alpha", "local"),
            ("src/pool.rs", 60, "second distinct defect beta", "local"),
            ("src/pool.rs", 120, "third distinct defect gamma", "local"),
            ("src/pool.rs", 180, "fourth distinct defect delta", "local"),
        ],
    ));

    let warnings = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = warnings.clone();
    let reporter: locust_core::Reporter = Arc::new(move |e: Event| {
        if let Event::Warning { message } = e {
            sink.lock().unwrap().push(message);
        }
    });

    let arena = Arena::with_providers(cfg, vcs, vec![provider], reporter)?;
    arena.run().await?;

    let warnings = warnings.lock().unwrap().clone();
    assert!(
        warnings.iter().any(|w| w.contains("every agent and every judge is `alpha`")),
        "single-provider self-judging must be called out: {warnings:?}"
    );
    assert!(
        warnings.iter().any(|w| w.contains("refuted none of")),
        "a panel that refutes nothing must be called out: {warnings:?}"
    );

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

/// The inverse: a healthy configuration must not nag.
#[tokio::test]
async fn a_well_formed_run_raises_no_validity_warnings() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let mut cfg = config(1, 2, &["alpha", "beta"]);
    cfg.arena.judge = None;

    let a: locust_provider::SharedProvider = Arc::new(
        Scripted::new("alpha", vec![("src/pool.rs", 2, "a defect alpha", "local")]).refuting(),
    );
    let b: locust_provider::SharedProvider = Arc::new(
        Scripted::new("beta", vec![("src/pool.rs", 60, "a defect beta", "local")]).refuting(),
    );

    let warnings = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = warnings.clone();
    let reporter: locust_core::Reporter = Arc::new(move |e: Event| {
        if let Event::Warning { message } = e {
            sink.lock().unwrap().push(message);
        }
    });

    let arena = Arena::with_providers(cfg, vcs, vec![a, b], reporter)?;
    arena.run().await?;

    let warnings = warnings.lock().unwrap().clone();
    assert!(warnings.is_empty(), "nothing to warn about here: {warnings:?}");

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

/// Found by running locust on itself: the provider hit its usage limit, all
/// four reviewers failed, and the run printed "0 findings raised" and exited
/// **0**. A CI gate would have passed a review that never happened.
#[tokio::test]
async fn a_review_where_every_reviewer_failed_is_an_error_not_a_clean_bill() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let cfg = config(1, 2, &["alpha"]);

    let provider: locust_provider::SharedProvider = Arc::new(
        Scripted::new("alpha", vec![("src/pool.rs", 2, "a defect", "local")]).failing_after(0),
    );

    let arena = Arena::with_providers(cfg, vcs, vec![provider], silent())?;
    let err = arena.run().await.unwrap_err().to_string();
    assert!(err.contains("every reviewer failed"), "got: {err}");
    assert!(err.contains("no review was performed"), "got: {err}");

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

/// The inverse must stay true: healthy reviewers that genuinely find nothing
/// are a clean review, not an error.
#[tokio::test]
async fn healthy_reviewers_finding_nothing_is_a_clean_review() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let cfg = config(1, 2, &["alpha"]);

    // Succeeds, reports an empty findings list.
    let provider: locust_provider::SharedProvider = Arc::new(Scripted::new("alpha", vec![]));

    let arena = Arena::with_providers(cfg.clone(), vcs, vec![provider], silent())?;
    let outcome = arena.run().await?;
    assert!(outcome.generations[0].findings.is_empty());
    assert!(outcome.confirmed_findings().is_empty());
    assert!(locust_core::report::markdown(&outcome, &cfg).contains("No confirmed findings"));

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

/// Partial failure degrades rather than aborting, but must not do so silently.
#[tokio::test]
async fn partial_reviewer_failure_warns_about_the_smaller_population() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let mut cfg = config(1, 2, &["alpha", "beta"]);
    cfg.arena.judge = Some("alpha".into());

    let a: locust_provider::SharedProvider = Arc::new(Scripted::new(
        "alpha",
        vec![("src/pool.rs", 2, "a real defect alpha", "local")],
    ));
    let b: locust_provider::SharedProvider =
        Arc::new(Scripted::new("beta", vec![]).failing_after(0));

    let warnings = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = warnings.clone();
    let reporter: locust_core::Reporter = Arc::new(move |e: Event| {
        if let Event::Warning { message } = e {
            sink.lock().unwrap().push(message);
        }
    });

    let arena = Arena::with_providers(cfg, vcs, vec![a, b], reporter)?;
    let outcome = arena.run().await?;
    assert!(!outcome.generations[0].findings.is_empty(), "the healthy half still reviewed");

    let warnings = warnings.lock().unwrap().clone();
    assert!(
        warnings.iter().any(|w| w.contains("reviewers failed")),
        "a halved population must be stated: {warnings:?}"
    );

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

/// Raised by the self-review: aborting on a totally-failed generation threw
/// away every generation that had already completed. Earlier generations are
/// finished work and cost real money; losing them to report a later failure is
/// the wrong trade.
#[tokio::test]
async fn a_late_generation_failing_still_reports_the_earlier_ones() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let cfg = config(3, 2, &["alpha"]);

    /// Healthy for generation 0, dead for every attack afterwards.
    struct DiesAfterFirstGeneration(Scripted);

    #[async_trait]
    impl Provider for DiesAfterFirstGeneration {
        fn name(&self) -> &str {
            self.0.name()
        }
        fn model(&self) -> &str {
            "scripted"
        }
        async fn complete(&self, req: Request) -> Result<Response> {
            if req.label.starts_with("gen1-atk") || req.label.starts_with("gen2-atk") {
                bail!("provider died");
            }
            self.0.complete(req).await
        }
    }

    let provider: locust_provider::SharedProvider =
        Arc::new(DiesAfterFirstGeneration(Scripted::new(
            "alpha",
            vec![("src/pool.rs", 2, "compute result doubled without bounds", "local")],
        )));

    let warnings = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = warnings.clone();
    let reporter: locust_core::Reporter = Arc::new(move |e: Event| {
        if let Event::Warning { message } = e {
            sink.lock().unwrap().push(message);
        }
    });

    let arena = Arena::with_providers(cfg.clone(), vcs, vec![provider], reporter)?;
    let outcome = arena.run().await.expect("the completed generation is still a result");

    assert_eq!(outcome.generations.len(), 1, "generation 0 survived the later failure");
    assert!(!outcome.confirmed_findings().is_empty(), "and its findings are reported");

    let warnings = warnings.lock().unwrap().clone();
    assert!(
        warnings.iter().any(|w| w.contains("generation(s) that completed")),
        "the truncation must be stated: {warnings:?}"
    );

    // The report still renders, and says what it is.
    let md = locust_core::report::markdown(&outcome, &cfg);
    assert!(md.contains("Confirmed findings"));

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

/// But a run whose *first* generation fails entirely has nothing to report,
/// and must stay an error rather than producing an empty clean bill.
#[tokio::test]
async fn a_first_generation_failing_entirely_is_still_fatal() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let cfg = config(3, 2, &["alpha"]);

    let provider: locust_provider::SharedProvider = Arc::new(
        Scripted::new("alpha", vec![("src/pool.rs", 2, "a defect", "local")]).failing_after(0),
    );

    let arena = Arena::with_providers(cfg, vcs, vec![provider], silent())?;
    let err = arena.run().await.unwrap_err().to_string();
    assert!(err.contains("every reviewer failed"), "got: {err}");

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

/// A reviewer under pressure to produce findings will cite a line past the end
/// of the file, or quote code it composed rather than read. Those findings read
/// exactly like real ones, so the citation is checked mechanically and the
/// result put in front of the defender and the panel.
#[tokio::test]
async fn a_finding_citing_code_that_does_not_exist_is_marked() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let cfg = config(1, 2, &["alpha"]);

    let provider: locust_provider::SharedProvider = Arc::new(Scripted::new(
        "alpha",
        vec![
            // Real line in a real file.
            ("src/pool.rs", 2, "a grounded defect in the change", "local"),
            // A file the change never touched and which does not exist.
            ("src/imaginary.rs", 5, "a defect in a file that is not there", "local"),
            // Real file, line far past its end.
            ("src/pool.rs", 99999, "a defect at an impossible line", "local"),
        ],
    ));

    let warnings = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = warnings.clone();
    let reporter: locust_core::Reporter = Arc::new(move |e: Event| {
        if let Event::Warning { message } = e {
            sink.lock().unwrap().push(message);
        }
    });

    let arena = Arena::with_providers(cfg, vcs, vec![provider], reporter)?;
    let outcome = arena.run().await?;
    let findings = &outcome.generations[0].findings;

    let by_file = |name: &str, line: u32| {
        findings
            .iter()
            .find(|f| f.raw.file == name && f.raw.line == Some(line))
            .unwrap_or_else(|| panic!("no finding for {name}:{line}"))
    };

    assert_eq!(by_file("src/pool.rs", 2).grounding, Grounding::Verified);
    assert_eq!(by_file("src/imaginary.rs", 5).grounding, Grounding::FileNotFound);
    assert!(matches!(
        by_file("src/pool.rs", 99999).grounding,
        Grounding::LineOutOfRange { cited: 99999, .. }
    ));

    // The reviewer's scorecard records it, and the run says so out loud.
    let fitness: usize = outcome.generations[0].fitness.iter().map(|f| f.ungrounded).sum();
    assert!(fitness >= 2, "ungrounded findings are counted against their author");
    assert!(
        warnings.lock().unwrap().iter().any(|w| w.contains("could not be located")),
        "the run must say that citations failed to resolve"
    );

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

/// Exclusions have to remove content from the diff text, not just from the
/// file list -- otherwise an excluded lockfile or vendored tree is still
/// shipped to every provider inside the diff.
#[tokio::test]
async fn excluded_files_are_absent_from_the_diff_itself() -> Result<()> {
    let repo = fixture_repo()?;
    std::fs::write(repo.join("secrets.env"), "API_TOKEN=hunter2-do-not-leak\n")?;
    std::fs::write(repo.join("Cargo.lock"), "name = \"generated\"\n")?;
    git(&repo, &["add", "-A"])?;
    git(&repo, &["commit", "-qm", "add excluded files"])?;

    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let mut ctx = locust_core::config::ContextConfig::default();
    ctx.exclude.push("secrets.env".into());

    let range = vcs.range("main", "HEAD")?;
    let pack = locust_core::context::ContextPack::build(vcs.as_ref(), &range, &ctx)?;

    assert!(!pack.files.iter().any(|f| f.path == "secrets.env"), "excluded from the file list");
    assert!(!pack.diff.contains("hunter2-do-not-leak"), "secret leaked in the diff text");
    assert!(!pack.diff.contains("secrets.env"), "excluded path leaked in the diff text");
    assert!(!pack.diff.contains("Cargo.lock"), "default exclusion leaked");

    // And nothing about it survives into what agents are actually shown.
    let rendered = pack.render(true);
    assert!(!rendered.contains("hunter2-do-not-leak"));
    assert!(rendered.contains("src/pool.rs"), "the real change is still there");

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

/// Raised by the two-provider review, and the third phase to need this guard:
/// when every judge call failed, `judge()` returned no rulings, the report said
/// "No confirmed findings" and `--fail-on` passed. Nothing judged is not the
/// same as nothing confirmed.
#[tokio::test]
async fn a_generation_where_every_judge_failed_is_an_error() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let cfg = config(1, 2, &["alpha"]);

    /// Reviewers and defenders work; only judging is dead.
    struct JudgingIsDown(Scripted);

    #[async_trait]
    impl Provider for JudgingIsDown {
        fn name(&self) -> &str {
            self.0.name()
        }
        fn model(&self) -> &str {
            "scripted"
        }
        async fn complete(&self, req: Request) -> Result<Response> {
            if req.system.contains("adjudicating a code review") {
                bail!("judging is unavailable");
            }
            self.0.complete(req).await
        }
    }

    let provider: locust_provider::SharedProvider = Arc::new(JudgingIsDown(Scripted::new(
        "alpha",
        vec![("src/pool.rs", 2, "a defect worth judging", "local")],
    )));

    let arena = Arena::with_providers(cfg, vcs, vec![provider], silent())?;
    let err = arena.run().await.unwrap_err().to_string();
    assert!(err.contains("every judge failed"), "got: {err}");
    assert!(err.contains("no judging was performed"), "got: {err}");

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

/// Raised by the same review: every caveat the run raised existed only as a
/// transient string on the reporter callback, so the durable record showed
/// verdicts without the qualifications the run itself had made.
#[tokio::test]
async fn caveats_reach_the_durable_record_not_just_the_terminal() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let cfg = config(1, 2, &["alpha"]);

    let provider: locust_provider::SharedProvider = Arc::new(Scripted::new(
        "alpha",
        vec![("src/pool.rs", 99999, "cites a line that is not there", "local")],
    ));

    let arena = Arena::with_providers(cfg.clone(), vcs, vec![provider], silent())?;
    let outcome = arena.run().await?;

    assert!(!outcome.warnings.is_empty(), "warnings must survive into the outcome");
    assert!(
        outcome.warnings.iter().any(|w| w.contains("every agent and every judge")),
        "the single-provider caveat is part of the record: {:?}",
        outcome.warnings
    );

    // And they survive a round trip through the stored JSON, which is what
    // `--json` and `report --render` read.
    let json = serde_json::to_string(&outcome)?;
    let back: locust_core::RunOutcome = serde_json::from_str(&json)?;
    assert_eq!(back.warnings, outcome.warnings);

    let md = locust_core::report::markdown(&outcome, &cfg);
    assert!(md.contains("Caveats from this run"), "and are rendered before the findings");

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

#[tokio::test]
async fn an_empty_diff_is_an_error_not_an_empty_review() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let mut cfg = config(1, 1, &["alpha"]);
    cfg.run.base = "HEAD".into();
    cfg.run.target = "HEAD".into();

    let provider: locust_provider::SharedProvider = Arc::new(Scripted::new("alpha", vec![]));
    let arena = Arena::with_providers(cfg, vcs, vec![provider], silent())?;
    let err = arena.run().await.unwrap_err().to_string();
    assert!(err.contains("no reviewable changes"), "got: {err}");

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

/// Role assignment must produce the altitude/severity ordering the report
/// relies on, independent of the order the judge happened to emit rulings in.
#[tokio::test]
async fn confirmed_findings_are_ranked_for_a_reader() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let cfg = config(1, 2, &["alpha"]);

    let provider: locust_provider::SharedProvider = Arc::new(Scripted::new(
        "alpha",
        vec![
            ("src/pool.rs", 2, "doubling is unchecked", "architectural"),
            ("src/pool.rs", 40, "a wholly different local problem here", "local"),
        ],
    ));

    let arena = Arena::with_providers(cfg, vcs, vec![provider], silent())?;
    let outcome = arena.run().await?;
    let ranked = outcome.confirmed_findings();
    assert!(ranked.len() >= 2);

    for pair in ranked.windows(2) {
        let (_, a) = pair[0];
        let (_, b) = pair[1];
        assert!(
            (a.raw.severity, a.raw.altitude) >= (b.raw.severity, b.raw.altitude),
            "report order must be worst-first"
        );
    }
    assert_eq!(ranked[0].1.raw.altitude, Altitude::Architectural);
    assert_eq!(ranked[0].1.raw.severity, Severity::High);
    let _ = Role::Attacker;

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}

#[tokio::test]
async fn missing_judgments_cannot_pass_a_review() -> Result<()> {
    for (limit, invalid) in [(Some(0), false), (None, true), (Some(1), false)] {
        let repo = fixture_repo()?;
        let vcs = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
        let cfg = config(1, 1, &["alpha"]);
        let mut provider = Scripted::new("alpha", vec![
            ("src/pool.rs", 2, "arithmetic overflow", "local"),
            ("src/pool.rs", 150, "unbounded resource retention", "module"),
        ]);
        provider.judge_limit = limit;
        provider.invalid_judge_ids = invalid;
        let arena = Arena::with_providers(cfg, vcs, vec![Arc::new(provider)], silent())?;
        let err = arena.run().await.unwrap_err().to_string();
        assert!(err.contains("every judge failed"), "{err}");
        let warnings = std::fs::read_to_string(arena.run_dir().join("warnings.json"))?;
        assert!(warnings.contains("incomplete judgment"), "{warnings}");
        std::fs::remove_dir_all(repo).ok();
    }
    Ok(())
}

#[tokio::test]
async fn only_refuted_decoys_receive_defence_credit() -> Result<()> {
    use locust_core::bench::{Case, CaseManifest, Defect, DefectKind, Scorecard};
    let repo = fixture_repo()?;
    let vcs = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let cfg = config(1, 1, &["alpha"]);
    let provider = Scripted::new("alpha", vec![("src/pool.rs", 2, "false alarm", "local")]).refuting();
    let arena = Arena::with_providers(cfg.clone(), vcs, vec![Arc::new(provider)], silent())?;
    let mut outcome = arena.run().await?;
    let case = Case {
        dir: repo.clone(),
        manifest: CaseManifest {
            name: "test".into(), description: String::new(), language: String::new(), notes: String::new(),
            defects: vec![Defect {
                id: "decoy".into(), kind: DefectKind::Decoy, file: "src/pool.rs".into(),
                lines: None, severity: Severity::Low, summary: "false alarm".into(),
                matches: vec!["false alarm".into()], why_safe: "safe".into(),
            }],
        },
    };
    assert_eq!(Scorecard::build(&case, &outcome).decoys_caught, vec!["decoy"]);
    outcome.generations[0].rulings[0].raw.verdict = Verdict::Undecided;
    let card = Scorecard::build(&case, &outcome);
    assert!(card.decoys_caught.is_empty());
    assert_eq!(card.decoy_resistance(), None);
    let md = locust_core::report::markdown(&outcome, &cfg);
    assert!(!md.contains("Every finding raised was refuted"));
    outcome.generations[0].rulings.clear();
    assert!(Scorecard::build(&case, &outcome).decoys_caught.is_empty());
    std::fs::remove_dir_all(repo).ok();
    Ok(())
}

#[tokio::test]
async fn judgment_gaps_fail_across_batches_and_later_generations() -> Result<()> {
    struct Gap(Scripted, bool);
    #[async_trait]
    impl Provider for Gap {
        fn name(&self) -> &str { self.0.name() }
        fn model(&self) -> &str { self.0.model() }
        async fn complete(&self, req: Request) -> Result<Response> {
            let gap = req.label.contains("judge") && if self.1 {
                req.label.starts_with("gen1-")
            } else {
                req.label.ends_with("-b0")
            };
            let mut response = self.0.complete(req).await?;
            if gap { response.text = r#"{"rulings":[]}"#.into(); }
            Ok(response)
        }
    }
    for late in [false, true] {
        let repo = fixture_repo()?;
        let vcs = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
        let mut cfg = config(if late { 2 } else { 1 }, 1, &["alpha"]);
        cfg.evolution.enabled = false;
        cfg.arena.judge_batch = 1;
        let provider = Gap(Scripted::new("alpha", vec![
            ("src/pool.rs", 2, "arithmetic overflow", "local"),
            ("src/pool.rs", 150, "unbounded resource retention", "module"),
        ]), late);
        let arena = Arena::with_providers(cfg, vcs, vec![Arc::new(provider)], silent())?;
        let err = arena.run().await.unwrap_err().to_string();
        assert!(err.contains("incomplete judgment"), "{err}");
        if late {
            let saved: locust_core::RunOutcome = serde_json::from_str(
                &std::fs::read_to_string(arena.run_dir().join("report.json"))?)?;
            assert_eq!(saved.generations.len(), 1, "completed work survives");
        }
        std::fs::remove_dir_all(repo).ok();
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn every_cli_phase_uses_the_target_checkout_and_wrapper_workspace() -> Result<()> {
    use locust_provider::cli::{CliProvider, CliSpec, Extract};
    struct CheckedCli {
        scripted: Scripted,
        cli: CliProvider,
        target: String,
        phases: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl Provider for CheckedCli {
        fn name(&self) -> &str { self.scripted.name() }
        fn model(&self) -> &str { self.scripted.model() }
        fn needs_workspace(&self) -> bool { true }
        async fn complete(&self, req: Request) -> Result<Response> {
            let workspace = req.workspace.as_ref().expect("every phase has a workspace");
            let checked = self.cli.complete(req.clone()).await?;
            let lines: Vec<_> = checked.text.lines().collect();
            assert_eq!(lines.len(), 3);
            assert_eq!(Path::new(lines[0]).canonicalize()?, workspace.canonicalize()?);
            assert_eq!(lines[1], self.target.trim(), "{} inspected the wrong revision", req.label);
            assert_eq!(Path::new(lines[2]).canonicalize()?, workspace.canonicalize()?);
            self.phases.lock().unwrap().push(req.label.clone());
            self.scripted.complete(req).await
        }
    }
    let repo = fixture_repo()?;
    let target = git(&repo, &["rev-parse", "feature"])?;
    git(&repo, &["checkout", "-q", "main"])?;
    let vcs = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let mut cfg = config(2, 2, &["alpha"]);
    cfg.run.target = "feature".into();
    cfg.run.workspaces = locust_core::config::WorkspacePolicy::Auto;
    let cli = CliProvider::new(locust_provider::ProviderSettings { timeout_secs: 5, ..Default::default() }, CliSpec {
        command: vec!["sh".into(), "-c".into(), "pwd; git rev-parse HEAD; printf '%s\\n' \"$LOCUST_TEST_WORKSPACE\"".into()],
        stdin: None, extract: Extract::Raw, usage_path: None, env: Default::default(),
        wrapper: vec!["env".into(), "LOCUST_TEST_WORKSPACE={workspace}".into()],
    })?;
    let provider = Arc::new(CheckedCli {
        scripted: Scripted::new("alpha", vec![("src/pool.rs", 2, "a defect", "local")]),
        cli, target, phases: Mutex::new(Vec::new()),
    });
    let arena = Arena::with_providers(cfg, vcs, vec![provider.clone()], silent())?;
    arena.run().await?;
    let phases = provider.phases.lock().unwrap();
    for phase in ["-atk-", "-def-", "-judge", "synthesis", "/mutate/"] {
        assert!(phases.iter().any(|label| label.contains(phase)), "missing {phase}: {phases:?}");
    }
    std::fs::remove_dir_all(repo).ok();
    Ok(())
}

#[test]
fn defender_skeptic_traits_target_the_code_so_claim_experiments_use_neutral_traits() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs = locust_vcs::detect(&repo, Some("git"))?;
    let cfg = config(1, 1, &["alpha"]);
    let range = vcs.range("main", "HEAD")?;
    let pack = locust_core::context::ContextPack::build(vcs.as_ref(), &range, &cfg.context)?;
    let mut defender = cfg.seed_personas(Role::Defender)?[0].clone();
    defender.traits = locust_core::traits::Traits::preset("skeptic").unwrap();
    let prompt = locust_core::prompts::defend(&defender, &pack, &[], false);
    assert!(prompt.system.contains("Be hostile to this code."));
    defender.traits = locust_core::traits::Traits::NEUTRAL;
    defender.prompt = "Demand evidence for each claim; concede true defects.".into();
    let prompt = locust_core::prompts::defend(&defender, &pack, &[], false);
    assert!(!prompt.system.contains("Be hostile to this code."));
    assert!(prompt.system.contains(&defender.prompt));
    std::fs::remove_dir_all(repo).ok();
    Ok(())
}

// --- review of the handoff's F1 implementation -----------------------------

/// The handoff states its intent plainly: "fail a late judging abort after
/// saving earlier completed generations." Its all-judges-failed path does that
/// via `GenerationAborted`, which the run loop downcasts and breaks on.
///
/// Its second coverage check, in `generation()`, used a plain `bail!`. The run
/// loop cannot downcast that, so it took `Err(other) => return Err(other)` and
/// returned before writing any report — discarding generations that had
/// already completed and been paid for.
///
/// Reachable whenever judging is split across batches and one batch loses
/// every judge while another succeeds: the failed/total count stays partial,
/// so the all-judges-failed guard does not fire, yet some finding has no
/// ruling.
#[tokio::test]
async fn a_late_coverage_failure_still_reports_completed_generations() -> Result<()> {
    let repo = fixture_repo()?;
    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let mut cfg = config(2, 2, &["alpha"]);
    cfg.evolution.enabled = false;
    // One finding per judge call, so the two findings land in separate batches.
    cfg.arena.judge_batch = 1;

    /// Judges everything except the second batch of the second generation,
    /// which is what a batch-size-dependent timeout looks like.
    struct AbandonsOneLateBatch(Scripted);

    #[async_trait]
    impl Provider for AbandonsOneLateBatch {
        fn name(&self) -> &str {
            self.0.name()
        }
        fn model(&self) -> &str {
            "scripted"
        }
        async fn complete(&self, req: Request) -> Result<Response> {
            let judging = req.system.contains("adjudicating a code review");
            if judging && req.label.starts_with("gen1-") && req.label.ends_with("-b1") {
                return Ok(Response {
                    text: r#"{"rulings":[]}"#.into(),
                    usage: Usage::default(),
                    provider: "alpha".into(),
                    model: "scripted".into(),
                });
            }
            self.0.complete(req).await
        }
    }

    let provider: locust_provider::SharedProvider = Arc::new(AbandonsOneLateBatch(Scripted::new(
        "alpha",
        vec![
            ("src/pool.rs", 2, "first distinct defect alpha", "local"),
            ("src/pool.rs", 150, "second distinct defect beta", "local"),
        ],
    )));

    let arena = Arena::with_providers(cfg, vcs, vec![provider], silent())?;
    let run_dir = arena.run_dir().to_path_buf();
    let result = arena.run().await;

    // Failing is right: a review that did not judge everything must not pass.
    assert!(result.is_err(), "incomplete judgment must fail the run");

    // Generation 0 was judged, scored and written. That work must stay
    // readable rather than being thrown away to report a later failure.
    assert!(run_dir.join("gen-0/rulings.json").exists(), "generation 0 was written");
    assert!(
        run_dir.join("report.md").exists(),
        "no report, so the completed generation is unreadable: {:?}",
        result.as_ref().err().map(|e| e.to_string())
    );
    let report = std::fs::read_to_string(run_dir.join("report.md"))?;
    assert!(report.contains("Caveats from this run"), "and it carries the caveat");
    assert!(report.contains("incomplete judgment"), "naming what went wrong: {report:.400}");

    std::fs::remove_dir_all(&repo).ok();
    Ok(())
}
