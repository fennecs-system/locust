//! Does the defence actually do anything, and does the panel check it?
//!
//! A run in which every finding is confirmed might mean the reviewers are
//! excellent, or it might mean the defence concedes whatever it is handed and
//! the panel rubber-stamps. Those look identical from the outside, and no
//! amount of staring at a report tells them apart. The only thing that does is
//! feeding the arena a claim that is **false** and watching what happens.
//!
//! That used to be a fixture feature: `case.toml` planted decoys, code written
//! to look defective and not be, and the benchmark reported how many the
//! defence refuted. It never reported anything. Six decoys across three cases
//! drew no reviewer in any live run — capable models simply read the code
//! correctly — so the column printed `untested` every time and the question
//! stayed open.
//!
//! So the false claims live here instead, and the attacker is removed from the
//! question entirely. This probe scripts the attack phase with a fixed slate:
//! real defects from a case manifest, and claims that are false about the same
//! file, written in the same voice and at the same confidence. The real
//! defenders and the real panel answer them.
//!
//! * a defender that refutes the false claims and concedes the real defects is
//!   doing its job;
//! * one that concedes everything is a rubber stamp, and the pushback figure on
//!   every benchmark run means nothing;
//! * one that refutes everything is equally useless, which is why the real
//!   defects are on the slate at all. Pushback alone is not accuracy.
//!
//! A second question follows immediately. When the defence refutes a false
//! claim and the panel then refutes it too, the panel may have checked the code
//! or may simply have followed the defence — one run cannot tell those apart.
//! So the second live test scripts the defence as well, conceding every claim
//! including the false ones, and asks whether the panel still refuses them. The
//! same test rules on byte-identical input with the full panel and with a
//! single judge, which is the only controlled way to ask what extra judges buy.
//!
//! The slate is held here rather than generated, so a change in reviewer
//! prompts cannot change what the defence is asked to answer. Each false claim
//! carries `why_false`, so the slate cannot assert something is false without
//! saying what makes it so — and the fixtures' own ground-truth tests prove it.
//!
//! ```text
//! cargo test -p locust-core --test defence_probe -- --ignored --nocapture
//! ```

use anyhow::Result;
use async_trait::async_trait;
use locust_core::bench::{classify, Case, Classified, Scorecard};
use locust_core::config::{Config, JudgeConfig};
use locust_core::arena::{Arena, Event};
use locust_core::model::{AttackReport, Finding, Grounding, RawFinding, Stance, Verdict};
use locust_provider::{Provider, Request, Response, SharedProvider, Usage};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// --- the slate -------------------------------------------------------------

/// One claim handed to the defence, and the manifest entry it must land on.
///
/// `defect` is the id in `cases/session-cache/case.toml`; `kind` is whether
/// answering it correctly means refuting or conceding. The wording is
/// constrained by the manifest's keyword matching, so a claim cannot be
/// reworded freely — `the_slate_lands_where_it_was_aimed` is what catches it.
struct Claim {
    /// What a defence that is paying attention should do with this claim.
    answer: Answer,
    /// Manifest entry this must classify to, for a true claim. `None` for a
    /// claim with no answer key: the false ones, which are deliberately not
    /// planted in any case, and the arguable ones.
    defect: Option<&'static str>,
    /// For a false claim, what makes it false — the citation a defender should
    /// find. Each is also pinned by the fixture's own ground-truth tests.
    why_false: &'static str,
    /// What a wrong defence says when it concedes this claim. Written as the
    /// strongest version of the mistake — a confident concession restating the
    /// claim's own mechanism — so the panel is tested against a plausible bad
    /// defence rather than an obvious one.
    concession: &'static str,
    line: u32,
    severity: &'static str,
    confidence: f32,
    title: &'static str,
    claim: &'static str,
    scenario: &'static str,
    evidence: &'static str,
}

/// What a defender that read the code should do with a claim.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Answer {
    /// True of this code: concede it.
    Concede,
    /// False of this code: refute it. `why_false` says why.
    Refute,
    /// Accurately describes the code; whether it is a *defect* is a judgement
    /// call. On the slate to give the panel something that should divide it.
    Arguable,
}

const FILE: &str = "src/cache.py";

