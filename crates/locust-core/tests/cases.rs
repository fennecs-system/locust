//! Every case's manifest, checked against the kind of report it will receive.
//!
//! A manifest is an answer key, and a wrong answer key is worse than none: a
//! benchmark that credits a finding to the wrong defect, or leaves a correct
//! identification unplaced, sends the next person hunting a reviewer weakness
//! that does not exist. That has already happened once here — a `matches`
//! entry of `["_hits", "0"]` matched every finding that mentioned `_hits`,
//! because `"0"` is one character.
//!
//! So each planted defect carries sample reports written the way a reviewer
//! writes them, including the phrasings most likely to collide with a
//! neighbouring entry, and each must land on its own defect and no other —
//! in either manifest order, since `classify` must not be decided by the order
//! entries happen to appear in the file.

use locust_core::bench::{classify, Case, Classified};
use locust_core::model::{Finding, Grounding, RawFinding, Severity};
use std::path::{Path, PathBuf};

fn cases_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../cases")
}

/// `(case, defect, file, line, title, claim)` — the claim is where the
/// keywords live, and is written as prose a reviewer would actually produce.
type Sample = (
    &'static str,
    &'static str,
    &'static str,
    u32,
    &'static str,
    &'static str,
);

const SAMPLES: &[Sample] = &[
    // --- retry-budget ----------------------------------------------------
    (
        "retry-budget",
        "retries-non-idempotent",
        "src/lib.rs",
        56,
        "send() retries writes that are not idempotent",
        "send() retries on any Err regardless of req.method. A POST to /charges whose response \
         was lost is replayed up to three more times, and the payments API has no idempotency \
         key, so one authorisation becomes four charges.",
    ),
    (
        "retry-budget",
        "budget-never-replenished",
        "src/lib.rs",
        65,
        "The retry budget is process-wide and never refilled",
        "budget is one AtomicU32 per Client shared by every task in the worker, and nothing ever \
         refills it. After a bad spell spends the 100 retries, no request in that process is \
         retried again for the lifetime of the worker.",
    ),
    (
        "retry-budget",
        "budget-exhaustion-wraps",
        "src/lib.rs",
        65,
        "fetch_sub on an exhausted budget wraps",
        "Once the budget reaches zero, fetch_sub(1) wraps to u32::MAX, so the exhausted state \
         lasts exactly one call before four billion retries become available again.",
    ),
    (
        "retry-budget",
        "retries-client-errors",
        "src/lib.rs",
        61,
        "Deterministic client errors are retried",
        "The Err arm does not look at the error at all, so Error::Status(400) and Status(404) \
         are retried three times each. A request the server has already rejected on its merits \
         cannot succeed on a second attempt; the client just turns one rejection into four and \
         reports the failure 350ms later than it could.",
    ),
    (
        "retry-budget",
        "timeout-is-per-attempt",
        "src/lib.rs",
        56,
        "The caller's timeout now bounds one attempt, not the call",
        "The doc comment still promises that the caller's timeout bounds the call, but the same \
         timeout is handed to every attempt. A caller asking for two seconds can now wait four \
         timeouts plus 350ms of backoff before it hears anything.",
    ),
    (
        "permission-check",
        "middleware-treats-raiser-as-predicate",
        "src/middleware.py",
        8,
        "authorize() still treats the permission check as a predicate",
        "check_permission() returns None and signals refusal by raising, but authorize() tests \
         its result. `not None` is True, so every request that should be allowed is answered \
         403, and a request that should be refused raises out of the middleware instead.",
    ),
    (
        "permission-check",
        "denial-swallowed-in-save-invoice",
        "src/api.py",
        18,
        "The permission denial is logged and discarded",
        "save_invoice() wraps the check in `except Exception` and only writes a log line, so a \
         refusal is swallowed and the function returns normally. Any caller that reaches the \
         endpoint can write an invoice.",
    ),
    (
        "permission-check",
        "write-precedes-the-check",
        "src/api.py",
        15,
        "The invoice is written before the permission is checked",
        "store.write(invoice) happens before check_permission() is called, so even a denial \
         that propagated correctly would arrive after the row was persisted.",
    ),
    (
        "permission-check",
        "cache-keyed-on-user-alone",
        "src/permissions.py",
        25,
        "The permission cache key ignores the permission",
        "The cache key is user.id alone, so the answer to the first permission asked about a \
         user is returned for every later one. A viewer passing invoice:read then passes \
         user:manage.",
    ),
];

fn finding(file: &str, line: u32, title: &str, claim: &str) -> Finding {
    Finding {
        id: "sample".into(),
        generation: 0,
        persona: "sample".into(),
        persona_id: "sample".into(),
        provider: "sample".into(),
        raw: RawFinding {
            file: file.into(),
            line: Some(line),
            category: String::new(),
            title: title.into(),
            claim: claim.into(),
            failure_scenario: String::new(),
            // Quoted code is excluded from matching on purpose: it carries
            // identifiers from every neighbouring defect.
            evidence: "fn send() { self.budget.fetch_sub(1, Ordering::Relaxed); }".into(),
            severity: Severity::High,
            altitude: Default::default(),
            confidence: 0.8,
        },
        cluster: "sample".into(),
        duplicate_of: None,
        corroboration: 1,
        grounding: Grounding::Verified,
    }
}

