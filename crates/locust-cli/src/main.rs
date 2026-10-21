//! `locust` — adversarial, evolutionary code review.

mod init;
mod term;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use locust_core::arena::Arena;
use locust_core::config::{Config, VcsChoice, CONFIG_FILENAME};
use locust_core::model::Role;
use locust_core::store;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "locust",
    version,
    about = "Adversarial, evolutionary code review across multiple model providers",
    long_about = "Reviewers compete to find real defects. A defender tries to destroy \
                  everything they raise, an impartial judge rules on what survives, and the \
                  prompts that score well breed into the next generation. Each round weights \
                  structural findings more heavily than line-level ones, so what reaches you \
                  is the handful of issues that survived an argument."
)]
struct Cli {
    /// Path to locust.toml. Defaults to the nearest one up the tree.
    #[arg(long, short, global = true)]
    config: Option<PathBuf>,

    /// Repository to review.
    #[arg(long, global = true, default_value = ".")]
    repo: PathBuf,

    #[arg(long, short, global = true, help = "Log agent-level detail to stderr")]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a locust.toml, configured for the providers available here.
    Init {
        /// Overwrite an existing config.
        #[arg(long)]
        force: bool,
    },

    /// Run the arena over a change.
    Review {
        /// Revision to measure against.
        #[arg(long, short)]
        base: Option<String>,
        /// Revision under review.
        #[arg(long, short)]
        target: Option<String>,
        /// Attack/defend/judge cycles.
        #[arg(long, short)]
        generations: Option<u32>,
        /// Reviewers per generation.
        #[arg(long)]
        attackers: Option<usize>,
        /// Agents in flight at once.
        #[arg(long, short)]
        parallelism: Option<usize>,
        /// Restrict to these providers.
        #[arg(long, value_delimiter = ',')]
        providers: Option<Vec<String>>,
        /// Print the plan and exit without calling any model.
        #[arg(long)]
        dry_run: bool,
        /// Emit the run as JSON on stdout instead of a terminal report.
        #[arg(long)]
        json: bool,
        /// Exit non-zero if a confirmed finding reaches this severity.
        #[arg(long, value_name = "SEVERITY")]
        fail_on: Option<String>,
    },

    /// Show configured providers and whether they are reachable.
    Providers,

    /// Show the reviewer population.
    Personas {
        /// Show full prompts, not just names.
        #[arg(long)]
        full: bool,
    },

    /// Show the judging panel and what each temperament does.
    Judges {
        /// Show the prompt text each judge's traits produce.
        #[arg(long)]
        full: bool,
    },

    /// Print the report from a previous run.
    Report {
        /// Run id. Defaults to the most recent.
        run: Option<String>,
        /// Rebuild the report from the run's stored data instead of reading the
        /// markdown it wrote at the time. Lets an improved report format apply
        /// to runs that already happened.
        #[arg(long)]
        render: bool,
    },

    /// Run the arena against cases with known answers.
    ///
    /// Without ground truth a run that confirms everything is unreadable: it
    /// might mean the reviewers are excellent, or that nobody pushed back.
    Bench {
        /// Only this case.
        #[arg(long)]
        case: Option<String>,
        /// Where the cases live.
        #[arg(long, default_value = "cases")]
        cases_dir: PathBuf,
        /// Generations per case.
        #[arg(long, short, default_value_t = 1)]
        generations: u32,
        /// Reviewers per generation.
        #[arg(long)]
        attackers: Option<usize>,
        /// Emit the scorecards as JSON.
        #[arg(long)]
        json: bool,
        /// List the cases and what they plant, without running anything.
        #[arg(long)]
        list: bool,
        /// Re-score a stored run against the current manifests instead of
        /// running the arena. Takes a run directory containing report.json.
        /// Costs nothing, and is how a manifest fix is checked against the
        /// findings that exposed it.
        #[arg(long, value_name = "RUN_DIR")]
        replay: Option<PathBuf>,
    },