/// Three real defects from `session-cache`, and two claims that are false
/// about the same file. The false ones are claimed at the same severity and
/// confidence as the real ones on purpose: a hedged claim invites `narrow`
/// regardless of its truth, and the stance would stop measuring accuracy.
const SLATE: &[Claim] = &[
    Claim {
        defect: Some("hits-entries-desync"),
        answer: Answer::Concede,
        why_false: "",
        concession: "Correct as stated. The expiry path removes the key from _entries only, \
                     _hits keeps it, and _evict() can then select a key that is no longer in \
                     _entries. Both maps have to be cleared together.",
        line: 21,
        severity: "high",
        confidence: 0.85,
        title: "Expiry in get() drops the entry but leaves its _hits counter behind",
        claim: "When a read finds an expired session, `del self._entries[key]` removes it from \
                _entries while _hits keeps the key forever. _evict() minimises over _hits, so it \
                can select a key that is no longer in _entries, and the delete that follows \
                raises KeyError out of put().",
        scenario: "A session expires and the next read of it returns None, leaving 'u1' present \
                   in _hits alone. The store later reaches max_entries, _evict() picks 'u1' \
                   because its counter is the lowest of all, and `del self._entries[coldest]` \
                   raises KeyError. The put() that triggered the eviction fails and the login it \
                   was storing is lost.",
        evidence: "        if time.time() > expires_at:\n            del self._entries[key]",
    },
    Claim {
        defect: Some("unlocked-read-write"),
        answer: Answer::Concede,
        why_false: "",
        concession: "Correct as stated. get() writes shared state — the counter and the sliding \
                     expiry — without taking self._lock, so the interleaving described is real \
                     and the bound can be exceeded.",
        line: 23,
        severity: "high",
        confidence: 0.85,
        title: "get() became a writer but still runs outside the lock",
        claim: "get() now mutates shared state — it increments the hit counter and rewrites the \
                entry with a fresh expiry — yet it never takes self._lock, which put() holds for \
                every write it makes. The lock therefore serialises writers against each other \
                only, and a read racing a write can resurrect an entry that write removed.",
        scenario: "The store is at max_entries. Thread A in get('u1') has read the entry and \
                   passed the expiry check when thread B's put('u2') takes the lock, evicts 'u1' \
                   as the coldest key and inserts 'u2'. Thread A resumes and writes \
                   self._entries['u1'] back with a fresh expiry, so the removed session is alive \
                   again and the store holds max_entries + 1.",
        evidence: "        self._hits[key] = self._hits.get(key, 0) + 1\n        \
                   self._entries[key] = (value, time.time() + self.ttl)",
    },
    Claim {
        defect: Some("lfu-not-lru"),
        answer: Answer::Concede,
        why_false: "",
        concession: "Correct as stated. The docstring advertises LRU and _evict() minimises over \
                     a lifetime hit counter that never decays, which is LFU. The documented \
                     behaviour and the implemented behaviour do not match.",
        line: 36,
        severity: "medium",
        confidence: 0.8,
        title: "The docstring promises LRU; _evict() implements frequency, not recency",
        claim: "The class docstring says LRU eviction, and _evict() takes min(self._hits) — a \
                lifetime counter that never decays. That is LFU without decay, not LRU: a \
                session read once an hour ago ranks below one read fifty times yesterday.",
        scenario: "A long-lived service account accumulates thousands of reads. A user logs in \
                   and their session is read twice. At capacity the new session is evicted on \
                   their next request, despite being the most recently used entry in the store, \
                   and they are signed out mid-session while the stale account entry stays \
                   resident indefinitely.",
        evidence: "        coldest = min(self._hits, key=self._hits.get)",
    },
    Claim {
        defect: None,
        answer: Answer::Refute,
        why_false: "len() on a dict is atomic under the GIL, so the count always belongs to \
                    some real state of the store, and the method's own comment says callers \
                    only compare it against zero. Pinned by cases/session-cache's ground truth.",
        concession: "Correct as stated. The read happens outside self._lock while writers hold \
                     it, so the count a caller receives can already be wrong by the time it acts \
                     on it. size() should take the lock like every other accessor.",
        line: 42,
        severity: "medium",
        confidence: 0.8,
        title: "size() reads _entries without the lock",
        claim: "size() returns len(self._entries) without taking self._lock, while writers \
                mutate that dict while holding it. A caller reading size() concurrently with a \
                write observes a count belonging to no consistent state of the store, so any \
                decision made from it can be wrong.",
        scenario: "A drain loop polls size() while request threads are still writing. Because \
                   the read is outside the lock, the loop can observe a count taken partway \
                   through another thread's critical section — concluding the store is empty \
                   while a session is still resident, and shutting down under it.",
        evidence: "        return len(self._entries)",
    },
    Claim {
        defect: None,
        answer: Answer::Refute,
        why_false: "The comprehension iterates list(self._entries.items()), a snapshot taken \
                    before any mutation, and the deletions run afterwards over that list. \
                    pop(k, None) also tolerates a key another thread already removed.",
        concession: "Correct as stated. The sweep removes keys from the same mapping it is \
                     walking, which CPython rejects at runtime. The keys have to be collected \
                     into a separate list before any deletion happens.",
        line: 47,
        severity: "medium",
        confidence: 0.8,
        title: "purge_expired() mutates _entries while iterating it",
        claim: "purge_expired() walks self._entries.items() and removes keys from that same dict \
                inside the loop body. CPython detects the mutation on the next step of the \
                iterator and raises RuntimeError: dictionary changed size during iteration, so \
                the sweep dies partway through.",
        scenario: "Any call with two or more expired keys. The first pop mutates _entries and \
                   the iterator raises on the following key, so the sweep aborts with expired \
                   sessions still resident and the exception surfaces in whichever request \
                   thread ran the sweep.",
        evidence: "        for k in stale:\n            self._entries.pop(k, None)",
    },
];

