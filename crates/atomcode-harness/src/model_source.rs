//! What a model source answers with, and the host implementation of it.
//!
//! The *seam* is `ModelSourceSvc` in `seams.rs`: the agent side says what it
//! needs, the host fills the slot. This module holds the two halves both sides
//! must agree on — the request ([`Want`]) and the answer ([`ModelEndpoint`]) —
//! plus [`ConfigAndEnv`], the implementation a stock host fills the slot with.
//!
//! Why a seam and not a function (ADR 0013): reading the config file and the
//! environment is the **host's** job, and the agent side must not know which file
//! or which variable. Before this, four rows in `plugins/` each read them with
//! their own fallbacks and their own error text — the same gateway resolved two
//! ways in one process. Now there is one implementation of that answer, and a
//! deployment that wants a different one replaces the slot.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use atomcode_kernel::provider::LlmProvider;

use atomcode_config::config::provider::ResolvedModelConfig;

/// What a row is asking for.
///
/// Stated as *what the row knows*, not as a priority: which sources rank how is
/// host policy, so a row that names its own endpoint should not also be spelling
/// out that "explicit beats the environment".
#[derive(Debug, Clone, Copy)]
pub enum Want<'a> {
    /// The row states its own endpoint and needs only the credential.
    Explicit {
        base_url: &'a str,
        model: &'a str,
        /// The variable holding the key, when the row names one. `None` means
        /// the default, and *which* variable that is is decided here — not by
        /// each row, or the default would have as many definitions as there are
        /// rows that forgot to name one.
        api_key_env: Option<&'a str>,
    },
    /// The model AtomCode is already configured to use; the selection may name a
    /// `[models.*]` entry.
    UserConfig { selection: Option<&'a str> },
    /// `ATOMCODE_BASE_URL` / `ATOMCODE_MODEL`, for a deployment with no config
    /// file.
    Environment { api_key_env: Option<&'a str> },
    /// Same as [`Want::Environment`], but the row overrides the model name —
    /// the side-call slot ("same gateway, smaller model").
    EnvironmentWithModel {
        api_key_env: Option<&'a str>,
        model: &'a str,
    },
}

/// One model a host can build a provider for, as the tree sees it.
///
/// Deliberately not `ResolvedModelConfig`: that one carries the api key and the
/// wire details, which are the host's business. This is what a ROW may know —
/// enough to choose, and nothing that would be bad to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    /// What a `model` argument names. The host's selection id, not the wire
    /// model name: a deployment may expose the same wire model twice under
    /// different settings.
    pub id: String,
    pub display_name: String,
    pub context_window: usize,
    pub supports_vision: bool,
    /// Higher is more capable. `None` means this model does not participate in
    /// capability ordering, which is also how the delegation ceiling reads it:
    /// unranked models are never offered to a subagent, because nothing is
    /// known about what delegating to them costs.
    pub capable_rank: Option<i64>,
    /// Reasoning levels this model accepts. Empty means no effort switching.
    pub effort_levels: Vec<String>,
    /// Free text from the host: what this model is good for. Never inferred
    /// from the name.
    pub note: Option<String>,
}

/// The models this host can build a provider for.
///
/// A seam, not a function, for the same reason the rest of this module is one:
/// building a provider needs auth, an account, a gateway — the host's business.
/// A row that wants to run a child on a different model names an id and asks.
///
/// Optional by design. A tree with one `llm` row and no catalog simply has no
/// choice to offer, and the rows that read this seam say so rather than
/// pretending: `task` and `team` keep running on the conversation's model.
#[async_trait::async_trait]
pub trait Models: Send + Sync {
    /// Everything the host knows about, unfiltered. Read live: a login or a
    /// `/model` can change it mid-session, so no caller may cache it.
    fn list(&self) -> Vec<ModelInfo>;
    /// The selection id the conversation itself is running on, when it is one
    /// of the above.
    fn current(&self) -> Option<String>;
    /// Build (or reuse) the provider for one id.
    async fn provider(&self, id: &str) -> Result<Arc<dyn LlmProvider>, String>;
}

