//! `locust init` — write a config that works on this machine.
//!
//! Detection matters more than it looks: a config listing four providers when
//! only one has credentials produces a run with a quarter of the population and
//! no obvious sign that anything is wrong.

use crate::term;
use anyhow::{bail, Result};
use locust_core::config::CONFIG_FILENAME;
use std::fmt::Write as _;
use std::path::Path;

struct Detected {
    api: Vec<(
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        f64,
        f64,
    )>,
    clis: Vec<&'static str>,
}

/// (name, wire, key env var, default model, $/Mtok in, $/Mtok out)
const API_CANDIDATES: &[(&str, &str, &str, &str, f64, f64)] = &[
    (
        "anthropic",
        "anthropic",
        "ANTHROPIC_API_KEY",
        "claude-opus-5",
        5.0,
        25.0,
    ),
    ("openai", "open_ai", "OPENAI_API_KEY", "gpt-5", 1.25, 10.0),
    (
        "gemini",
        "gemini",
        "GEMINI_API_KEY",
        "gemini-2.5-pro",
        1.25,
        10.0,
    ),
];

const CLI_CANDIDATES: &[&str] = &["claude", "codex", "gemini"];

fn detect() -> Detected {
    Detected {
        api: API_CANDIDATES
            .iter()
            .filter(|(_, _, env, ..)| std::env::var_os(env).is_some())
            .copied()
            .collect(),
        clis: CLI_CANDIDATES
            .iter()
            .filter(|c| locust_provider::cli::which(c).is_some())
            .copied()
            .collect(),
    }
}