#[test]
fn every_sample_report_lands_on_its_own_defect() {
    for (case_name, defect_id, file, line, title, claim) in SAMPLES {
        let case = Case::load(&cases_dir().join(case_name))
            .unwrap_or_else(|e| panic!("loading case `{case_name}`: {e:#}"));
        assert!(
            case.manifest.defects.iter().any(|d| d.id == *defect_id),
            "`{case_name}` has no defect `{defect_id}`"
        );
        let expected = Classified::Real {
            defect: defect_id.to_string(),
        };

        let f = finding(file, *line, title, claim);
        assert_eq!(
            classify(&f, &case.manifest.defects),
            expected,
            "`{case_name}` / `{defect_id}`: {title}"
        );
        let mut reversed = case.manifest.defects.clone();
        reversed.reverse();
        assert_eq!(
            classify(&f, &reversed),
            expected,
            "`{case_name}` / `{defect_id}` depends on manifest order: {title}"
        );
    }
}

/// Every planted defect has at least one sample. Without this, adding a defect
/// and forgetting to exercise its keywords is silent, and the first time
/// anyone finds out is a live run that reports it as missed.
#[test]
fn every_planted_defect_has_a_sample_report() {
    for case in Case::discover(&cases_dir()).expect("cases load") {
        for defect in &case.manifest.defects {
            // session-cache's keyword discrimination is pinned by its own
            // tests in bench.rs, written from the verbatim text of the live
            // run that exposed the collision.
            if case.manifest.name == "session-cache" {
                continue;
            }
            assert!(
                SAMPLES
                    .iter()
                    .any(|(c, d, ..)| *c == case.manifest.name && *d == defect.id),
                "`{}` / `{}` has no sample report",
                case.manifest.name,
                defect.id
            );
        }
    }
}

/// Every defect must be matchable and both trees must exist. `Case::load`
/// enforces that; this makes sure it is run against every case in the
/// directory, including one added without touching this file.
#[test]
fn every_case_in_the_directory_loads() {
    let cases = Case::discover(&cases_dir()).expect("cases load");
    assert!(
        cases.len() >= 3,
        "expected at least three cases, found {}",
        cases.len()
    );
    for case in &cases {
        assert!(
            case.real().count() > 0,
            "`{}` plants no defect",
            case.manifest.name
        );
    }
}

/// Verbatim from the first live benchmark run of these cases, 20 October 2026.
///
/// Reconstructed claims prove nothing: one regression written to pin an
/// earlier keyword collision passed against both the broken and the fixed
/// manifest, because the paraphrase had dropped the very token that caused
/// the collision. These are the exact strings the arena produced, including
/// the passing mentions of `budget` and `except Exception` that decide where
/// several of them land.
///
/// `(case, expected defect or None for a deliberate straddle, file, line,
///  title, claim, failure scenario)`
type LiveSample = (
    &'static str,
    Option<&'static str>,
    &'static str,
    u32,
    &'static str,
    &'static str,
    &'static str,
);

