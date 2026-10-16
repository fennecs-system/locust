//! `locust.toml` — the configurable surface.
//!
//! Everything the arena does is declared here: which models compete, who
//! judges, how findings are scored, and how the population breeds between
//! generations. Personas are just prompts, so a team's review standards live
//! in version control next to the code they apply to.

use anyhow::{bail, Context, Result};
use locust_provider::api::Wire;
use locust_provider::cli::{CliProvider, CliSpec, Extract};
use locust_provider::{api, Provider, ProviderSettings, SharedProvider};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::model::{Origin, Persona, Role};

pub const CONFIG_FILENAME: &str = "locust.toml";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub run: RunConfig,
    #[serde(default)]
    pub arena: ArenaConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub scoring: ScoringConfig,
    #[serde(default)]
    pub evolution: EvolutionConfig,
    #[serde(default)]
    pub synthesis: SynthesisConfig,
    #[serde(default, rename = "provider")]
    pub providers: Vec<ProviderConfig>,
    #[serde(default, rename = "persona")]
    pub personas: Vec<PersonaConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    /// Revision the change is measured against.
    pub base: String,
    /// Revision under review.
    pub target: String,
    /// How many attack/defend/judge cycles to run. Each one raises the
    /// altitude pressure and bans the previous round's findings.
    pub generations: u32,
    /// Maximum agents in flight at once.
    pub parallelism: usize,
    /// `auto` prefers jj when the repo is a jj workspace.
    pub vcs: VcsChoice,
    /// Whether agents get their own checkout. `auto` gives one to any provider
    /// that can use it.
    pub workspaces: WorkspacePolicy,
    /// Restrict the arena to a subset of the configured providers.
    #[serde(default)]
    pub providers: Vec<String>,
    /// Stop the run if estimated spend exceeds this. 0 disables the check.
    #[serde(default)]
    pub budget_usd: f64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            base: "main".into(),
            target: "HEAD".into(),
            generations: 3,
            parallelism: 6,
            vcs: VcsChoice::Auto,
            workspaces: WorkspacePolicy::Auto,
            providers: Vec::new(),
            budget_usd: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VcsChoice {
    #[default]
    Auto,
    Git,
    Jj,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WorkspacePolicy {
    /// A checkout for any provider that can make use of one.
    #[default]
    Auto,
    /// A checkout for every agent, including API providers (lets you point a
    /// future tool-using API agent at it).
    Always,
    /// No checkouts; agents see only the context pack.
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArenaConfig {
    /// Attackers per generation, spread across the enabled providers.
    pub attackers: usize,
    /// Defenders per generation.
    pub defenders: usize,
    /// Provider that rules on contested findings.
    pub judge: Option<String>,
    /// Never let a provider judge a finding one of its own agents raised.
    /// Self-preference is the main bias in LLM-as-judge setups.
    pub judge_excludes_author: bool,
    /// How many findings one judge call may handle. Smaller batches cost more
    /// but reason better.
    pub judge_batch: usize,
}

impl Default for ArenaConfig {
    fn default() -> Self {
        Self {
            attackers: 6,
            defenders: 3,
            judge: None,
            judge_excludes_author: true,
            judge_batch: 8,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextConfig {
    /// Include the post-change contents of each touched file, not just the
    /// hunks. API providers cannot open files, so without this they review a
    /// diff through a keyhole.
    pub include_full_files: bool,
    pub max_file_bytes: usize,
    pub max_total_bytes: usize,
    /// Paths excluded from review (globs are matched on substrings).
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Extra instructions pasted into every agent's prompt: house style,
    /// known-acceptable patterns, the service's threat model.
    #[serde(default)]
    pub notes: String,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            include_full_files: true,
            max_file_bytes: 60_000,
            max_total_bytes: 400_000,
            exclude: vec![
                "Cargo.lock".into(),
                "package-lock.json".into(),
                "pnpm-lock.yaml".into(),
                "yarn.lock".into(),
                "go.sum".into(),
                ".min.js".into(),
                "/vendor/".into(),
                "/node_modules/".into(),
                "/target/".into(),
            ],
            notes: String::new(),
        }
    }
}

/// The fitness function. A persona's score is the sum over the findings it
/// raised, weighted by severity and by altitude-at-this-generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScoringConfig {
    pub confirmed: f64,
    /// Negative: raising noise has to cost something, or every persona
    /// converges on spraying low-confidence guesses.
    pub refuted: f64,
    pub undecided: f64,
    /// Credit for independently rediscovering what another agent already
    /// found. Positive but small.
    pub duplicate: f64,
    /// Added per extra provider that independently raised the same cluster.
    pub corroboration_bonus: f64,
    /// Applied when the judge marks a finding as a restatement of something
    /// already settled in an earlier generation.
    pub stale_penalty: f64,
    /// Scale confirmed findings by the judge's severity rating.
    pub severity_weighted: bool,
}

impl Default for ScoringConfig {
    fn default() -> Self {
        Self {
            confirmed: 3.0,
            refuted: -1.5,
            undecided: 0.25,
            duplicate: 0.3,
            corroboration_bonus: 0.5,
            stale_penalty: -1.0,
            severity_weighted: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvolutionConfig {
    pub enabled: bool,
    /// Top performers carried into the next generation verbatim.
    pub elite: usize,
    /// Winners rewritten by the meta-prompt in light of their scorecard.
    pub mutants: usize,
    /// New personas blending two winners.
    pub crossovers: usize,
    /// Unrelated personas pulled from the seed library each generation.
    pub immigrants: usize,
    /// Provider that rewrites personas. Defaults to the judge.
    pub meta_provider: Option<String>,
    /// Personas scoring below this are culled regardless of rank.
    pub cull_below: f64,
}

impl Default for EvolutionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            elite: 2,
            mutants: 2,
            crossovers: 1,
            immigrants: 1,
            meta_provider: None,
            cull_below: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynthesisConfig {
    /// Cluster the surviving findings into root-cause themes at the end.
    pub enabled: bool,
    pub provider: Option<String>,
    pub max_themes: usize,
}

impl Default for SynthesisConfig {
    fn default() -> Self {
        Self { enabled: true, provider: None, max_themes: 8 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// locust calls the HTTP API itself.
    Api,
    /// locust shells out to an agentic CLI inside the agent's worktree.
    Cli,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub name: String,
    pub kind: ProviderKind,
    #[serde(default = "enabled_default")]
    pub enabled: bool,

    // --- api ---
    /// Wire format. Defaults from `name` when it is a known vendor.
    #[serde(default)]
    pub wire: Option<Wire>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,

    // --- cli ---
    /// Shorthand for a known agentic CLI: `claude`, `codex`, `gemini`.
    #[serde(default)]
    pub preset: Option<String>,
    /// argv, with `{system}` / `{prompt}` / `{model}` placeholders.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub stdin: Option<String>,
    #[serde(default)]
    pub usage_path: Option<String>,

    // --- shared ---
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub input_cost_per_mtok: f64,
    #[serde(default)]
    pub output_cost_per_mtok: f64,
    #[serde(default)]
    pub max_retries: Option<u32>,

    // Kept last so the generated config reads scalars-first; the TOML
    // serialiser reorders tables for us, so this is presentation only.
    #[serde(default)]
    pub extract: Option<Extract>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

fn enabled_default() -> bool {
    true
}

impl ProviderConfig {
    fn settings(&self) -> ProviderSettings {
        let d = ProviderSettings::default();
        ProviderSettings {
            name: self.name.clone(),
            model: self.model.clone().unwrap_or_else(|| default_model(&self.name, self.wire)),
            max_tokens: self.max_tokens.unwrap_or(d.max_tokens),
            temperature: self.temperature.unwrap_or(d.temperature),
            timeout_secs: self.timeout_secs.unwrap_or(d.timeout_secs),
            input_cost_per_mtok: self.input_cost_per_mtok,
            output_cost_per_mtok: self.output_cost_per_mtok,
            max_retries: self.max_retries.unwrap_or(d.max_retries),
        }
    }

    /// Instantiate. Fails loudly on a missing key or an unknown preset so a
    /// run does not silently proceed with half its population.
    pub fn build(&self) -> Result<SharedProvider> {
        let settings = self.settings();
        match self.kind {
            ProviderKind::Api => {
                let wire = self
                    .wire
                    .or_else(|| infer_wire(&self.name))
                    .with_context(|| {
                        format!(
                            "provider `{}`: set `wire` to one of anthropic/open_ai/gemini",
                            self.name
                        )
                    })?;
                let key_env = self
                    .api_key_env
                    .clone()
                    .unwrap_or_else(|| default_key_env(wire).to_string());
                let p: Arc<dyn Provider> = match wire {
                    Wire::Anthropic => Arc::new(api::AnthropicProvider::new(
                        settings,
                        self.base_url.clone(),
                        &key_env,
                    )?),
                    Wire::OpenAi => {
                        Arc::new(api::OpenAiProvider::new(settings, self.base_url.clone(), &key_env)?)
                    }
                    Wire::Gemini => {
                        Arc::new(api::GeminiProvider::new(settings, self.base_url.clone(), &key_env)?)
                    }
                };
                Ok(p)
            }
            ProviderKind::Cli => {
                let mut spec = match &self.preset {
                    Some(name) => {
                        locust_provider::cli::preset(name)
                            .map(|(spec, _)| spec)
                            .with_context(|| {
                                format!(
                                    "provider `{}`: unknown preset `{name}` (try claude, codex, gemini)",
                                    self.name
                                )
                            })?
                    }
                    None => CliSpec {
                        command: Vec::new(),
                        stdin: None,
                        extract: Extract::Raw,
                        usage_path: None,
                        env: HashMap::new(),
                    },
                };
                // Explicit fields override the preset.
                if let Some(c) = &self.command {
                    spec.command = c.clone();
                }
                if self.stdin.is_some() {
                    spec.stdin = self.stdin.clone();
                }
                if let Some(e) = &self.extract {
                    spec.extract = e.clone();
                }
                if self.usage_path.is_some() {
                    spec.usage_path = self.usage_path.clone();
                }
                spec.env.extend(self.env.clone());

                if spec.command.is_empty() {
                    bail!("provider `{}`: set `preset` or `command`", self.name);
                }
                Ok(Arc::new(CliProvider::new(settings, spec)?))
            }
        }
    }
}

fn infer_wire(name: &str) -> Option<Wire> {
    let n = name.to_ascii_lowercase();
    if n.contains("anthropic") || n.contains("claude") {
        Some(Wire::Anthropic)
    } else if n.contains("gemini") || n.contains("google") {
        Some(Wire::Gemini)
    } else if n.contains("openai") || n.contains("gpt") || n.contains("openrouter") || n.contains("groq")
    {
        Some(Wire::OpenAi)
    } else {
        None
    }
}

fn default_key_env(wire: Wire) -> &'static str {
    match wire {
        Wire::Anthropic => "ANTHROPIC_API_KEY",
        Wire::OpenAi => "OPENAI_API_KEY",
        Wire::Gemini => "GEMINI_API_KEY",
    }
}

fn default_model(name: &str, wire: Option<Wire>) -> String {
    match wire.or_else(|| infer_wire(name)) {
        Some(Wire::Anthropic) => "claude-opus-5".into(),
        Some(Wire::OpenAi) => "gpt-5".into(),
        Some(Wire::Gemini) => "gemini-2.5-pro".into(),
        None => "unknown".into(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonaConfig {
    pub name: String,
    pub role: Role,
    pub prompt: String,
    #[serde(default)]
    pub focus: String,
    #[serde(default)]
    pub temperature: Option<f32>,
}

impl PersonaConfig {
    pub fn to_persona(&self, generation: u32) -> Persona {
        Persona {
            id: format!("{}-{}", self.role.short(), slug(&self.name)),
            name: self.name.clone(),
            role: self.role,
            prompt: self.prompt.clone(),
            focus: self.focus.clone(),
            temperature: self.temperature.unwrap_or(0.8),
            generation,
            parent: None,
            origin: Origin::Seed,
        }
    }
}

pub fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = true;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Walk up from `start` looking for `locust.toml`.
    pub fn discover(start: &Path) -> Option<std::path::PathBuf> {
        let mut dir = Some(start);
        while let Some(d) = dir {
            let candidate = d.join(CONFIG_FILENAME);
            if candidate.is_file() {
                return Some(candidate);
            }
            dir = d.parent();
        }
        None
    }

    pub fn validate(&self) -> Result<()> {
        if self.providers.iter().filter(|p| p.enabled).count() == 0 {
            bail!("no enabled providers; add at least one [[provider]] to {CONFIG_FILENAME}");
        }
        let mut seen = std::collections::HashSet::new();
        for p in &self.providers {
            if !seen.insert(&p.name) {
                bail!("duplicate provider name `{}`", p.name);
            }
        }
        for name in [&self.arena.judge, &self.evolution.meta_provider, &self.synthesis.provider]
            .into_iter()
            .flatten()
        {
            if !self.providers.iter().any(|p| &p.name == name) {
                bail!("`{name}` is named as a role provider but is not configured");
            }
        }
        for name in &self.run.providers {
            if !self.providers.iter().any(|p| &p.name == name) {
                bail!("[run] providers lists `{name}`, which is not configured");
            }
        }
        if self.arena.attackers == 0 {
            bail!("[arena] attackers must be at least 1");
        }
        if self.run.generations == 0 {
            bail!("[run] generations must be at least 1");
        }
        if self.run.parallelism == 0 {
            bail!("[run] parallelism must be at least 1");
        }
        Ok(())
    }

    /// Providers taking part in this run, honouring `[run] providers`.
    pub fn active_providers(&self) -> Vec<&ProviderConfig> {
        self.providers
            .iter()
            .filter(|p| p.enabled)
            .filter(|p| self.run.providers.is_empty() || self.run.providers.contains(&p.name))
            .collect()
    }

    /// Seed personas, falling back to the built-in library when the config
    /// declares none.
    pub fn seed_personas(&self, role: Role) -> Vec<Persona> {
        let configured: Vec<Persona> = self
            .personas
            .iter()
            .filter(|p| p.role == role)
            .map(|p| p.to_persona(0))
            .collect();
        if configured.is_empty() {
            crate::seeds::library(role)
        } else {
            configured
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_filesystem_safe() {
        assert_eq!(slug("Concurrency Hawk!"), "concurrency-hawk");
        assert_eq!(slug("  --weird__name  "), "weird-name");
    }

    #[test]
    fn wire_inference_covers_the_common_names() {
        assert_eq!(infer_wire("claude-cli"), Some(Wire::Anthropic));
        assert_eq!(infer_wire("openrouter"), Some(Wire::OpenAi));
        assert_eq!(infer_wire("mystery-vendor"), None);
    }

    #[test]
    fn empty_provider_list_is_rejected() {
        let cfg = Config::default();
        assert!(cfg.validate().is_err());
    }

    fn sample_provider(name: &str, kind: ProviderKind) -> ProviderConfig {
        ProviderConfig {
            name: name.into(),
            kind,
            enabled: true,
            wire: None,
            model: None,
            base_url: None,
            api_key_env: None,
            preset: Some("claude".into()),
            command: None,
            stdin: None,
            usage_path: None,
            max_tokens: None,
            temperature: None,
            timeout_secs: None,
            input_cost_per_mtok: 0.0,
            output_cost_per_mtok: 0.0,
            max_retries: None,
            extract: Some(Extract::JsonPath("result".into())),
            env: HashMap::from([("FOO".into(), "bar".into())]),
        }
    }

    /// Every run snapshots its config to disk and `locust init` writes one
    /// from scratch, so a config that cannot make the trip out and back is a
    /// runtime failure the type-checker will never catch. `extract` is an
    /// enum and `env` a map, which are the two fields most likely to break it.
    #[test]
    fn config_survives_a_toml_round_trip() {
        let mut cfg = Config::default();
        cfg.providers.push(sample_provider("claude-cli", ProviderKind::Cli));
        cfg.providers.push(sample_provider("anthropic", ProviderKind::Api));
        cfg.personas.push(PersonaConfig {
            name: "migration safety".into(),
            role: Role::Attacker,
            prompt: "You review every change as though it will be half-deployed.".into(),
            focus: "deploy windows".into(),
            temperature: Some(0.7),
        });

        let text = toml::to_string_pretty(&cfg).expect("config must serialise to TOML");
        let back: Config = toml::from_str(&text).expect("and parse back");

        assert_eq!(back.providers.len(), 2);
        assert_eq!(back.providers[0].extract, Some(Extract::JsonPath("result".into())));
        assert_eq!(back.providers[0].env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(back.personas[0].name, "migration safety");
        assert_eq!(back.run.generations, cfg.run.generations);
        back.validate().unwrap();
    }

    /// `locust init` writes a commented template; it has to parse.
    #[test]
    fn unknown_keys_are_rejected_rather_than_silently_ignored() {
        let text = "[run]\nbase = \"main\"\ntarget = \"HEAD\"\ngenerations = 2\n\
                    parallelism = 4\nvcs = \"auto\"\nworkspaces = \"auto\"\ngenrations = 9\n";
        // A typo in a config key should be an error, not a silently ignored
        // setting that makes the run behave differently than the file says.
        assert!(toml::from_str::<Config>(text).is_err());
    }

    #[test]
    fn role_provider_must_exist() {
        let mut cfg = Config::default();
        cfg.providers.push(sample_provider("anthropic", ProviderKind::Api));
        cfg.arena.judge = Some("nobody".into());
        assert!(cfg.validate().is_err());
        cfg.arena.judge = Some("anthropic".into());
        assert!(cfg.validate().is_ok());
    }
}