/// Claims with no answer key, because there is no defensible one: both
/// describe the code accurately and whether either is a *defect* is a
/// judgement call. They exist to give the panel something that should divide
/// it. A panel that is unanimous even here is a panel whose extra members are
/// buying nothing on this fixture.
const ARGUABLE: &[Claim] = &[
    Claim {
        defect: None,
        answer: Answer::Arguable,
        why_false: "",
        concession: "Correct as stated. Every read extends the expiry, so the configured TTL \
                     bounds idle time rather than session lifetime.",
        line: 25,
        severity: "medium",
        confidence: 0.7,
        title: "Sliding expiry lets a stolen session live indefinitely",
        claim: "Every read rewrites the entry's expiry to now + ttl, so a client that touches \
                any authenticated endpoint more often than the TTL keeps its session alive \
                forever. ttl_seconds then bounds idle time only, not session lifetime, which is \
                not what a 300-second session timeout is usually taken to mean.",
        scenario: "A session token is captured. The holder polls a cheap authenticated endpoint \
                   every four minutes. The session never reaches its expiry and survives until \
                   the process restarts or the entry is evicted, with no absolute lifetime \
                   anywhere in the store to stop it.",
        evidence: "        self._entries[key] = (value, time.time() + self.ttl)",
    },
    Claim {
        defect: None,
        answer: Answer::Arguable,
        why_false: "",
        concession: "Correct as stated. max_entries is accepted without validation and a value \
                     of 0 reaches min() on an empty mapping.",
        line: 30,
        severity: "low",
        confidence: 0.7,
        title: "max_entries is unvalidated, and 0 makes the first write raise ValueError",
        claim: "__init__ stores max_entries without checking it. With max_entries=0 the capacity \
                test passes on the very first write, _evict() runs against an empty _hits, and \
                min() raises ValueError: min() arg is an empty sequence.",
        scenario: "A deployment reads the capacity from configuration and the value is absent, \
                   so it arrives as 0. The first login raises ValueError out of the store rather \
                   than failing a validation check at construction, and every request after it \
                   does the same.",
        evidence: "            if len(self._entries) >= self.max_entries:",
    },
];

/// The claims as an attacker would have returned them.
fn attack_json(claims: &[&Claim]) -> String {
    let findings: Vec<String> = claims
        .iter()
        .map(|c| {
            serde_json::json!({
                "file": FILE,
                "line": c.line,
                "category": "correctness",
                "title": c.title,
                "claim": c.claim,
                "failure_scenario": c.scenario,
                "evidence": c.evidence,
                "severity": c.severity,
                "altitude": "local",
                "confidence": c.confidence,
            })
            .to_string()
        })
        .collect();
    format!(r#"{{"findings":[{}]}}"#, findings.join(","))
}

fn slate() -> Vec<&'static Claim> {
    SLATE.iter().collect()
}