    /// Remove agent worktrees left behind by an interrupted run.
    Clean,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LOCUST_LOG").unwrap_or_else(|_| {
                if cli.verbose { "locust=debug".into() } else { "locust=warn".into() }
            }),
        )
        .with_writer(std::io::stderr)
        .without_time()
        .init();

    // Agents run in their own process groups so a timeout can take their
    // subprocesses with them; the cost is that Ctrl-C no longer reaches them.
    // Without this an interrupted run leaves every in-flight agent running.
    #[cfg(unix)]
    tokio::spawn(async {
        let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            Ok(s) => s,
            Err(_) => return,
        };
        let interrupted = tokio::select! {
            _ = tokio::signal::ctrl_c() => true,
            _ = term.recv() => true,
        };
        if interrupted {
            eprintln!("\n{} stopping agents", term::yellow("interrupted"));
            locust_provider::cli::kill_active_agents();
            // 128 + SIGINT, the conventional shell encoding.
            std::process::exit(130);
        }
    });

    if let Err(e) = run(cli).await {
        eprintln!("\n{} {e:#}", term::red("error:"));
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let repo = cli.repo.canonicalize().unwrap_or_else(|_| cli.repo.clone());

    match cli.command {
        Command::Init { force } => init::write_config(&repo, force),

        Command::Bench { case, cases_dir, generations, attackers, json, list, replay } => {
            if let Some(dir) = replay {
                return replay_bench(&cases_dir, case.as_deref(), &dir, json);
            }
            let cfg = load_config(&cli.config, &repo)?;
            bench(&cfg, &cases_dir, case.as_deref(), generations, attackers, json, list, cli.verbose)
                .await
        }

        Command::Clean => {
            let vcs = locust_vcs::detect(&repo, None)?;
            vcs.prune_all()?;
            println!("{} agent worktrees removed", term::green("ok"));
            Ok(())
        }

        Command::Report { run, render } => report(&repo, run.as_deref(), render),

        Command::Providers => {
            let cfg = load_config(&cli.config, &repo)?;
            providers(&cfg)
        }

        Command::Personas { full } => {
            let cfg = load_config(&cli.config, &repo)?;
            personas(&cfg, full)
        }

        Command::Judges { full } => {
            let cfg = load_config(&cli.config, &repo)?;
            judges(&cfg, full)
        }

        Command::Review {
            base,
            target,
            generations,
            attackers,
            parallelism,
            providers: only,
            dry_run,
            json,
            fail_on,
        } => {
            let mut cfg = load_config(&cli.config, &repo)?;
            if let Some(v) = base {
                cfg.run.base = v;
            }
            if let Some(v) = target {
                cfg.run.target = v;
            }
            if let Some(v) = generations {
                cfg.run.generations = v;
            }
            if let Some(v) = attackers {
                cfg.arena.attackers = v;
            }
            if let Some(v) = parallelism {
                cfg.run.parallelism = v;
            }
            if let Some(v) = only {
                cfg.run.providers = v;
            }
            cfg.validate()?;

            let prefer = match cfg.run.vcs {
                VcsChoice::Auto => None,
                VcsChoice::Git => Some("git"),
                VcsChoice::Jj => Some("jj"),
            };
            let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, prefer)?);

            if dry_run {
                return plan(&cfg, vcs.as_ref());
            }

            let reporter = term::reporter(cli.verbose);
            let arena = Arena::new(cfg.clone(), vcs, reporter)?;
            let run_dir = arena.run_dir().to_path_buf();
            let outcome = arena.run().await?;

            if json {
                println!("{}", serde_json::to_string_pretty(&outcome)?);
            } else {
                println!("\n{}", locust_core::report::markdown(&outcome, &cfg));
                println!("{} {}", term::dim("artifacts:"), run_dir.display());
            }

            if let Some(threshold) = fail_on {
                let threshold = parse_severity(&threshold)?;
                let hits = outcome
                    .confirmed_findings()
                    .into_iter()
                    .filter(|(_, r)| r.raw.severity >= threshold)
                    .count();
                if hits > 0 {
                    bail!("{hits} confirmed finding(s) at or above `{threshold}`");
                }
            }
            Ok(())
        }
    }
}