const FROM_THE_LIVE_RUN: &[LiveSample] = &[
    (
        "retry-budget", Some("budget-exhaustion-wraps"), "src/lib.rs", 66,
        "Budget fetch_sub wraps 0 to u32::MAX, so exhausting the budget makes it effectively unlimited",
        "`fetch_sub(1)` on an `AtomicU32` wraps on underflow. When the budget is 0, the call stores 4294967295 and returns 0. Only that one caller is refused; every later caller sees a value other than 0 and gets to retry. Once the budget runs out, the client can spend about 4 billion more retries.",
        "The payments API goes down. Tasks on the shared Client use up the 100 retries. Task A's failure then hits `fetch_sub` while the budget is 0: it gets 0, returns Err, and the stored budget becomes u32::MAX. From then on every task's failed request is retried 3 times with backoff, so the guard stops limiting the extra load on the struggling upstream in the case it was written for. With N concurrent tasks, each of the first several racers that reaches 0 can also be the one that wraps it, but a single wrap is enough.",
    ),
    (
        "retry-budget", Some("retries-non-idempotent"), "src/lib.rs", 58,
        "Retrying non-idempotent payment requests after Transport errors can charge twice",
        "`send` retries every `Err` for every `req.method`, including POST. This covers `Error::Transport` timeouts and connection resets, where the server may already have processed the request. The payments API has no idempotency keys, so a retry repeats the payment that already went through.",
        "A worker sends a POST /charges. The server commits the charge, but the response is delayed past `self.timeout`, or the connection resets before the body arrives. round_trip returns `Err(Transport(\"timeout\"))`. Since attempt 0 < 3 and the budget is above 0, the client sleeps 50ms and sends the same POST again. The server creates a second charge and returns Ok, so the caller sees one success while the customer has been billed twice. The same thing happens for a 5xx returned after a partial commit.",
    ),
    (
        "retry-budget", Some("timeout-is-per-attempt"), "src/lib.rs", 56,
        "send no longer respects the caller's timeout bound",
        "A billing worker calling Client::send can now block for four timeout periods plus 350 milliseconds, despite the retained contract that the caller's timeout bounds the call.",
        "A worker constructs Client with a one-second timeout and sends a request while the upstream stalls. Each round_trip times out after one second. With the initial budget available, send makes four attempts and sleeps for 50, 100, and 200 milliseconds, returning after approximately 4.35 seconds instead of within one second.",
    ),
    (
        "retry-budget", Some("retries-client-errors"), "src/lib.rs", 61,
        "Deterministic 4xx Status errors are retried and drain the shared budget",
        "`Error::Status(u16)` is retried just like a transport failure. Client errors such as 400, 402, 404 and 422 will fail the same way every time, yet each one costs 3 retries from the process-wide budget plus 350ms of sleeping.",
        "A batch sends 34 requests that get 402 or 422 (for example, declined cards). Each one uses 3 retries, so 102 retries in total exhaust RETRY_BUDGET. After that, real transient failures (5xx or resets) on other tasks get no retries. Also, each 4xx response reaches its caller only after an extra 50+100+200ms of blocking plus 3 more round trips to the payments API.",
    ),
    (
        "permission-check", Some("cache-keyed-on-user-alone"), "src/permissions.py", 25,
        "Permission cache keyed by user.id only, so the first answer is reused for every permission",
        "`_CACHE` stores one boolean per `user.id`, not per `(user.id, permission)`. Whichever permission is checked first in a request decides the result of every later `check_permission` call for that user, whatever permission it asks about.",
        "In one request a viewer is checked for 'invoice:read'. `_CACHE[uid] = True` is stored. Later in the same request `check_permission(viewer, 'invoice:write')` finds `cached = True` and returns without raising, so the viewer gets write access. The reverse also happens: a clerk first checked for 'user:manage' caches False, and the clerk is then denied 'invoice:read'. The result depends on the order of checks within the request.",
    ),
    (
        "permission-check", Some("middleware-treats-raiser-as-predicate"), "src/middleware.py", 8,
        "authorize rejects permitted users and propagates denials",
        "authorize still treats the permission check as a boolean, but check_permission returns None on success and raises Denied on failure. Permitted requests therefore return 403, while denied requests raise instead of returning the documented forbidden response.",
        "With an empty cache, call authorize with a viewer and a handler whose permission is invoice:read. It returns (403, \"forbidden\") without invoking the handler. Changing the permission to invoice:write causes Denied to escape.",
    ),
    (
        "permission-check", None, "src/api.py", 16,
        "save_invoice writes before the permission check and ignores Denied",
        "save_invoice calls store.write(invoice) before it calls check_permission. It then catches any exception and only logs a warning, so a user without invoice:write still gets their invoice saved and the function returns normally.",
        "A viewer (role 'viewer', who has only invoice:read) calls save_invoice. store.write(invoice) runs first. check_permission raises Denied('invoice:write'), the `except Exception` catches it and logs a warning, and the function returns invoice.id. The unauthorized write is now saved and the caller sees success. Before this change the function raised Forbidden before writing.",
    ),
];

#[test]
fn the_findings_the_arena_actually_produced_land_where_they_should() {
    for (case_name, expected_id, file, line, title, claim, scenario) in FROM_THE_LIVE_RUN {
        let case = Case::load(&cases_dir().join(case_name)).expect("case loads");
        let mut f = finding(file, *line, title, claim);
        f.raw.failure_scenario = scenario.to_string();

        match expected_id {
            Some(id) => assert_eq!(
                classify(&f, &case.manifest.defects),
                Classified::Real {
                    defect: id.to_string()
                },
                "`{case_name}`: {title}"
            ),
            // The straddle: one finding describing two planted defects. It
            // must stay unplaced rather than being credited to whichever
            // entry comes first, and the scorecard reports both candidates
            // instead of calling the uncredited one missed.
            None => {
                assert_eq!(
                    classify(&f, &case.manifest.defects),
                    Classified::Unclassified,
                    "`{case_name}`: {title}"
                );
                let mut ids: Vec<&str> = locust_core::bench::matching(&f, &case.manifest.defects)
                    .iter()
                    .map(|d| d.id.as_str())
                    .collect();
                ids.sort_unstable();
                assert_eq!(
                    ids,
                    [
                        "denial-swallowed-in-save-invoice",
                        "write-precedes-the-check"
                    ],
                    "the straddle must name both defects it describes"
                );
            }
        }
    }
}