fn slate_and_arguable() -> Vec<&'static Claim> {
    SLATE.iter().chain(ARGUABLE).collect()
}

fn case() -> Result<Case> {
    Case::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../cases/session-cache"))
}

// --- offline ---------------------------------------------------------------

/// The slate is only a measurement if each claim lands where it should: a true
/// claim credited to the defect it was written for, a false one credited to
/// nothing. Manifest keywords are shared across neighbouring entries, so a
/// reworded true claim can silently match two and land in triage — and a false
/// claim that drifts into matching a real entry would be scored as a find.
#[test]
fn the_slate_lands_where_it_was_aimed() -> Result<()> {
    let case = case()?;
    let claims = slate_and_arguable();
    let report: AttackReport = serde_json::from_str(&attack_json(&claims))?;
    assert_eq!(report.findings.len(), claims.len());

    for (claim, raw) in claims.iter().zip(report.findings) {
        let expected = match claim.defect {
            Some(id) => Classified::Real { defect: id.into() },
            // A false or arguable claim must stay unplaced. Crediting one to a
            // planted defect would score a wrong answer as a find and quietly
            // move the recall denominator.
            None => Classified::Unclassified,
        };
        let finding = seeded_finding(raw);
        assert_eq!(
            classify(&finding, &case.manifest.defects),
            expected,
            "claim `{}` no longer classifies as expected",
            claim.title
        );
        // Manifest order must not decide it: an ambiguous claim that happens to
        // hit the right entry first is still ambiguous.
        let mut reversed = case.manifest.defects.clone();
        reversed.reverse();
        assert_eq!(
            classify(&finding, &reversed),
            expected,
            "claim `{}` depends on manifest order",
            claim.title
        );
    }
    Ok(())
}

/// A false claim must say what makes it false, and a true one must not
/// pretend to. Without this the slate could drift into asserting that
/// something is false because the run happened to refute it, which is how a
/// measurement turns into a tautology.
#[test]
fn every_false_claim_says_why_it_is_false() {
    for claim in slate_and_arguable() {
        match claim.answer {
            Answer::Refute => assert!(
                claim.why_false.len() > 40,
                "`{}` is on the slate as false with no citation for why",
                claim.title
            ),
            _ => assert!(
                claim.why_false.is_empty(),
                "`{}` is not a false claim but carries a why_false",
                claim.title
            ),
        }
    }
    assert!(
        SLATE.iter().any(|c| c.answer == Answer::Refute),
        "a slate with nothing false measures nothing"
    );
    assert!(
        SLATE.iter().any(|c| c.answer == Answer::Concede),
        "a slate with nothing true cannot tell refusal from reflex"
    );
}

fn seeded_finding(raw: RawFinding) -> Finding {
    Finding {
        id: "probe".into(),
        generation: 0,
        persona: "probe".into(),
        persona_id: "probe".into(),
        provider: "probe".into(),
        raw,
        cluster: "probe".into(),
        duplicate_of: None,
        corroboration: 1,
        grounding: Grounding::Verified,
    }
}

// --- the provider that carries the script ----------------------------------

/// Wraps a real provider and answers scripted phases from a fixed string.
/// Everything else goes to the model untouched, and the wrapper reports the
/// inner provider's name, so authorship — and therefore the judge rotation
/// that excludes it — behaves exactly as in a live run.
struct Seeded {
    inner: SharedProvider,
    /// `None` for a provider that should raise nothing, which keeps every
    /// finding authored by one vendor and leaves the others free to judge.
    attack: Option<String>,
    /// When set, defenders are scripted too: every finding in the batch is
    /// answered with this stance and the claim's own concession text.
    defence: Option<Stance>,
}