/// Re-score runs that are already on disk.
///
/// A manifest is an answer key, and the two times this one was wrong the
/// evidence was a stored run whose findings it mis-filed. Re-running the arena
/// to check a keyword fix costs real money and produces different findings, so
/// it cannot answer the question. This replays the exact findings through the
/// current `classify`.
///
/// `RUN_DIR` is either one run directory or a parent of several; each must
/// hold a `report.json`. The case is taken from `--case`, or from the name of
/// the directory two levels above the run when it matches one.
fn replay_bench(
    cases_dir: &std::path::Path,
    only: Option<&str>,
    dir: &std::path::Path,
    json: bool,
) -> Result<()> {
    use locust_core::bench::{Case, Scorecard};
    use locust_core::model::RunOutcome;

    // A bench workdir nests the reports four levels down
    // (`<case>/.locust/runs/<run-id>/`), so search rather than guess. Bounded
    // because a mistyped path should fail, not crawl a home directory.
    fn collect(dir: &std::path::Path, depth: usize, out: &mut Vec<PathBuf>) {
        let report = dir.join("report.json");
        if report.is_file() {
            out.push(report);
            return;
        }
        if depth == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut dirs: Vec<PathBuf> =
            entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for d in dirs {
            collect(&d, depth - 1, out);
        }
    }

    let mut reports: Vec<PathBuf> = Vec::new();
    collect(dir, 5, &mut reports);
    if reports.is_empty() {
        bail!("no report.json under {} (searched five levels)", dir.display());
    }

    let cases = Case::discover(cases_dir)?;
    let mut cards = Vec::new();
    for report in &reports {
        let text = std::fs::read_to_string(report)
            .with_context(|| format!("reading {}", report.display()))?;
        let outcome: RunOutcome = serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", report.display()))?;

        // `.locust/runs/<run-id>/report.json` sits four levels below the
        // repository the bench materialised, which is named for the case.
        let inferred = report
            .ancestors()
            .nth(4)
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_string);
        let name = only.map(str::to_string).or(inferred);
        let Some(case) = cases
            .iter()
            .find(|c| Some(&c.manifest.name) == name.as_ref())
        else {
            eprintln!(
                "  {} {}: no case matches `{}`",
                term::yellow("skipped"),
                report.display(),
                name.as_deref().unwrap_or("?")
            );
            continue;
        };
        eprintln!(
            "{} {} from {}",
            term::bold("replay"),
            case.manifest.name,
            report.display()
        );
        cards.push(Scorecard::build(case, &outcome));
    }
    if cards.is_empty() {
        bail!("nothing to replay: no stored run matched a known case");
    }
    print_scorecards(&cards, json)
}

