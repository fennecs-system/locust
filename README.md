# locust

Adversarial, evolutionary code review across multiple model providers.

Ask one model to review a diff and you get a list of plausible-sounding
line-level remarks. Nothing in that loop separates a remark that is *true* from
one that merely reads well, so a human has to check every one — which is the
work you were trying to remove.

locust changes the shape of the task. Reviewers compete. A defender tries to
destroy everything they raise. An impartial judge, drawn from a different
vendor, rules on what survives. Reviewers are scored on surviving findings, so
noise costs them. Between rounds the winning prompts breed and the rest are
culled, with the weight on structural findings ramping up each round.

What reaches you is the last step: the findings that survived an argument,
clustered into root-cause themes.

```
locust init          # write a config for the providers available here
locust review        # run the arena
```

## How a round works

```
  attack    6 reviewers × N providers, each in a private worktree
              ↓  61 raw findings
  cluster   collapse near-duplicates, count independent rediscoveries
              ↓  23 distinct claims
  defend    3 defenders refute, narrow or concede each one
              ↓
  judge     a judge rules on each; a provider that authored none of them
            carries the vote. A panel of temperaments is opt-in
              ↓  9 confirmed
  score     persona fitness = what survived, weighted by severity × altitude
              ↓
  breed     elites carried over, winners rewritten, losers culled
              ↓  next generation, higher altitude pressure
```

After the last round, a synthesis pass clusters the survivors by root cause.

## Why higher-level issues surface

This is the part that does not happen by itself. Three mechanisms force it:

**Altitude is scored, and the weight ramps.** The judge tags every finding with
how far from a single line the problem lives — `line`, `local`, `module`,
`architectural` — and that tag carries a weight that changes across the run. A
line-level finding is worth `1.00×` in the first generation and `0.15×` in the
last; an architectural one goes from `1.80×` to `4.00×`. Early rounds pay for
anything true. Later rounds barely pay for nitpicks.

**Settled ground is banned.** Every finding an earlier round confirmed *or*
refuted is handed to the next round's reviewers as a do-not-raise list, and the
judge marks restatements as stale. Reviewers cannot farm the same easy bug; to
score at all they have to look somewhere new.

**Noise costs more than silence.** A refuted finding scores `-1.5`. A reviewer
that raises nothing scores `0.0`. Without that asymmetry the optimal strategy is
to spray low-confidence guesses, and the population converges on exactly the
behaviour this tool exists to eliminate.

Taken together, a persona that only produces style remarks has its findings
refuted by defenders, scores negative, and is culled — while the prompts that
found something real get rewritten against their own scorecard and bred forward.

## Judges have temperament

One judge's verdict is unfalsifiable. You cannot tell a finding that is
genuinely solid from one that happened to catch a lenient judge, because the
verdict looks identical either way. The answer to that is a panel that
disagrees on purpose — and the honest statement of where this project stands is
that the panel has not yet been shown to earn its cost.