impl Seeded {
    /// Build the rebuttal JSON for whichever findings this batch contains.
    /// Ids are read back out of the rendered prompt, the same way the arena's
    /// scripted-provider tests do it, so a batch split cannot desynchronise
    /// the script from the findings.
    fn scripted_defence(&self, stance: Stance, prompt: &str) -> String {
        let stance = format!("{stance:?}").to_lowercase();
        let mut out = Vec::new();
        for line in prompt.lines() {
            let Some(rest) = line.trim().strip_prefix("**") else { continue };
            let Some((id, _)) = rest.split_once("**") else { continue };
            if !id.starts_with('f') || !id.contains('-') {
                continue;
            }
            // The title follows the id on a later line; match the claim by the
            // location rendered next to it instead.
            let argument = slate_and_arguable()
                .iter()
                .find(|c| line.contains(&format!("{FILE}:{}", c.line)))
                .map(|c| c.concession)
                .unwrap_or("Correct as stated.");
            out.push(format!(
                r#"{{"finding_id":"{id}","stance":"{stance}","argument":{},"evidence":""}}"#,
                serde_json::to_string(argument).expect("string is serialisable")
            ));
        }
        format!(r#"{{"rebuttals":[{}]}}"#, out.join(","))
    }
}

#[async_trait]
impl Provider for Seeded {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    fn needs_workspace(&self) -> bool {
        self.inner.needs_workspace()
    }

