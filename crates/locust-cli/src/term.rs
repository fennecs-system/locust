//! Terminal output: colour when attached to a tty, plain otherwise.

use locust_core::arena::{Event, Reporter};
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn colour() -> bool {
    static CACHED: AtomicBool = AtomicBool::new(false);
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let on = std::io::stdout().is_terminal()
            && std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true);
        CACHED.store(on, Ordering::Relaxed);
    });
    CACHED.load(Ordering::Relaxed)
}

fn wrap(code: &str, s: &str) -> String {
    if colour() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn bold(s: &str) -> String {
    wrap("1", s)
}
pub fn dim(s: &str) -> String {
    wrap("2", s)
}
pub fn red(s: &str) -> String {
    wrap("31", s)
}
pub fn green(s: &str) -> String {
    wrap("32", s)
}
pub fn yellow(s: &str) -> String {
    wrap("33", s)
}
pub fn cyan(s: &str) -> String {
    wrap("36", s)
}

/// Live progress. Agent-level chatter is only shown with `--verbose`, because
/// a default run spawns dozens of them; the phase and generation lines are the
/// useful granularity.
pub fn reporter(verbose: bool) -> Reporter {
    Arc::new(move |event: Event| match event {
        Event::RunStarted { run_id, files, generations } => {
            eprintln!(
                "{} {} · {files} files · {generations} generations",
                bold("locust"),
                dim(&run_id)
            );
        }
        Event::GenerationStarted { generation, total, pressure, personas } => {
            eprintln!(
                "\n{} {}",
                bold(&format!("generation {}/{}", generation + 1, total)),
                dim(&format!("altitude pressure {pressure:.2}"))
            );
            eprintln!("  {} {}", dim("reviewers"), personas.join(", "));
        }
        Event::AgentStarted { label } => {
            if verbose {
                eprintln!("  {} {label}", dim("→"));
            }
        }
        Event::AgentFinished { label, produced } => {
            if verbose {
                eprintln!("  {} {label} {}", green("✓"), dim(&format!("({produced})")));
            }
        }
        Event::AgentFailed { label, error } => {
            // Always shown: a silently failing agent skews the fitness scores,
            // and the user should know the population was smaller than planned.
            eprintln!("  {} {label}: {}", yellow("!"), dim(&error));
        }
        Event::PhaseFinished { phase, count, .. } => {
            let noun = match phase {
                "attack" => "findings raised",
                "defend" => "rebuttals",
                _ => "rulings",
            };
            eprintln!("  {:<8} {count} {}", cyan(phase), dim(noun));
        }
        Event::GenerationFinished { confirmed, refuted, contested, top, .. } => {
            let split = if contested > 0 {
                format!(", {} the panel split on", bold(&contested.to_string()))
            } else {
                String::new()
            };
            eprintln!(
                "  {:<8} {} confirmed, {} refuted{split}",
                cyan("verdict"),
                bold(&confirmed.to_string()),
                refuted
            );
            if let Some(best) = top.first() {
                eprintln!(
                    "  {:<8} {} {}",
                    cyan("best"),
                    best.persona_name,
                    dim(&format!(
                        "score {:.1}, precision {:.0}%",
                        best.score,
                        best.precision * 100.0
                    ))
                );
            }
        }
        Event::Breeding { .. } => eprintln!("  {:<8} {}", cyan("breed"), dim("rewriting reviewers")),
        Event::Warning { message } => {
            eprintln!("\n{} {}\n", yellow("warn"), message);
        }
        Event::Synthesizing => eprintln!("\n{} {}", cyan("synthesis"), dim("clustering survivors")),
        Event::BudgetReached { spent, budget } => {
            eprintln!(
                "\n{} spent ${spent:.2} of a ${budget:.2} budget — stopping after this \
                 generation and reporting what the arena settled so far",
                yellow("budget")
            );
        }
        Event::RunFinished { usage, .. } => {
            let cost = if usage.cost_usd > 0.0 {
                format!(" · ${:.2}", usage.cost_usd)
            } else {
                String::new()
            };
            eprintln!(
                "\n{} {} in / {} out tokens{cost}\n",
                green("done"),
                usage.input_tokens,
                usage.output_tokens
            );
        }
    })
}
