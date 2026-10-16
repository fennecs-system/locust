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
use locust_core::model::{Altitude, Role, Severity, Verdict};
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

    std::fs::write(dir.join("src/pool.rs"), "pub fn take() -> u32 {\n    0\n}\n")?;
    git(&dir, &["add", "-A"])?;
    git(&dir, &["commit", "-qm", "initial"])?;

    // The change lands on a branch, so `main` and `HEAD` differ.
    git(&dir, &["checkout", "-q", "-b", "feature"])?;
    std::fs::write(
        dir.join("src/pool.rs"),
        "pub fn take() -> u32 {\n    let n = compute();\n    n * 2\n}\n\nfn compute() -> u32 {\n    7\n}\n",
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
                .map(|(id, altitude)| {
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