/// What a subagent may be delegated to, given who is asking.
///
/// **A rank is evidence to EXCLUDE, never a requirement to be included.** The
/// first version of this had it backwards — it demanded a `capable_rank` on the
/// conversation's model and on every candidate, and returned nothing when
/// either was missing. That is correct in the sense that nothing can be ordered
/// without ranks, and useless in practice: the shipped config has a rank on one
/// model out of four, so the feature was dead on arrival for the deployment it
/// was written for, and the only fixes on offer were "change what the gateway
/// sends" and "make the person edit config.toml". Neither is a fix.
///
/// So the rule is stated as a filter with a floor:
///
/// * **the conversation's own model is always on the list.** Delegating to it
///   spends exactly what the person already chose, so there is nothing to
///   decide and nothing to guard. This is the floor the whole feature degrades
///   to, and with no catalog at all `task` and `team` still run here.
/// * **both ranked** ⇒ the ceiling applies: no stronger than the conversation.
/// * **neither ranked** ⇒ offered. Two models nobody ordered are siblings; there
///   is no evidence one costs more than the other, and refusing on no evidence
///   is how the feature died the first time.
/// * **only the candidate ranked** ⇒ withheld. A rank exists precisely to mark a
///   capability tier, and one marked against an unmarked conversation cannot be
///   placed below it.
/// * **only the conversation ranked** ⇒ the candidate is withheld for the mirror
///   reason: it cannot be shown to be at or below the ceiling.
///
/// Sorted weakest-known-first, with the unordered ones after them — so
/// [`cheapest`] can mean something even when most of the catalog says nothing.
pub fn delegatable(models: &dyn Models) -> Vec<ModelInfo> {
    let all = models.list();
    // No idea what this conversation runs on ⇒ no floor to stand on and no
    // ceiling to measure against, so nothing is offered. `task` and `team` are
    // unaffected: they take no `model` and run where they already were.
    let Some(current) = models.current() else {
        return Vec::new();
    };
    let current = Some(current);
    let here = |m: &ModelInfo| current.as_deref() == Some(m.id.as_str());
    let host_rank = all.iter().find(|m| here(m)).and_then(|m| m.capable_rank);

    let mut out: Vec<ModelInfo> = all
        .into_iter()
        .filter(|m| {
            here(m)
                || match (host_rank, m.capable_rank) {
                    (Some(ceiling), Some(rank)) => rank <= ceiling,
                    (None, None) => true,
                    _ => false,
                }
        })
        .collect();
    out.sort_by(|a, b| {
        a.capable_rank
            .is_none()
            .cmp(&b.capable_rank.is_none())
            .then_with(|| a.capable_rank.cmp(&b.capable_rank))
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// The cheapest model that can be SHOWN to be cheap, or `None`.
///
/// For the side-call slot, which wants the weakest thing that can write a title.
/// `None` when nothing in the catalog carries a rank — and `None` means the slot
/// stays empty and side calls run on the conversation's model, which is what
/// they did before any of this existed. Guessing "probably that one" from a
/// context window or a name would be the same mistake as inferring cost from a
/// model's name.
pub fn cheapest(models: &dyn Models) -> Option<ModelInfo> {
    delegatable(models)
        .into_iter()
        .filter(|m| m.capable_rank.is_some())
        .min_by(|a, b| {
            a.capable_rank
                .cmp(&b.capable_rank)
                .then_with(|| a.id.cmp(&b.id))
        })
}

/// What every caller gets back, whatever the source: one shape, so no caller has
/// to know which source answered.
#[derive(Debug, Clone)]
pub struct ModelEndpoint {
    /// The wire model name.
    pub model: String,
    pub base_url: String,
    /// The key itself. Never logged, never dumped; the rows that hold config
    /// trees deliberately take a *variable name* instead — see
    /// [`ModelSource::Environment::key_env`].
    pub api_key: String,
    /// `None` where the source does not state one, so the adapter's own default
    /// stands rather than a number invented here.
    pub context_window: Option<u32>,
    pub supports_vision: Option<bool>,
    pub thinking_type: Option<String>,
    pub thinking_keep: Option<String>,
    /// Whether the endpoint is known to accept a top-level `reasoning_effort`.
    pub supports_reasoning_effort: bool,
    /// Where this came from, for the sentence a row tells the person. Not
    /// decoration: a window someone configured and a fallback that merely looks
    /// configured are the same number, and only the source can tell them apart.
    pub origin: String,
}

/// The stock implementation: the user's config file, then the environment.
///
/// The host fills `ModelSourceSvc` with this. Its policy is the one every
/// AtomCode product already follows — a row that states an endpoint is left
/// alone, `--env-model` reads the environment, the default row reads
/// `~/.atomcode/config.toml`.
pub struct ConfigAndEnv {
    /// Where the user's config file is. Injected rather than looked up so a test
    /// can point it at a scratch directory, the same reason every other path in
    /// this crate is injected.
    pub home: PathBuf,
}

impl ConfigAndEnv {
    /// Resolve a request. A plain method, not a service: the callers are the
    /// rows that mount a model, and it has to answer *while they mount*, so a
    /// slot filled after `App::start` could not serve them anyway.
    pub fn resolve(&self, want: Want<'_>) -> Result<ModelEndpoint, String> {
        match want {
            Want::Explicit {
                base_url,
                model,
                api_key_env,
            } => Ok(ModelEndpoint::explicit(
                base_url.to_string(),
                model.to_string(),
                api_key_from_env(key_env(api_key_env))?,
                "the row's own fields",
            )),
            Want::UserConfig { selection } => {
                ModelEndpoint::from_user_config(&self.home, selection)
            }
            Want::Environment { api_key_env } => {
                ModelEndpoint::from_environment(key_env(api_key_env))
            }
            Want::EnvironmentWithModel { api_key_env, model } => {
                let mut endpoint = ModelEndpoint::from_environment(key_env(api_key_env))?;
                // The row's model replaces the source's — that is the whole point
                // of the side-call row — while everything else about the endpoint
                // carries.
                endpoint.model = model.to_string();
                Ok(endpoint)
            }
        }
    }
}

impl ModelEndpoint {
    /// The user's config file, or a message saying what to do about it.
    ///
    /// `home` is passed in rather than looked up so a test can point it at a
    /// scratch directory — the same reason every other path in this crate is
    /// injected.
    pub fn from_user_config(home: &Path, selection: Option<&str>) -> Result<Self, String> {
        let path = home.join("config.toml");
        let cfg = atomcode_config::config::Config::load(&path).map_err(|e| {
            format!(
                "no model is configured in {} ({e}).\n  \
                 - set one up by running AtomCode once, or\n  \
                 - use `--env-model` and export ATOMCODE_BASE_URL / ATOMCODE_MODEL / \
                 ATOMCODE_API_KEY",
                path.display()
            )
        })?;
        let resolved = cfg.resolve_model(selection).map_err(|e| {
            format!(
                "cannot resolve a model from {}: {e}\n  \
                 - pick one with `config = {{ model = \"…\" }}` on this row, or\n  \
                 - use `--env-model` and export ATOMCODE_BASE_URL / ATOMCODE_MODEL / \
                 ATOMCODE_API_KEY",
                path.display()
            )
        })?;
        Ok(Self::from_resolved(&resolved, &path))
    }

    /// The environment, or a message listing exactly what is missing — all of
    /// it, not whichever variable this row happened to look at first.
    pub fn from_environment(key_env: &str) -> Result<Self, String> {
        let base_url = env("ATOMCODE_BASE_URL");
        let model = env("ATOMCODE_MODEL");
        let api_key = env(key_env);
        let mut missing = Vec::new();
        if base_url.is_none() {
            missing.push("ATOMCODE_BASE_URL");
        }
        if model.is_none() {
            missing.push("ATOMCODE_MODEL");
        }
        if api_key.is_none() {
            missing.push(key_env);
        }
        if !missing.is_empty() {
            return Err(format!(
                "this row needs {}.\n  \
                 - already configured AtomCode? use the `llm-atomcode-config` row instead, \
                 which reads ~/.atomcode/config.toml\n  \
                 - just trying it out? add --offline for a scripted model that needs no key\n  \
                 - or set them: export {}=…",
                missing.join(", "),
                missing.join("=… ")
            ));
        }
        Ok(Self {
            model: model.expect("checked"),
            base_url: base_url.expect("checked"),
            api_key: api_key.expect("checked"),
            context_window: None,
            supports_vision: None,
            thinking_type: None,
            thinking_keep: None,
            // An endpoint a person wired up by hand is assumed to take the field
            // a level would need; if it does not, the adapter remembers the
            // rejection for the session. Defaulting the other way would make a
            // configured level silently do nothing.
            supports_reasoning_effort: true,
            origin: format!("the environment (`{key_env}` and ATOMCODE_BASE_URL / ATOMCODE_MODEL)"),
        })
    }

    /// The row's own fields, with whatever the source states on top.
    pub fn explicit(
        base_url: String,
        model: String,
        api_key: String,
        origin: impl Into<String>,
    ) -> Self {
        Self {
            base_url,
            model,
            api_key,
            context_window: None,
            supports_vision: None,
            thinking_type: None,
            thinking_keep: None,
            supports_reasoning_effort: true,
            origin: origin.into(),
        }
    }

    fn from_resolved(resolved: &ResolvedModelConfig, path: &Path) -> Self {
        Self {
            model: resolved.model.clone(),
            base_url: resolved.base_url.clone().unwrap_or_default(),
            api_key: resolved.api_key.clone().unwrap_or_default(),
            context_window: Some(resolved.context_window as u32),
            supports_vision: Some(resolved.supports_vision),
            thinking_type: resolved.thinking_type.clone(),
            thinking_keep: resolved.thinking_keep.clone(),
            supports_reasoning_effort: atomcode_config::config::endpoint_supports_reasoning_effort(
                resolved.reasoning_effort.as_deref(),
                resolved.reasoning_effort_levels.as_deref(),
            ),
            origin: format!("{}", path.display()),
        }
    }
}

/// The variable a row's key comes from when the row names none.
///
/// One definition, because this is a *policy* and not a row's data: with it
/// written at the call sites, a row that forgot to state its own variable had a
/// different default from the row next to it, and nothing could tell whether
/// they agreed.
pub const DEFAULT_API_KEY_ENV: &str = "ATOMCODE_API_KEY";

/// The variable name a request resolves to: what the row said, else the default.
fn key_env(named: Option<&str>) -> &str {
    named.unwrap_or(DEFAULT_API_KEY_ENV)
}

/// The trust this process runs under — `$ATOMCODE_HOME`, else `$HOME/.atomcode`.
///
/// A *named* resolver rather than an `env_var` anyone can call: "which variable
/// holds this" is a decision, and a decision belongs next to the thing it is
/// about. Handing out a generic reader is how the reads went back to being
/// scattered the first time.
pub fn atomcode_home() -> PathBuf {
    if let Some(dir) = env("ATOMCODE_HOME") {
        return PathBuf::from(dir);
    }
    user_home()
        .map(|h| h.join(".atomcode"))
        .unwrap_or_else(|| PathBuf::from(".atomcode"))
}

/// The user's home directory (`$HOME`, or `$USERPROFILE` on Windows).
///
/// Deliberately separate from `capabilities::pathutil::home_dir`, which answers
/// the same question for a different reason: that one is dependency-free so the
/// tool layer can expand a model-supplied `~` without depending on this crate.
/// This one is the tree's, for finding where a person's skills and config live.
pub fn user_home() -> Option<PathBuf> {
    #[cfg(windows)]
    let var = env("USERPROFILE");
    #[cfg(not(windows))]
    let var = env("HOME");
    var.map(PathBuf::from)
}

/// Which backend answers `web_search`, when the row does not name one.
///
/// Not a model, and here anyway: this module is where "which variable holds
/// this" is decided, and the alternative is the row reading the environment
/// itself — which is exactly the shape the criterion in
/// `tests/reasoning_effort.rs` exists to stop. It caught this one on the way in.
///
/// `None` means the row said nothing and the environment said nothing; the tool
/// picks its own default, and an unknown name falls back to it too.
pub fn web_search_provider() -> Option<String> {
    env("ATOMCODE_WEB_SEARCH_PROVIDER")
}

/// One key variable, for a caller that states its own endpoint and only needs
/// the credential.
fn api_key_from_env(key_env: &str) -> Result<String, String> {
    env(key_env).ok_or_else(|| format!("this row needs {key_env} (the key for its endpoint)"))
}

/// An environment variable, treating empty as unset — an exported-but-blank
/// variable is a mistake, not a value.
///
/// Private on purpose. A public generic reader is the escape hatch through which
/// the reads became scattered to begin with: it keeps the *spelling* in one place
/// while leaving the *decision* at every call site. Callers name a source
/// ([`Want`]) or a directory ([`atomcode_home`], [`user_home`]) instead.
fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Where the user's config file lives, given the harness home.
pub fn user_config_path(home: &Path) -> PathBuf {
    home.join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_variable_counts_as_unset() {
        // Guard against a test elsewhere having exported one.
        assert_eq!(env("ATOMCODE_DEFINITELY_NOT_SET_12345"), None);
    }

    #[test]
    fn the_environment_source_names_every_missing_variable() {
        let saved: Vec<(&str, Option<String>)> = ["ATOMCODE_BASE_URL", "ATOMCODE_MODEL"]
            .into_iter()
            .map(|k| (k, std::env::var(k).ok()))
            .collect();
        for (k, _) in &saved {
            std::env::remove_var(k);
        }
        let err = ModelEndpoint::from_environment("ATOMCODE_TEST_KEY_UNSET").expect_err("refused");
        for wanted in [
            "ATOMCODE_BASE_URL",
            "ATOMCODE_MODEL",
            "ATOMCODE_TEST_KEY_UNSET",
        ] {
            assert!(err.contains(wanted), "`{wanted}` must be named:\n{err}");
        }
        // Restore whatever the harness's own environment had.
        for (k, v) in saved {
            if let Some(v) = v {
                std::env::set_var(k, v);
            }
        }
    }

    /// A config file with no model must say what to do about it, and name the
    /// file it looked in — an error that only says "no model" leaves the reader
    /// hunting for which file was meant.
    #[test]
    fn the_config_source_names_the_file_it_looked_in() {
        let dir = std::env::temp_dir().join(format!("plexus-model-src-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let err = ModelEndpoint::from_user_config(&dir, None).expect_err("no config there");
        assert!(err.contains("config.toml"), "{err}");
        assert!(
            err.contains("--env-model"),
            "the way out must be offered: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