pub fn write_config(repo: &Path, force: bool) -> Result<()> {
    let path = repo.join(CONFIG_FILENAME);
    if path.exists() && !force {
        bail!(
            "{} already exists (pass --force to overwrite)",
            path.display()
        );
    }

    let found = detect();
    if found.api.is_empty() && found.clis.is_empty() {
        println!(
            "{} no providers detected — set ANTHROPIC_API_KEY (or OPENAI_API_KEY / \
             GEMINI_API_KEY), or install an agentic CLI such as `claude`.\n\
             Writing a config with all of them commented out.",
            term::yellow("warn")
        );
    }

    let text = render(&found);
    std::fs::write(&path, &text)?;

    println!("{} {}", term::green("wrote"), path.display());
    if !found.api.is_empty() {
        println!(
            "  {} {}",
            term::dim("api"),
            found
                .api
                .iter()
                .map(|(n, ..)| *n)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !found.clis.is_empty() {
        println!("  {} {}", term::dim("cli"), found.clis.join(", "));
    }

    let distinct = found.api.len() + found.clis.len();
    if distinct < 2 {
        println!(
            "\n{} only one provider is available, so every agent and the judge will be the \
             same model. The arena still works — reviewers compete and findings are still \
             defended and judged — but a model judging its own output grades it generously, \
             and you lose the disagreement between vendors that makes corroboration mean \
             anything. Add a second provider when you can.",
            term::yellow("note")
        );
    }

    println!("\nnext: {}", term::bold("locust review --dry-run"));
    Ok(())
}

fn render(found: &Detected) -> String {
    let mut s = String::new();

    s.push_str(
        "# locust — adversarial, evolutionary code review.\n\
         #\n\
         # Reviewers compete to find real defects in a change. A defender tries to destroy\n\
         # everything they raise; an impartial judge rules on what survives. Reviewers are\n\
         # scored on findings that live through that, so noise costs them. Between\n\
         # generations the winning prompts breed and the rest are culled.\n\
         #\n\
         #   locust review --dry-run     what it will do, and what it will cost\n\
         #   locust review               run it\n\
         #   locust providers            check credentials and CLIs\n\
         #   locust personas --full      read the reviewer prompts\n\n",
    );

    let _ = writeln!(
        s,
        "[run]\n\
         base = \"main\"\n\
         target = \"HEAD\"\n\n\
         # Each generation raises the weight on structural findings and bans everything\n\
         # already settled, so later rounds are forced to look higher. Two is the minimum\n\
         # for that to mean anything; three is the useful default.\n\
         generations = 3\n\n\
         # Agents in flight at once. Raise it if your rate limits allow.\n\
         parallelism = 6\n\n\
         # auto | git | jj — auto prefers jj when the repo is a jj workspace.\n\
         vcs = \"auto\"\n\n\
         # auto gives a private checkout to any provider that can use one (the CLI\n\
         # providers below), so those agents can grep for call sites and run tests to prove\n\
         # a finding rather than asserting it. always | never also available.\n\
         workspaces = \"auto\"\n\n\
         # Stop after the current generation once spend passes this, and report what the\n         # arena settled so far. Needs per-token costs set on the providers below.\n         # 0 disables the check.\n\
         budget_usd = 0.0\n"
    );

    let judge = found
        .api
        .first()
        .map(|(n, ..)| n.to_string())
        .or_else(|| found.clis.first().map(|c| format!("{c}-cli")));

    let _ = writeln!(
        s,
        "\n[arena]\n\
         # Reviewers per generation, spread across the providers below.\n\
         attackers = 6\n\n\
         # Defenders answering them. More defenders means findings must survive more\n\
         # angles of attack before they reach you.\n\
         defenders = 3\n\n\
         # Findings per defender and judge call. Smaller batches reason better and cost more.\n\
         judge_batch = 8\n\n\
         # Who the judges *are* is the [[judge]] panel further down; this setting is only\n\
         # about which provider carries their votes. Left unset, each judge rotates across\n\
         # providers that authored none of the findings in front of it -- models prefer\n\
         # their own output, and a judge ruling on its own agents' findings turns the\n\
         # tournament into a popularity contest between vendors. Setting it pins the whole\n\
         # panel to one provider, which keeps temperament diversity but loses vendor\n\
         # diversity.\n\
         judge_excludes_author = true\n\
         {}judge = \"{}\"\n",
        if found.api.len() + found.clis.len() > 1 {
            "# "
        } else {
            ""
        },
        judge.as_deref().unwrap_or("anthropic")
    );

    s.push_str(
        "\n[context]\n\
         # Hand reviewers the whole post-change file, not just the hunks. Reviewing a\n\
         # unified diff alone is the main cause of confident, wrong findings: the reviewer\n\
         # cannot see the guard twelve lines above the hunk.\n\
         include_full_files = true\n\
         max_file_bytes = 60000\n\
         max_total_bytes = 400000\n\
         exclude = [\"Cargo.lock\", \"package-lock.json\", \"/node_modules/\", \"/vendor/\", \"/target/\", \"*.min.js\"]\n\n\
         # Pasted into every agent's prompt. House conventions, patterns that are\n\
         # deliberate here, your actual threat model — this is the cheapest way to stop\n\
         # the arena relitigating decisions your team already made.\n\
         notes = \"\"\"\n\
         \"\"\"\n",
    );

    s.push_str(
        "\n[scoring]\n\
         # A reviewer's fitness. Points are multiplied by the judge's severity rating and\n\
         # by an altitude weight that ramps each generation, so a line-level finding worth\n\
         # 1.0x in the first round is worth 0.15x in the last.\n\
         confirmed = 3.0\n\n\
         # Negative on purpose. Without a cost for noise the best strategy is to spray\n\
         # low-confidence guesses, which is the behaviour this tool exists to remove.\n\
         refuted = -1.5\n\
         undecided = 0.25\n\n\
         # Credit for independently rediscovering something already confirmed. A duplicate\n         # inherits its cluster's verdict, so echoing a *refuted* claim costs the full\n         # `refuted` penalty — otherwise agreeing with noise is free.\n\
         duplicate = 0.3\n\n\
         # Per extra provider that raised the same finding independently.\n\
         corroboration_bonus = 0.5\n\n\
         # Applied when the judge rules a finding a restatement of a settled one.\n\
         stale_penalty = -1.0\n\
         severity_weighted = true\n\n\
         # How much a split panel discounts a finding. 0.0 ignores dissent; 1.0 scores a\n\
         # finding purely by the share of panel weight behind it. Does nothing with one judge.\n\
         consensus_weight = 0.5\n",
    );

    s.push_str(
        "\n[evolution]\n\
         # Selection acts on prompts. Each generation the population is rebuilt from\n\
         # elites carried over unchanged, mutants rewritten against their own scorecard,\n\
         # crossovers blending two winners, and immigrants from the built-in library so\n\
         # the population does not collapse onto one point of view.\n\
         enabled = true\n\
         elite = 2\n\
         mutants = 2\n\
         crossovers = 1\n\
         immigrants = 1\n\n\
         # Personas scoring at or below this are culled regardless of rank.\n\
         cull_below = 0.0\n",
    );

    s.push_str(
        "\n[synthesis]\n\
         # The last step, and the one that produces the output you actually read: cluster\n\
         # the surviving findings by root cause so three confirmed findings about forgotten\n\
         # cleanup are reported as one issue about where this code puts its cleanup.\n\
         enabled = true\n\
         max_themes = 8\n",
    );

    s.push_str(
        "\n# --- sandbox -----------------------------------------------------------\n\
         #\n\
         # locust implements no sandboxing of its own, deliberately. Containers, microVMs,\n\
         # cgroups and jails are a different layer of concern with mature tools already\n\
         # solving them, and a half-built sandbox is worse than an obvious absence of one\n\
         # because it invites misplaced trust.\n\
         #\n\
         # What locust does is compose. `command` is a prefix argv that the agent command\n\
         # is appended to, so the result is whatever you already run. {workspace} is the\n\
         # agent's private checkout and {id} its label.\n\
         #\n\
         # [sandbox]\n\
         # command = [\"docker\", \"run\", \"--rm\", \"--network=none\",\n\
         #            \"-v\", \"{workspace}:/w\", \"-w\", \"/w\", \"locust-agent\"]\n\
         #\n\
         # Equally:\n\
         #   systemd-run --scope -p MemoryMax=2G -p CPUQuota=200% --\n\
         #   nix develop /etc/nixos#agent -c\n\
         #   bwrap --ro-bind / / --bind {workspace} {workspace} --unshare-net --\n\
         #   firecracker-run --rootfs agent.ext4 --mount {workspace} --\n\
         #\n\
         # Only CLI providers spawn processes, so only they are affected. A provider can\n\
         # set `sandbox = false` to opt out if it is already isolated by other means, or\n\
         # needs something the sandbox denies.\n\
         #\n\
         # Prompt text is never interpolated into this argv -- only {workspace}, {id} and\n\
         # {model} are -- so a model cannot inject arguments into your container command.\n\
         #\n\
         # An agent still needs whatever credentials its CLI authenticates with, so a\n\
         # sandbox that blocks the network blocks the agent: allow the API host only.\n\
         #\n\
         # Killing the wrapper does not always kill what it started. A timeout signals the\n\
         # process group locust spawned -- for docker that is the client, not the container,\n\
         # which the daemon owns. Add `--rm --init --stop-timeout 10` so a runaway agent is\n\
         # bounded by something other than locust. systemd-run, bwrap and nix develop stay\n\
         # in the process group and die with it.\n\n",
    );

    s.push_str(
        "\n# --- judges ------------------------------------------------------------\n\
         #\n\
         # Judges have temperament, expressed as Big Five (OCEAN) traits in 0.0-1.0.\n\
         # `locust judges` prints the panel; `locust judges --full` prints the prompt text\n\
         # each temperament produces.\n\
         #\n\
         #   openness           structural argument vs. demand a reproduction\n\
         #   conscientiousness  audit every claim vs. judge on overall plausibility\n\
         #   extraversion       weigh the debate vs. ignore it and re-read the code\n\
         #   agreeableness      benefit of the doubt vs. guilty until proven\n\
         #   neuroticism        a possible failure is inevitable vs. dismiss hypotheticals\n\
         #\n\
         # 0.5 is neutral and adds no prompt text at all, so a trait you do not set costs\n\
         # you nothing. The band around 0.5 is deliberately wide: 0.6 still reads as\n\
         # neutral, so you have to mean it before a judge changes behaviour.\n\
         #\n\
         # The default is ONE judge with no temperament, and judging is a third of a\n\
         # run's calls. The three-temperament panel below is the alternative: a finding\n\
         # all three confirm cleared three different bars; one only `stickler` confirms\n\
         # is a nitpick; one only `pragmatist` refuted is worth a second look. That\n\
         # spread is the whole reason to run more than one judge, and it is reported\n\
         # under 'Where the panel split'.\n\
         #\n\
         # It is commented out because it has not been shown to pay for itself. On the\n\
         # `session-cache` benchmark slate the panel and a single judge returned the same\n\
         # verdict on 7 of 7 claims; the panel split once, on a claim that is a judgement\n\
         # call either way, and the split did not move the verdict. Uncomment all three if\n\
         # you want dissent reported -- it is the only evidence that a confirmation cleared\n\
         # more than one standard of proof -- and expect to pay three times for judging.\n\
         #\n\
         # Presets: neutral, stickler, pragmatist, architect, paranoid, skeptic, advocate.\n\n",
    );

    for (name, note) in [
        (
            "stickler",
            "audits every claim, wants the evidence quoted, assumes the reviewer is wrong",
        ),
        (
            "pragmatist",
            "asks whether anyone will actually be hurt; deflates hypotheticals",
        ),
        (
            "architect",
            "will confirm a structural claim on the strength of the argument alone",
        ),
    ] {
        let _ = writeln!(
            s,
            "# {note}\n# [[judge]]\n# name = \"{name}\"\n# preset = \"{name}\"\n"
        );
    }

    s.push_str(
        "# Override individual axes on top of a preset, or skip the preset entirely.\n\
         # `provider` pins a judge to one model; left unset, judges rotate across providers\n\
         # that authored none of the findings they are ruling on -- so a three-judge panel\n\
         # over three providers gets temperament diversity and vendor diversity at once.\n\
         #\n\
         # [[judge]]\n\
         # name = \"release gate\"\n\
         # provider = \"anthropic\"\n\
         # weight = 2.0            # counts double in the vote\n\
         # preset = \"skeptic\"\n\
         # neuroticism = 0.9       # ... but anxious about what it does confirm\n\n",
    );

    s.push_str("\n# --- providers ---------------------------------------------------------\n");

    if found.api.is_empty() && found.clis.is_empty() {
        s.push_str("#\n# Nothing was detected on this machine. Uncomment one and set its key.\n");
    }

    for (name, wire, env, model, cin, cout) in API_CANDIDATES {
        let on = found.api.iter().any(|(n, ..)| n == name);
        let c = if on { "" } else { "# " };
        if !on {
            let _ = writeln!(s, "\n# {env} is not set:");
        } else {
            s.push('\n');
        }
        let _ = writeln!(
            s,
            "{c}[[provider]]\n\
             {c}name = \"{name}\"\n\
             {c}kind = \"api\"\n\
             {c}wire = \"{wire}\"\n\
             {c}model = \"{model}\"\n\
             {c}api_key_env = \"{env}\"\n\
             {c}input_cost_per_mtok = {cin}\n\
             {c}output_cost_per_mtok = {cout}"
        );
    }

    s.push_str(
        "\n# Agentic CLIs run inside the agent's own private worktree, so they can open files\n\
         # the context pack omitted and grep for every call site.\n\
         #\n\
         # They are given read-only tools by default. To let them run the test suite — which\n\
         # makes a finding much harder to refute, because it was demonstrated rather than\n\
         # argued — grant execution explicitly by spelling out the command:\n\
         #\n\
         #   command = [\"claude\", \"-p\", \"--output-format\", \"json\",\n\
         #              \"--allowed-tools\", \"Read,Grep,Glob,Bash\",\n\
         #              \"--append-system-prompt\", \"{system}\"]\n\
         #\n\
         # The worktree is disposable and discarded after the agent finishes, but the agent\n\
         # runs with your credentials and network access. Grant it deliberately.\n",
    );

    for cli in CLI_CANDIDATES {
        let on = found.clis.contains(cli);
        let c = if on { "" } else { "# " };
        if !on {
            let _ = writeln!(s, "\n# `{cli}` is not on PATH:");
        } else {
            s.push('\n');
        }
        let _ = writeln!(
            s,
            "{c}[[provider]]\n\
             {c}name = \"{cli}-cli\"\n\
             {c}kind = \"cli\"\n\
             {c}preset = \"{cli}\"\n\
             {c}timeout_secs = 900"
        );
    }

    s.push_str(
        "\n# A provider the presets do not cover — any OpenAI-compatible endpoint, or a\n\
         # CLI with its own argv. {system}, {prompt} and {model} are substituted.\n\
         #\n\
         # [[provider]]\n\
         # name = \"openrouter\"\n\
         # kind = \"api\"\n\
         # wire = \"open_ai\"\n\
         # base_url = \"https://openrouter.ai/api/v1\"\n\
         # model = \"deepseek/deepseek-r1\"\n\
         # api_key_env = \"OPENROUTER_API_KEY\"\n\
         #\n\
         # [[provider]]\n\
         # name = \"custom-cli\"\n\
         # kind = \"cli\"\n\
         # command = [\"my-agent\", \"--system\", \"{system}\"]\n\
         # stdin = \"{prompt}\"\n\
         # extract = { json_path = \"result\" }   # or \"raw\", or { json_lines = \"msg/text\" }\n",
    );

    s.push_str(
        "\n# --- personas ----------------------------------------------------------\n\
         #\n\
         # Omit these entirely and locust uses its built-in library (see `locust personas`).\n\
         # Declaring even one of a role replaces the library for that role, so these are\n\
         # where your team's review standards live, in version control next to the code\n\
         # they apply to. A persona is a point of view, not a checklist.\n\
         #\n\
         # Reviewers take the same OCEAN traits as judges, read in a reviewer's voice:\n\
         # agreeableness is how readily it gives the author the benefit of the doubt,\n\
         # neuroticism is how much a rare failure path worries it. The built-in library is\n\
         # neutral on purpose -- those prompts were written to stand alone, and a trait\n\
         # paragraph bolted on would compete with them rather than sharpen them.\n\
         #\n\
         # [[persona]]\n\
         # role = \"attacker\"\n\
         # name = \"migration safety\"\n\
         # focus = \"changes that are unsafe while both versions are deployed\"\n\
         # neuroticism = 0.85        # a possible window is an inevitable one\n\
         # agreeableness = 0.2       # assumes nothing was thought through\n\
         # prompt = \"\"\"\n\
         # You review every change as though it will be half-deployed for twenty minutes.\n\
         # Old and new code run at once against one database.\n\
         #\n\
         # Can a writer on the new code produce a row the old code misreads? Does a column\n\
         # get read before the backfill finishes? Is a queue message written in a shape the\n\
         # currently-running consumers cannot parse? Does a feature flag default leave the\n\
         # system in a state neither version expects?\n\
         #\n\
         # Name the window and the interleaving. 'Could break during deploy' is not a\n\
         # finding; 'between the migration and the backfill, a read of X returns NULL and\n\
         # the old decoder panics' is.\n\
         # \"\"\"\n",
    );

    s
}