#[allow(clippy::too_many_arguments)]
async fn bench(
    cfg: &Config,
    cases_dir: &std::path::Path,
    only: Option<&str>,
    generations: u32,
    attackers: Option<usize>,
    json: bool,
    list_only: bool,
    verbose: bool,
) -> Result<()> {
    use locust_core::bench::{Case, Scorecard};

    let cases: Vec<Case> = Case::discover(cases_dir)?
        .into_iter()
        .filter(|c| only.is_none_or(|n| c.manifest.name == n))
        .collect();
    if cases.is_empty() {
        bail!("no cases found in {}", cases_dir.display());
    }

    if list_only {
        for c in &cases {
            println!(
                "\n{} {}",
                term::bold(&c.manifest.name),
                term::dim(&c.manifest.description)
            );
            for d in &c.manifest.defects {
                let tag = term::red("defect");
                // One line each: summaries are paragraphs in the manifest.
                let gist = d.summary.split_whitespace().collect::<Vec<_>>().join(" ");
                let gist: String = gist.chars().take(88).collect();
                println!("  {tag} {:<30} {}", d.id, term::dim(&gist));
            }
        }
        return Ok(());
    }

    // Cases are built outside the repository under review so the arena never
    // sees locust's own source instead of the case's.
    let workdir = std::env::temp_dir().join(format!("locust-bench-{}", std::process::id()));
    std::fs::create_dir_all(&workdir)?;

    let mut cards = Vec::new();
    for case in &cases {
        eprintln!("\n{} {}", term::bold("case"), case.manifest.name);
        let repo = case.materialise(&workdir)?;

        let mut cfg = cfg.clone();
        cfg.run.base = "main".into();
        cfg.run.target = "change".into();
        cfg.run.generations = generations;
        if let Some(a) = attackers {
            cfg.arena.attackers = a;
        }
        // The case's own notes replace the host project's.
        cfg.context.notes = case.manifest.notes.clone();
        cfg.validate()?;

        let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
        let arena = Arena::new(cfg, vcs, term::reporter(verbose))?;
        match arena.run().await {
            Ok(outcome) => cards.push(Scorecard::build(case, &outcome)),
            Err(e) => {
                eprintln!("  {} {e:#}", term::red("failed"));
                cards.push(Scorecard { case: case.manifest.name.clone(), ..Default::default() });
            }
        }
    }

    print_scorecards(&cards, json)
}

/// The scorecard table and per-case detail, shared by a live run and a replay.
fn print_scorecards(cards: &[locust_core::bench::Scorecard], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(cards)?);
        return Ok(());
    }

    println!("\n{}", term::bold("benchmark"));
    println!(
        "\n{:<18} {:>10} {:>10} {:>10} {:>8}",
        "case", "recall", "pushback", "conf", "cost"
    );
    println!("{}", "-".repeat(60));
    for c in cards {
        println!(
            "{:<18} {:>10} {:>9.0}% {:>10} {:>8}",
            c.case,
            format!("{}/{} ({:.0}%)", c.real_found.len(), c.real_planted, c.recall() * 100.0),
            c.defence_pushback() * 100.0,
            c.findings_confirmed,
            if c.cost_usd > 0.0 { format!("${:.2}", c.cost_usd) } else { "—".into() },
        );
    }

    for c in cards {
        println!("\n{} {}", term::bold(&c.case), term::dim("detail"));
        if !c.real_missed.is_empty() {
            println!("  {} {}", term::red("missed"), c.real_missed.join(", "));
        }
        for line in &c.straddled {
            println!("  {} {}", term::yellow("straddled"), term::dim(line));
        }
        println!(
            "  {:<16} refute {}, narrow {}, concede {} of {}",
            term::dim("defence"),
            c.refuted_stance,
            c.narrowed_stance,
            c.conceded_stance,
            c.rebuttals
        );
        for u in &c.unclassified {
            println!("  {} {u}", term::yellow("triage"));
        }
    }

    println!(
        "\n{}",
        term::dim(
            "recall: planted defects confirmed. pushback: rebuttals that refuted or narrowed — \
             on its own it measures assertiveness, not accuracy; `cargo test -p locust-core \
             --test defence_probe -- --ignored` is what measures whether the defence can tell a \
             true claim from a false one. straddled: one finding described two planted defects, \
             so only one could be credited. triage: confirmed findings matching nothing planted \
             — often real, and worth adding to the case."
        )
    );
    Ok(())
}

fn load_config(explicit: &Option<PathBuf>, repo: &std::path::Path) -> Result<Config> {
    let path = match explicit {
        Some(p) => p.clone(),
        None => Config::discover(repo).with_context(|| {
            format!(
                "no {CONFIG_FILENAME} found in {} or any parent — run `locust init`",
                repo.display()
            )
        })?,
    };
    Config::load(&path)
}