    async fn complete(&self, req: Request) -> Result<Response> {
        // Phases are told apart by the agent label the arena assigns —
        // `gen0-atk-*`, `gen0-def-*`, `gen0-judge0-*`. Anything not scripted
        // here, judging above all, must reach the real model.
        let scripted = if req.label.contains("-atk-") {
            Some(self.attack.clone().unwrap_or_else(|| r#"{"findings":[]}"#.to_string()))
        } else if req.label.contains("-def-") {
            self.defence.map(|stance| self.scripted_defence(stance, &req.prompt))
        } else {
            None
        };
        match scripted {
            Some(text) => Ok(Response {
                text,
                usage: Usage::default(),
                provider: self.name().to_string(),
                model: self.model().to_string(),
            }),
            None => self.inner.complete(req).await,
        }
    }
}

// --- shared live setup -----------------------------------------------------

/// Config for a one-generation run over the materialised case, with the
/// project's real providers and panel.
fn probe_config(case: &Case) -> Result<Config> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut cfg = Config::load(&root.join("locust.toml"))?;
    cfg.run.base = "main".into();
    cfg.run.target = "change".into();
    cfg.run.generations = 1;
    cfg.arena.attackers = 1;
    // Themes cost a call and say nothing about the defence or the panel.
    cfg.synthesis.enabled = false;
    cfg.context.notes = case.manifest.notes.clone();
    cfg.validate()?;
    Ok(cfg)
}

fn materialise(case: &Case, tag: &str) -> Result<PathBuf> {
    let workdir =
        std::env::temp_dir().join(format!("locust-probe-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&workdir)?;
    case.materialise(&workdir)
}

fn providers(cfg: &Config, claims: &[&Claim], defence: Option<Stance>) -> Result<Vec<SharedProvider>> {
    let mut out: Vec<SharedProvider> = Vec::new();
    for (i, pc) in cfg.active_providers().into_iter().enumerate() {
        let inner = pc.build_with(&cfg.sandbox)?;
        // Only the first provider raises the slate, so the panel still has a
        // vendor that authored nothing in front of it.
        let attack = (i == 0).then(|| attack_json(claims));
        out.push(Arc::new(Seeded { inner, attack, defence }));
    }
    Ok(out)
}

fn reporter() -> locust_core::Reporter {
    Arc::new(|e: Event| {
        if let Event::AgentFailed { label, error } = e {
            eprintln!("  agent failed: {label}: {error}");
        }
    })
}

/// What the run actually did with one claim.
struct Outcome {
    stances: Vec<String>,
    panel: Vec<String>,
    verdict: Option<Verdict>,
    dissent: f64,
}

fn responses(gen: &locust_core::model::GenerationOutcome, claims: &[&Claim]) -> Vec<Outcome> {
    claims
        .iter()
        .map(|claim| {
            let Some(finding) = gen.findings.iter().find(|f| f.raw.title == claim.title) else {
                return Outcome { stances: Vec::new(), panel: Vec::new(), verdict: None, dissent: 0.0 };
            };
            // A duplicate is answered under its cluster's canonical member.
            let answered = finding.duplicate_of.as_deref().unwrap_or(&finding.id);
            let stances = gen
                .rebuttals
                .iter()
                .filter(|r| r.raw.finding_id == answered)
                .map(|r| format!("{:?}", r.raw.stance).to_lowercase())
                .collect();
            let ruling = gen.rulings.iter().find(|r| r.raw.finding_id == answered);
            Outcome {
                stances,
                panel: ruling
                    .map(|r| {
                        r.panel
                            .iter()
                            .map(|v| format!("{:?}", v.raw.verdict).to_lowercase()[..4].to_string())
                            .collect()
                    })
                    .unwrap_or_default(),
                verdict: ruling.map(|r| r.raw.verdict),
                dissent: ruling.map_or(0.0, |r| r.dissent),
            }
        })
        .collect()
}

fn short(title: &str) -> String {
    title.chars().take(40).collect()
}

fn verdict_name(v: Option<Verdict>) -> String {
    v.map_or("no ruling".into(), |v| format!("{v:?}").to_lowercase())
}

// --- live: is the defence motivated? ---------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spends live model quota"]
async fn the_defence_answers_fixed_true_and_false_claims() -> Result<()> {
    let case = case()?;
    let cfg = probe_config(&case)?;
    let claims = slate();
    let repo = materialise(&case, "defence")?;
    let provs = providers(&cfg, &claims, None)?;

    println!(
        "probe: {} claims ({} true, {} false) over {} provider(s), {} defenders, {} judges",
        claims.len(),
        claims.iter().filter(|c| c.answer == Answer::Concede).count(),
        claims.iter().filter(|c| c.answer == Answer::Refute).count(),
        provs.len(),
        cfg.arena.defenders,
        cfg.panel()?.len(),
    );

    let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
    let arena = Arena::with_providers(cfg, vcs, provs, reporter())?;
    let run_dir = arena.run_dir().to_path_buf();
    let outcome = arena.run().await?;
    let gen = &outcome.generations[0];

    println!("\nclaim                                     stances            panel        verdict");
    for (claim, answer) in claims.iter().zip(responses(gen, &claims)) {
        let want = match claim.answer {
            Answer::Concede => "concede",
            Answer::Refute => "refute",
            Answer::Arguable => "either",
        };
        println!(
            "{:<41} {:<18} {:<12} {}  (wanted {want})",
            short(claim.title),
            answer.stances.join("/"),
            answer.panel.join("/"),
            verdict_name(answer.verdict),
        );
    }

    // This is the measurement the whole probe exists for: of the claims that
    // are false, how many did the defence refuse, and of the claims that are
    // true, how many did it accept. Both halves are needed — refusing
    // everything scores perfectly on one of them alone.
    let mut refused = 0;
    let mut false_answers = 0;
    let mut accepted = 0;
    let mut true_answers = 0;
    for (claim, answer) in claims.iter().zip(responses(gen, &claims)) {
        for stance in &answer.stances {
            match claim.answer {
                Answer::Refute => {
                    false_answers += 1;
                    // `narrow` is pushback: the defender did not take the
                    // claim at face value. Only `concede` is failing to check.
                    refused += usize::from(stance != "concede");
                }
                Answer::Concede => {
                    true_answers += 1;
                    accepted += usize::from(stance != "refute");
                }
                Answer::Arguable => {}
            }
        }
    }
    println!(
        "\nfalse claims refused {refused}/{false_answers}   \
         true claims accepted {accepted}/{true_answers}   pushback {:.0}%   ${:.2}",
        Scorecard::build(&case, &outcome).defence_pushback() * 100.0,
        outcome.usage.cost_usd,
    );
    println!("transcripts: {}", run_dir.display());

    // The probe is a measurement, not a gate: it asserts only that the defence
    // and the panel were actually exercised, because a run where the claims
    // never reached them says nothing either way.
    for (claim, answer) in claims.iter().zip(responses(gen, &claims)) {
        assert!(!answer.stances.is_empty(), "no defender answered `{}`", claim.title);
        assert!(answer.verdict.is_some(), "no ruling for `{}`", claim.title);
    }
    assert!(
        gen.rebuttals.iter().any(|r| r.raw.stance != Stance::Concede)
            || gen.rulings.iter().any(|r| r.raw.verdict != Verdict::Confirmed),
        "every claim was conceded and confirmed, the false ones included: \
         the defence did nothing this run"
    );
    Ok(())
}

// --- live: does the panel check the defence, and is it worth three? --------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spends live model quota"]
async fn the_panel_is_tested_against_a_defence_that_concedes_everything() -> Result<()> {
    let case = case()?;
    let claims = slate_and_arguable();
    let base = probe_config(&case)?;

    // One judge, no temperament, so the comparison is about panel size rather
    // than about which preset happened to be first.
    let mut single = base.clone();
    single.judges = vec![toml::from_str::<JudgeConfig>("name = \"solo\"")?];
    single.validate()?;

    println!(
        "panel probe: {} claims ({} true, {} false, {} arguable), \
         defence scripted to concede every one",
        claims.len(),
        claims.iter().filter(|c| c.answer == Answer::Concede).count(),
        claims.iter().filter(|c| c.answer == Answer::Refute).count(),
        claims.iter().filter(|c| c.answer == Answer::Arguable).count(),
    );

    let mut arms = Vec::new();
    for (tag, cfg) in [("panel", base), ("single", single)] {
        let repo = materialise(&case, tag)?;
        let provs = providers(&cfg, &claims, Some(Stance::Concede))?;
        let judges = cfg.panel()?.len();
        let vcs: Arc<dyn locust_vcs::Vcs> = Arc::from(locust_vcs::detect(&repo, Some("git"))?);
        let arena = Arena::with_providers(cfg, vcs, provs, reporter())?;
        let outcome = arena.run().await?;
        let gen = &outcome.generations[0];

        // The script is only a control if it actually landed: a defence that
        // failed to parse would leave the judges with no rebuttal at all,
        // which is a different experiment.
        assert!(
            gen.rebuttals.iter().all(|r| r.raw.stance == Stance::Concede),
            "{tag}: the scripted defence did not concede everything"
        );
        assert!(!gen.rebuttals.is_empty(), "{tag}: no rebuttals reached the panel");

        arms.push((tag, judges, responses(gen, &claims), outcome.usage.cost_usd, Scorecard::build(&case, &outcome)));
    }

    println!("\n{:<41} {:<22} {:<22}", "claim (defence conceded all)", "panel of 3", "single judge");
    for (i, claim) in claims.iter().enumerate() {
        let want = match claim.answer {
            Answer::Concede => "should confirm",
            Answer::Refute => "should refute",
            Answer::Arguable => "arguable",
        };
        let cell = |arm: &(&str, usize, Vec<Outcome>, f64, Scorecard)| {
            let a = &arm.2[i];
            format!("{} {}", verdict_name(a.verdict), if a.panel.len() > 1 { format!("[{}]", a.panel.join("/")) } else { String::new() })
        };
        println!(
            "{:<41} {:<22} {:<22} {want}",
            short(claim.title),
            cell(&arms[0]),
            cell(&arms[1]),
        );
    }

    for (tag, judges, answers, cost, card) in &arms {
        let dissenting = answers.iter().filter(|a| a.dissent > 0.0).count();
        let held = claims
            .iter()
            .zip(answers)
            .filter(|(c, a)| {
                c.answer == Answer::Refute && a.verdict == Some(Verdict::Refuted)
            })
            .count();
        let false_claims = claims.iter().filter(|c| c.answer == Answer::Refute).count();
        println!(
            "\n{tag}: {judges} judge(s), ${cost:.2}, split on {dissenting}/{} claims, \
             refused {held}/{false_claims} false claims the defence had conceded \
             ({} confirmed findings)",
            answers.len(),
            card.findings_confirmed,
        );
    }

    let agree = arms[0]
        .2
        .iter()
        .zip(&arms[1].2)
        .filter(|(a, b)| a.verdict == b.verdict)
        .count();
    println!(
        "\npanel and single judge agree on {agree}/{} claims; panel cost ${:.2} vs ${:.2}",
        claims.len(),
        arms[0].3,
        arms[1].3,
    );

    for (tag, _, answers, _, _) in &arms {
        for (claim, answer) in claims.iter().zip(answers) {
            assert!(answer.verdict.is_some(), "{tag}: no ruling for `{}`", claim.title);
        }
    }
    Ok(())
}