**The default is one judge, with no temperament.** Judging is a third of a
run's model calls, and on the one measurement that exists a panel of three
returned the same verdict as a single judge on every claim. That measurement is
in `tests/defence_probe.rs` and is described under [Ground truth](#ground-truth);
it is one fixture and one run, so treat it as a reason to make the panel opt-in
rather than as a settled result.

A judge carries a Big Five (OCEAN) temperament in `0.0..=1.0`:

| trait | low | high |
|---|---|---|
| **openness** | demands a reproduction; a design critique with no named failure is an opinion | confirms a structural claim on the argument alone; promotes altitude |
| **conscientiousness** | judges on overall plausibility | audits every claim against the quoted code, line by line |
| **extraversion** | ignores the rhetoric and re-reads the source | treats the debate as the evidence; a case argued well and answered weakly has been won |
| **agreeableness** | guilty until proven; an uncarried claim is refuted, not undecided | benefit of the doubt; a false positive costs five minutes, a false negative ships the bug |
| **neuroticism** | dismisses hypotheticals; rates severity by expected impact | a possible failure path is an inevitable one; rates severity for the bad case |

Declare `[[judge]]` entries and you get a panel back, where the **spread is the
output**:

```
f0-1-2  connection leak on the error path
  stickler    CONFIRMED  high      ← even the auditor couldn't fault it
  pragmatist  CONFIRMED  medium
  architect   CONFIRMED  high
  → confirmed, unanimous

f0-0-4  magic number should be a constant
  stickler    CONFIRMED  low       ← only the pedant cared
  pragmatist  REFUTED
  architect   REFUTED
  → refuted, 1/3 dissent
```

A finding all three confirm cleared three genuinely different bars. One only
`stickler` confirms is a nitpick. One only `pragmatist` refuted is worth a
second look. The report has a **Where the panel split** section for exactly
this, and `[scoring] consensus_weight` makes a contested confirmation score
less than a unanimous one. With one judge there is never any dissent, and all
of that does nothing.

`locust judges` prints the panel; `--full` prints the prompt text each
temperament produces.

**0.5 is neutral and emits nothing.** The band around it is deliberately wide —
0.6 still reads as neutral — so you have to mean it before a judge changes
behaviour, and a persona that sets no traits reads exactly as it did before
traits existed. Presets: `neutral`, `stickler`, `pragmatist`, `architect`,
`paranoid`, `skeptic`, `advocate`.

```toml
[[judge]]
name = "release gate"
preset = "skeptic"
neuroticism = 0.9      # ... but anxious about what it does confirm
provider = "anthropic" # unset = rotate across non-authoring providers
weight = 2.0           # counts double in the vote
```

Reviewers take the same traits, read in a reviewer's voice — agreeableness
becomes how readily it gives the author the benefit of the doubt, neuroticism
how much a rare failure path worries it. The built-in reviewer library stays
neutral on purpose: those prompts were written to stand alone, and a trait
paragraph bolted on would compete with them rather than sharpen them.

Evolution rewrites prose, not traits. A mutant inherits its parent's
temperament and a crossover takes the midpoint of its two parents', so a
reviewer you calibrated by hand keeps that calibration across generations
instead of having it drift under selection.

## Why multiple providers

Two reasons, and only the second is about quality of ideas.

**Judging.** Models prefer their own output. A model judging findings its own
agents raised grades them generously, and the whole tournament quietly becomes a
popularity contest. By default each judge's vote is carried by a provider that
authored none of the findings in front of it (`judge_excludes_author`), with the
rotation offset per judge — so if you do declare a three-judge panel, three
providers give it temperament diversity and vendor diversity from the same
configuration. With the default single judge, this is what keeps that judge off
its own vendor's findings.

**Corroboration.** When two vendors independently raise the same finding, that
is weak evidence it is real — they have different training data and different
blind spots. locust clusters duplicates, counts how many *distinct* providers
reached each one, and surfaces that to the judge and in the report. It is
evidence, not proof; several models share the same false alarms, and the judge
is told so explicitly.

Clustering happens twice, because one pass is not enough. A cheap lexical pass
collapses obvious repeats before judging, to keep judge batches small. But two
reviewers describing one defect from opposite ends share almost no vocabulary —
*"expiry deletes from `_entries` but not `_hits`"* and *"`_evict` can pick a key
absent from `_entries`"* are one bug and overlap on almost nothing. The judge is
reading both cases side by side anyway, so it is asked to merge them, which
costs no extra calls.

locust runs with a single provider and the arena still functions. You just lose
both of the above, and `locust init` says so.

## Providers

Two adapters, configured side by side:

| | reaches the model by | agent can | use when |
|---|---|---|---|
| `kind = "api"` | locust calls the HTTP API | read the context pack only | you want uniform behaviour, exact cost accounting, and speed |
| `kind = "cli"` | spawns `claude -p`, `codex exec`, … in the agent's worktree | open any file, grep call sites, and — if you grant it — **run the tests** | you want findings proved rather than asserted |

The `api` wire formats are `anthropic`, `open_ai` and `gemini`; `base_url`
makes `open_ai` cover OpenRouter, Groq, DeepSeek, vLLM and the rest. The `cli`
kind has presets for `claude`, `codex` and `gemini`, or you can give it argv
with `{system}` / `{prompt}` / `{model}` placeholders.

CLI agents get **read-only tools by default** — they can read and grep inside
their disposable worktree, but not execute. Granting execution lets an agent
demonstrate a failure instead of arguing for it, which is the single biggest
jump in finding quality this tool offers, and `locust init` writes the exact
line to uncomment. It is opt-in because the agent runs with your credentials
and network access, disposable worktree or not.

## Sandboxing

locust implements none, deliberately. Containers, microVMs, cgroups and jails
are a different layer of concern with mature tools already solving them, and a
half-built sandbox is worse than an obvious absence of one because it invites
misplaced trust.

What locust does is compose. `[sandbox] command` is a prefix argv that the agent
command is appended to:

```toml
[sandbox]
command = ["docker", "run", "--rm", "--network=none",
           "-v", "{workspace}:/w", "-w", "/w", "locust-agent"]
```

`{workspace}` is the agent's working directory and `{id}` its label. With a
private checkout, it points to that checkout at the target revision. When
workspaces are disabled or checkout creation fails, it points to the live
repository root. A read-write mount then exposes your working tree; use
`{workspace}:/w:ro` for read-only review. The same shape covers
`systemd-run --scope -p MemoryMax=2G --`, `nix develop ... -c`,
`bwrap --unshare-net --`, a Firecracker wrapper script, or anything else that
takes a command as its tail. Only CLI providers spawn processes, so only they
are affected; a provider can opt out with `sandbox = false`.

A caveat the arena found reviewing itself: **killing the wrapper does not
always kill what it started.** A timeout signals the process group locust
spawned, which is the `docker` client — not the container, which the daemon
owns. Use `--rm` plus a container-side limit so a runaway agent is bounded by
something other than locust:

```toml
command = ["docker", "run", "--rm", "--init", "--stop-timeout", "10",
           "--network=none", "-v", "{workspace}:/w", "-w", "/w", "locust-agent"]
```

`systemd-run --scope`, `bwrap` and `nix develop -c` do not have this problem:
they stay in the process group and die with it.

Two more things worth knowing. Prompt text is **never** interpolated into the
wrapper argv — only `{workspace}`, `{id}` and `{model}` are, so a model cannot
inject arguments into your container invocation. And an agent still needs the
credentials its CLI authenticates with, so a sandbox that blocks the network
blocks the agent: allow the API host and nothing else.

## Isolation

With `workspaces = "auto"` (the default), each filesystem-capable agent gets a
checkout at the target revision under `.locust/wt/`, destroyed when it finishes.
`always` requests a checkout for every provider; `never` uses the live repository
root. Checkout creation failures also fall back to that root, whose checked-out
revision may differ from the configured target. `git worktree` is the default
and needs nothing installed. A `jj` backend sits behind the same trait and is
picked automatically in a jj workspace — anonymous heads make a generation of
speculative workspaces free to create and discard, and the operation log lets
you rewind one.

## Personas

A reviewer is nothing but a system prompt. That prompt is its genome, and it is
what evolves.

Eight attackers and three defenders ship built in — `locust personas --full` to
read them. Declaring even one `[[persona]]` of a role in `locust.toml` replaces
the built-in library for that role, which is the point: your team's review
standards live in version control, next to the code they apply to.

**Only attackers are under selection.** Defenders are carried forward between
generations unchanged — they are not scored, bred or culled, and no defender
fitness exists. What motivates them is the prompt: a defender is told it is
scored on refutations the judge agrees with, and that conceding a true finding
is free while bluffing on one costs it credibility on every other rebuttal in
the round. That framing does work — see [Ground truth](#ground-truth) — but it
is framing, not a feedback loop, so defender quality cannot improve across a
run the way reviewer quality can.

Defenders also render traits in the reviewer's voice. The `skeptic` preset
therefore includes “Be hostile to this code,” which is not the same thing as
being skeptical of *findings*. For that, use neutral traits and an explicit
defender prompt asking for evidence against each claim while conceding true
defects, then compare answers to the same fixed true and false claims —
pushback alone does not measure accuracy.

```toml
[[persona]]
role = "attacker"
name = "migration safety"
focus = "changes that are unsafe while both versions are deployed"
prompt = """
You review every change as though it will be half-deployed for twenty minutes.
Old and new code run at once against one database.

Can a writer on the new code produce a row the old code misreads? Does a column
get read before the backfill finishes?

Name the window and the interleaving. 'Could break during deploy' is not a
finding; 'between the migration and the backfill, a read of X returns NULL and
the old decoder panics' is.
"""
```

The `[context] notes` field is the other half of that: house conventions and
patterns that are deliberate here, pasted into every agent's prompt. It is the
cheapest way to stop the arena relitigating decisions your team already made.

## Ground truth

Everything above is a claim about behaviour, and a review tool is the hardest
kind of thing to check: a run that confirms everything might mean the reviewers
are excellent, or that the defence is unmotivated and the judge rubber-stamps.
Those look identical from the outside.

So `cases/` holds changes with known answers, and `locust bench` runs the arena
against them.

```
locust bench --list                 the planted defects, per case
locust bench --generations 1        run the arena against every case
locust bench --case retry-budget    just one
locust bench --replay RUN_DIR       re-score stored runs, spending nothing
```

`--replay` exists because a manifest is itself code that can be wrong, and
twice now it has been. Re-running the arena to check a keyword fix costs money
*and* produces different findings, so it cannot answer the question; replaying
the stored findings through the current classifier can.

A case is a `base/` tree, a `head/` tree and a manifest naming the defects a
working arena should confirm. Missing one is a false negative, and recall is
the headline number.

A finding matching nothing planted is reported as **unclassified** rather than
counted against the run — an unanticipated finding is often a real defect the case
author missed, and scoring it as a false positive would train the benchmark to
reward blandness. One of `session-cache`'s six real defects got into the
manifest exactly that way.

| case | language | shape |
|---|---|---|
| `session-cache` | Python | concurrency and data-structure invariants in one file |
| `retry-budget` | Rust | retry semantics, a shared budget, an atomic that wraps |
| `permission-check` | Python | a contract change whose defects are at the *callers*, across three files |

Each case's planted defects are demonstrated rather than asserted — the two
Python cases ship `test_ground_truth.py`, and `retry-budget` ships a crate that
exercises `head/` directly. Those tests pin *buggy* behaviour on purpose: the
fixtures keep their bugs. They also pin the parts that are deliberately
*correct* but look wrong, because those are what the defence probe makes false
claims about, and an edit that quietly turned one into a real defect would turn
the probe into a tautology.

```
python3 -B -m unittest discover -s cases/session-cache
python3 -B -m unittest discover -s cases/permission-check
cargo test --manifest-path cases/retry-budget/ground_truth/Cargo.toml
```

### What a run looks like

Three cases, one generation, three reviewers, two providers, one judge:

```
case                   recall   pushback       conf     cost
------------------------------------------------------------
permission-check    3/4 (75%)         0%          3    $0.95
retry-budget        4/5 (80%)         8%          4    $1.03
session-cache       2/6 (33%)        38%          3    $1.17
```

Read honestly, that is one good result and two warnings. The arena found every
retry defect on `retry-budget` except one, and three of four on
`permission-check` — including the cross-file one, where the bug is at a caller
the diff does not contain. `session-cache` at 33% is the same fixture that
scored 50% with more reviewers, which is mostly a statement about how little
one run with three reviewers tells you.

`pushback` is the weakest number here. It counts rebuttals that refuted or
narrowed, which measures assertiveness rather than accuracy: a defender that
refutes everything scores well on it and is worthless. Nothing in a live run
tells you whether the defence can tell a true claim from a false one, because
every claim in a live run is one a reviewer believed. That question needs a
claim that is known to be false, which is what the probe below is for.

### What has actually been measured

Honest status, because a benchmark nobody has run says nothing:

**The defence pushes back on false claims.** `tests/defence_probe.rs` scripts
the attack phase with a fixed slate: three real defects from `session-cache`'s
manifest, and two claims that are false about the same file, all at the same
severity and confidence. Across three runs, **18 of 18** defender answers to
the false claims pushed back (15 refuted, 3 narrowed) and **27 of 27** answers
to the true ones accepted them (25 conceded, 2 narrowed). Not one false claim
was conceded and not one true defect was refuted. The refutations cite the
right mechanisms — GIL atomicity of `len()`, the snapshot the comprehension
takes — not lucky guesses.

**The panel is not just following the defence.** Same slate, but with the
defenders scripted to *concede* every claim, including the false ones, with a
confident wrong argument. Every judge still refuted them anyway. The panel
reads the code rather than the rebuttal.

**Three judges and one judge agreed on 7 of 7 claims.** That is why the default
is one. The panel split once, on whether an unvalidated capacity argument is a
defect or invalid-input hardening, and the plurality matched the single judge
anyway.

**Not measured:** whether evolution produces better reviewers than its parents,
whether trait variation on *reviewers* changes what they find, and anything at
all about how this behaves on a large diff.

**A thing that was tried and removed.** Cases used to plant *decoys* — code
written to look defective and not be — so that a live benchmark run could
report how many the defence refuted. Across three cases and six decoys, no
reviewer ever raised one: capable models read the code correctly and did not
take the bait, so the column printed `untested` on every run it ever ran. The
false claims survive, in the probe, where they are injected rather than hoped
for. The apparatus for hoping did not.

## Commands

```
locust init [--force]            write a config for the providers available here
locust review                    run the arena
  --base / --target              revisions (default: config)
  --generations N                rounds
  --dry-run                      the plan and the call count, spending nothing
  --json                         machine-readable outcome on stdout
  --fail-on high                 exit non-zero on a confirmed finding at this severity
locust providers                 configured providers and whether they are reachable
locust personas [--full]         the reviewer population
locust judges [--full]           the judging panel and what each temperament does
locust report [RUN] [--render]   re-print a previous run; --render rebuilds it
                                 from stored data with the current format
locust bench [--list] [--case C] run the arena against cases with known answers
  --replay RUN_DIR               re-score a stored run, spending nothing
locust clean                     remove worktrees from an interrupted run
```

`--dry-run` first. It prints the estimated model call count, which is the number
that matters: a default run is three generations of six reviewers.

## Run artifacts

```
.locust/runs/<run-id>/
  config.toml          exact config used
  context.md           what the agents were shown
  gen-0/
    personas.json      the population, with lineage
    findings.json      every claim, clustered
    rebuttals.json     every defence
    rulings.json       every verdict, with reasoning
    fitness.json       the scorecard that drove selection
  transcripts/         every model call, verbatim
  report.md
  report.json
```

Every call is written out. In a system whose output is an argument between
agents, "why did it say that" is the question you will actually need to answer,
and a scorecard without the transcript behind it is not auditable.

## Scoring

```
confirmed       +3.0 × severity_weight × altitude_weight(generation)
refuted         −1.5
undecided       +0.25
duplicate       +0.3    independently rediscovered something already confirmed
corroboration   +0.5    per extra provider that reached it independently
stale           −1.0    a restatement of something already settled
consensus       ×       scaled by panel agreement (consensus_weight, default 0.5)
```

A duplicate inherits its cluster's verdict. Rediscovering something **true**
pays `duplicate` — less than finding it first. Independently making the same
**false** claim costs the full `refuted` penalty, exactly as if you had raised
it first. Without that second half, echoing another reviewer's noise is free
while originating it is punished, and the cheapest strategy becomes agreeing
with whoever spoke first.

A confirmed finding marked stale earns only `stale_penalty`, capped at zero,
including its duplicates. Severity, altitude and corroboration add no reward.

All of it is in `[scoring]`. The two values worth thinking about before you
change them are `refuted`, which must stay negative, and `stale_penalty`, which
is what stops later generations re-reporting earlier ones.

## Status

Code review is the first target. The arena — competing agents, adversarial
defence, cross-provider adjudication, prompt evolution under a shifting fitness
function — is not specific to review, and the crate boundaries (`locust-core`,
`locust-provider`, `locust-vcs`) are drawn with that in mind. Review is the case
where the scoring signal is cleanest, so it is the one to get right first.

What is known to work, because it has been measured: the arena loop, the
defence's ability to tell a true claim from a false one, and the panel's
willingness to rule against a defence that is wrong. What is asserted but
unmeasured: that evolution improves reviewers, that reviewer temperament
changes what gets found, and that any of this scales to a large diff. The
[Ground truth](#ground-truth) section is the honest ledger, and
`cargo test -p locust-core --test defence_probe -- --ignored` re-runs the
measurements it reports.