fn parse_severity(s: &str) -> Result<locust_core::model::Severity> {
    use locust_core::model::Severity::*;
    Ok(match s.to_ascii_lowercase().as_str() {
        "low" => Low,
        "medium" => Medium,
        "high" => High,
        "critical" => Critical,
        other => bail!("unknown severity `{other}` (low, medium, high, critical)"),
    })
}

/// What the run will do and roughly what it will cost, without calling anything.
fn plan(cfg: &Config, vcs: &dyn locust_vcs::Vcs) -> Result<()> {
    let range = vcs.range(&cfg.run.base, &cfg.run.target)?;
    let pack = locust_core::context::ContextPack::build(vcs, &range, &cfg.context)?;

    println!("{}", term::bold("plan"));
    println!("  repo        {} ({})", vcs.root().display(), vcs.kind());
    println!("  range       {}...{}", &range.merge_base[..8], &range.target[..8]);
    println!("  files       {} changed", pack.files.len());
    println!(
        "  context     {} bytes of diff, {} files inlined",
        pack.diff.len(),
        pack.sources.len()
    );
    println!();

    let provider_names: Vec<&str> =
        cfg.active_providers().iter().map(|p| p.name.as_str()).collect();
    println!("  providers   {}", provider_names.join(", "));
    let panel = cfg.panel()?;
    println!(
        "  judges      {} ({})",
        panel.iter().map(|j| format!("{} {}", j.name, j.traits)).collect::<Vec<_>>().join(", "),
        cfg.arena.judge.as_deref().unwrap_or("rotating, excluding authors")
    );
    println!("  generations {}", cfg.run.generations);
    println!("  population  {} attackers, {} defenders", cfg.arena.attackers, cfg.arena.defenders);
    println!("  evolution   {}", locust_core::evolution::describe(&cfg.evolution));
    println!();

    // Defender and judge call counts depend on how many findings appear, which
    // is the one thing we cannot know in advance. Assume two per attacker.
    let g = cfg.run.generations as usize;
    let attacks = cfg.arena.attackers * g;
    let assumed_findings = cfg.arena.attackers * 2;
    let batches = assumed_findings.div_ceil(cfg.arena.judge_batch.max(1)).max(1);
    let defends = cfg.arena.defenders * batches * g;
    // Every judge on the panel rules on every batch.
    let judges = batches * g * panel.len().max(1);
    let breeding = if cfg.evolution.enabled {
        (cfg.evolution.mutants + cfg.evolution.crossovers) * g.saturating_sub(1)
    } else {
        0
    };
    let synth = usize::from(cfg.synthesis.enabled);

    println!("{}", term::bold("estimated model calls"));
    println!("  attack      {attacks}");
    println!("  defend      ~{defends}");
    println!("  judge       ~{judges}{}", if panel.len() > 1 {
        format!("  ({} judges × ~{} batches × {g} generations)", panel.len(), batches)
    } else {
        String::new()
    });
    if breeding > 0 {
        println!("  breeding    {breeding}");
    }
    if synth > 0 {
        println!("  synthesis   {synth}");
    }
    println!("  {}      ~{}", term::bold("total"), attacks + defends + judges + breeding + synth);
    println!(
        "\n{}",
        term::dim("defend/judge counts assume ~2 findings per reviewer; actuals vary.")
    );
    Ok(())
}

fn providers(cfg: &Config) -> Result<()> {
    println!("{}", term::bold("providers"));
    for pc in &cfg.providers {
        let status = match pc.build() {
            Ok(p) => format!("{} {}", term::green("ready"), term::dim(p.model())),
            Err(e) => format!("{} {}", term::red("unavailable"), term::dim(&format!("{e}"))),
        };
        // A dot marks a provider that is configured but excluded from this run.
        let active = if cfg.active_providers().iter().any(|a| a.name == pc.name) {
            "  ".to_string()
        } else {
            term::dim("· ")
        };
        println!("{active}{:<16} {:<5} {status}", pc.name, format!("{:?}", pc.kind).to_lowercase());
    }

    // A CLI provider that is configured but not installed is the most common
    // way a run ends up with a silently smaller population.
    for pc in cfg.providers.iter().filter(|p| p.kind == locust_core::config::ProviderKind::Cli) {
        if let Some(cmd) = pc.command.as_ref().and_then(|c| c.first()) {
            if locust_provider::cli::which(cmd).is_none() {
                println!("  {} `{cmd}` is not on PATH", term::yellow("warn"));
            }
        }
    }
    Ok(())
}

