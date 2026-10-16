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
  judge     a provider that authored none of them rules on each,
            and merges repeats the token overlap missed
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

## Why multiple providers

Two reasons, and only the second is about quality of ideas.

**Judging.** Models prefer their own output. A model judging findings its own
agents raised grades them generously, and the whole tournament quietly becomes a
popularity contest. By default locust routes each batch of findings to a
provider that authored none of them (`judge_excludes_author`).

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

## Isolation

Each agent that needs a filesystem gets its own checkout at the target revision,
under `.locust/wt/`, destroyed when it finishes. `git worktree` is the default
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
locust report [RUN]              re-print a previous run
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
```

A duplicate inherits its cluster's verdict. Rediscovering something **true**
pays `duplicate` — less than finding it first. Independently making the same
**false** claim costs the full `refuted` penalty, exactly as if you had raised
it first. Without that second half, echoing another reviewer's noise is free
while originating it is punished, and the cheapest strategy becomes agreeing
with whoever spoke first.

All of it is in `[scoring]`. The two values worth thinking about before you
change them are `refuted`, which must stay negative, and `stale_penalty`, which
is what stops later generations re-reporting earlier ones.

## Status

Code review is the first target. The arena — competing agents, adversarial
defence, cross-provider adjudication, prompt evolution under a shifting fitness
function — is not specific to review, and the crate boundaries (`locust-core`,
`locust-provider`, `locust-vcs`) are drawn with that in mind. Review is the case
where the scoring signal is cleanest, so it is the one to get right first.
