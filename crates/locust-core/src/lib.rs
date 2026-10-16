//! locust — adversarial, evolutionary code review across multiple providers.
//!
//! The thesis: a single model asked to review a diff produces a list of
//! plausible-sounding line-level remarks, because that is what the task shape
//! rewards. Nothing in the loop distinguishes a remark that is true from one
//! that merely reads well, so a human has to check every one — which is the
//! work we were trying to remove.
//!
//! locust changes the shape. Reviewers compete; a defender tries to destroy
//! what they produce; an impartial judge from a different vendor rules on what
//! survives. Reviewers are scored on surviving findings, so noise costs them.
//! Between generations the winning prompts breed and the rest are culled, with
//! the weight on structural findings ramping up each round — so the arena
//! stops rewarding nitpicks and starts rewarding the problems that need the
//! whole system in view.
//!
//! What reaches the engineer is the last step: the surviving findings, clustered
//! into root-cause themes.

pub mod arena;
pub mod config;
pub mod context;
pub mod dedup;
pub mod evolution;
pub mod grounding;
pub mod model;
pub mod prompts;
pub mod report;
pub mod scoring;
pub mod seeds;
pub mod store;
pub mod traits;

pub use arena::{Arena, Event, Reporter};
pub use config::Config;
pub use model::{Finding, Judge, RunOutcome, Theme, Verdict};
pub use traits::Traits;