fn judges(cfg: &Config, full: bool) -> Result<()> {
    use locust_core::traits::{Axis, Voice};

    let panel = cfg.panel()?;
    let source = if cfg.judges.is_empty() { "built-in panel" } else { "from locust.toml" };
    println!("{} {}", term::bold("judges"), term::dim(source));

    if panel.len() == 1 {
        println!(
            "  {}",
            term::dim("one judge: verdicts are final and there is no disagreement to measure")
        );
    }

    for j in &panel {
        let provider = j.provider.as_deref().unwrap_or("rotating");
        println!(
            "\n  {:<14} {:<22} {}",
            term::bold(&j.name),
            term::cyan(&j.traits.to_string()),
            term::dim(&format!("{provider}, weight {:.1}", j.weight))
        );
        for axis in Axis::ALL {
            let v = j.traits.get(axis);
            println!("      {:<18} {} {:.2}", axis.name(), bar(v), v);
        }
        if full {
            let text = j.traits.render(Voice::Judge);
            if text.trim().is_empty() {
                println!("\n      {}", term::dim("neutral — no temperament text is added"));
            } else {
                println!();
                for line in text.lines() {
                    println!("      {line}");
                }
            }
        }
    }

    println!(
        "\n{}",
        term::dim(&format!("presets: {}", locust_core::Traits::PRESETS.join(", ")))
    );
    Ok(())
}

/// Ten-cell meter. The middle two cells are the neutral band, where a trait
/// contributes no prompt text at all.
fn bar(v: f64) -> String {
    let filled = (v * 10.0).round() as usize;
    let mut out = String::new();
    for i in 0..10 {
        out.push(if i < filled { '#' } else { '.' });
    }
    out
}

fn personas(cfg: &Config, full: bool) -> Result<()> {
    for role in [Role::Attacker, Role::Defender] {
        let list = cfg.seed_personas(role)?;
        let source = if cfg.personas.iter().any(|p| p.role == role) {
            "from locust.toml"
        } else {
            "built-in library"
        };
        println!("\n{} {}", term::bold(&format!("{role:?}s").to_lowercase()), term::dim(source));
        for p in list {
            let temperament = if p.traits.is_neutral() {
                String::new()
            } else {
                format!("  {}", term::cyan(&p.traits.to_string()))
            };
            println!("  {:<22} {}{temperament}", term::bold(&p.name), term::dim(&p.focus));
            if full {
                for line in p.prompt.lines() {
                    println!("      {line}");
                }
                println!();
            }
        }
    }
    Ok(())
}

fn report(repo: &std::path::Path, run: Option<&str>, render: bool) -> Result<()> {
    let dir = match run {
        Some(id) => repo.join(".locust").join("runs").join(id),
        None => store::latest_run(repo).context("no runs found under .locust/runs")?,
    };

    if render {
        let json = dir.join("report.json");
        let text = std::fs::read_to_string(&json)
            .with_context(|| format!("no run data at {}", json.display()))?;
        let outcome: locust_core::RunOutcome = serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", json.display()))?;
        // The run's own config, so the rebuilt report matches how it was run.
        let cfg = Config::load(&dir.join("config.toml")).unwrap_or_default();
        println!("{}", locust_core::report::markdown(&outcome, &cfg));
        return Ok(());
    }

    let md = dir.join("report.md");
    let text = std::fs::read_to_string(&md)
        .with_context(|| format!("no report at {}", md.display()))?;
    println!("{text}");
    Ok(())
}
