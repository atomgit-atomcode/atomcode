//! Adding, editing and deleting provider accounts and model profiles — the
//! decisions, once, for every screen that offers them.
//!
//! [`crate::provider_edit`] knows how to put a key into the file; this knows
//! *which* keys a gesture means: what id a new account gets, that an offer
//! picked off a list becomes an account the moment a model is added to it, that
//! leaving the protocol alone must not rewrite `deepseek` into
//! `openai-compatible`, that a deleted account takes its models with it, that a
//! default left pointing at something gone is taken out. The terminal's
//! `/provider` panel and the web page's model settings both ask here, so the
//! two cannot come to disagree about what "add a model" does to a file — and
//! both write by patching the document, so a person's comments, ordering and
//! hand-written keys survive whichever one they used.
//!
//! What it does not do is say anything to a person: a refusal is a
//! [`BookError`], and each screen words it in its own catalog (a terminal line,
//! an HTTP status and message).

use std::path::PathBuf;

use crate::config::provider::default_context_window_for;
use crate::config::provider_preset::{self, AuthKind};
use crate::config::{is_codingplan_provider_name, Config};
use crate::provider_edit::{self, AccountPatch, Edit, KeyWrite, ModelPatch};
use crate::store::ConfigStore;

/// Why a change was refused. Each screen words it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BookError {
    /// The name has nothing an id can be made from.
    NameRules,
    /// This protocol has no endpoint of its own, and none was given.
    ProtocolNeedsEndpoint,
    /// The model name is empty.
    ModelNameEmpty,
    /// A CodingPlan account: `/login` owns it.
    ManagedAccountEdit(String),
    /// A CodingPlan account: `/logout` takes it away.
    ManagedAccountDelete(String),
    /// A CodingPlan account's model list is the gateway's.
    ManagedAccountModels(String),
    /// A CodingPlan model.
    ManagedModelEdit(String),
    ManagedModelDelete(String),
    /// Nothing by this id in the file.
    NotFound(String),
    /// The file could not be read or written.
    Write(String),
}

/// A new account.
#[derive(Clone, Debug, Default)]
pub struct AccountInput<'a> {
    /// What the id is made from; sanitised, and suffixed when taken.
    pub name: &'a str,
    /// A preset (`deepseek`) or protocol (`openai-compatible`) id.
    pub protocol: &'a str,
    pub display_name: Option<&'a str>,
    /// Empty, or the preset's own default, stores nothing: the build's default
    /// keeps following the build.
    pub endpoint: &'a str,
    /// Empty stores nothing.
    pub key: Option<&'a str>,
}

/// An edit to an account's connection.
#[derive(Clone, Debug, Default)]
pub struct AccountEdit<'a> {
    pub protocol: &'a str,
    pub endpoint: &'a str,
    /// Empty keeps the stored key.
    pub key: Option<&'a str>,
    pub display_name: Edit<&'a str>,
}

/// A new model.
#[derive(Clone, Debug, Default)]
pub struct ModelInput<'a> {
    pub account: &'a str,
    pub model: &'a str,
    pub display_name: Option<&'a str>,
    /// `None` is the protocol's default window.
    pub window: Option<usize>,
    pub max_tokens: Option<usize>,
    pub vision: Option<bool>,
    pub effort: Option<&'a str>,
    pub levels: Option<&'a [String]>,
    /// A key typed beside the model, for an account that has none yet.
    pub key: Option<&'a str>,
    /// Make this the model new sessions start on.
    pub default: bool,
}

/// An edit to one model's own settings. Never touches its account.
#[derive(Clone, Debug, Default)]
pub struct ModelEdit<'a> {
    pub model: &'a str,
    /// `None` keeps the window it has.
    pub window: Option<usize>,
    pub vision: Edit<bool>,
    pub effort: Edit<&'a str>,
    pub levels: Edit<&'a [String]>,
    pub display_name: Edit<&'a str>,
    pub max_tokens: Edit<usize>,
    pub default: bool,
}

/// The configuration file's provider half, read and written.
///
/// Holds the path, not a loaded `Config`: anything else may write the file, and
/// every decision here is made against what is on disk now.
#[derive(Clone, Debug)]
pub struct ProviderBook {
    path: PathBuf,
}

impl ProviderBook {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The file a running product reads.
    pub fn default_book() -> Self {
        Self::new(Config::default_path())
    }

    /// The file as the build would use it. A file that will not parse reads as
    /// the defaults: a screen must still be able to show what is there.
    pub fn load(&self) -> Config {
        Config::load(&self.path).unwrap_or_default()
    }

    /// One patch, under the lock every config transaction takes. Returns the
    /// configuration as written.
    fn write<F>(&self, mutate: F) -> Result<Config, BookError>
    where
        F: FnOnce(&mut toml_edit::DocumentMut) -> anyhow::Result<()>,
    {
        ConfigStore::new(self.path.clone())
            .update_document(mutate)
            .map(|commit| commit.snapshot.config)
            .map_err(|error| BookError::Write(format!("{error:#}")))
    }

    /// The id a new account of this name would get in `config`.
    pub fn account_id_for(config: &Config, name: &str) -> Result<String, BookError> {
        let base = sanitize(name);
        if base.is_empty() {
            return Err(BookError::NameRules);
        }
        // Not into the gateway's namespace, or it would be taken for a managed
        // account: undeletable, and never asked for a key.
        let base = match is_codingplan_provider_name(&base) {
            true => format!("custom-{base}"),
            false => base,
        };
        Ok(free_id(&base, |candidate| {
            config.provider_accounts.contains_key(candidate)
                || config.providers.contains_key(candidate)
        }))
    }

    pub fn add_account(&self, input: &AccountInput<'_>) -> Result<String, BookError> {
        self.create_account(input, &[]).map(|(id, _)| id)
    }

    /// An account and its models, in one write: a page that collects both in one
    /// form must not leave an account with no models behind when the second half
    /// fails. `models[*].account` is ignored — they hang off the new account.
    ///
    /// Returns the account id and the model ids, in order.
    pub fn create_account(
        &self,
        input: &AccountInput<'_>,
        models: &[ModelInput<'_>],
    ) -> Result<(String, Vec<String>), BookError> {
        let config = self.load();
        let id = Self::account_id_for(&config, input.name)?;
        let preset = provider_preset::preset_or_compatible(input.protocol);
        if input.endpoint.trim().is_empty() && preset.default_base_url.is_none() {
            return Err(BookError::ProtocolNeedsEndpoint);
        }
        for model in models {
            if model.model.trim().is_empty() {
                return Err(BookError::ModelNameEmpty);
            }
        }
        let endpoint = endpoint_override(input.endpoint, preset.id);
        let key = written_key(input.key);
        let display_name = input
            .display_name
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let planned = plan_models(&config, &id, preset.provider_type.wire(), models);
        let ids: Vec<String> = planned.iter().map(|p| p.id.clone()).collect();
        let account = id.clone();
        self.write(move |document| {
            provider_edit::put_account(
                document,
                &account,
                &AccountPatch {
                    provider: preset.id,
                    base_url: endpoint,
                    api_key: key,
                    display_name: match display_name {
                        Some(name) => Edit::Set(name),
                        None => Edit::Keep,
                    },
                },
            )?;
            write_planned(document, &account, &planned)
        })?;
        Ok((id, ids))
    }

    pub fn edit_account(&self, id: &str, edit: &AccountEdit<'_>) -> Result<(), BookError> {
        let config = self.load();
        if config.account_is_codingplan_managed(id) {
            return Err(BookError::ManagedAccountEdit(id.to_string()));
        }
        let legacy =
            !config.provider_accounts.contains_key(id) && config.providers.contains_key(id);
        if !legacy && !config.provider_accounts.contains_key(id) {
            return Err(BookError::NotFound(id.to_string()));
        }
        let wanted = provider_preset::preset_or_compatible(edit.protocol);
        let stored = if legacy {
            config
                .providers
                .get(id)
                .map(|p| p.provider_type.clone())
                .unwrap_or_default()
        } else {
            config
                .provider_accounts
                .get(id)
                .map(|a| a.provider.clone())
                .unwrap_or_else(|| id.to_string())
        };
        // A stored account may name a vendor preset (`deepseek`) that speaks the
        // protocol asked for. Leaving the protocol where it was must not rewrite
        // `deepseek` into `openai-compatible`, so the wire is compared rather
        // than the id.
        let moved =
            provider_preset::preset_or_compatible(&stored).provider_type != wanted.provider_type;
        let key = written_key(edit.key);
        let endpoint = edit.endpoint.trim();
        if legacy {
            let wire = moved.then(|| wanted.provider_type.wire());
            // The flat table's writer takes what to set, or nothing: there is no
            // keyless legacy protocol to clear one for.
            let key = match key {
                KeyWrite::Set(key) => Some(key),
                KeyWrite::Keep | KeyWrite::Clear => None,
            };
            let endpoint = (!endpoint.is_empty()).then_some(endpoint);
            let id = id.to_string();
            return self
                .write(move |document| {
                    provider_edit::patch_legacy_provider(document, &id, wire, endpoint, key)
                })
                .map(|_| ());
        }
        let provider = if moved { wanted.id.to_string() } else { stored };
        let endpoint = endpoint_override(edit.endpoint, &provider).map(str::to_string);
        // A protocol with no credential of its own drops a key left over from
        // the one before it: a local Ollama carrying an OpenAI key is a file
        // holding a secret nothing will ever send.
        let clears = moved && matches!(wanted.auth_kind, AuthKind::None);
        let display_name = edit.display_name;
        let id = id.to_string();
        self.write(move |document| {
            provider_edit::put_account(
                document,
                &id,
                &AccountPatch {
                    provider: &provider,
                    base_url: endpoint.as_deref(),
                    api_key: if clears { KeyWrite::Clear } else { key },
                    display_name,
                },
            )
        })
        .map(|_| ())
    }

    /// An account and every model under it. A model profile pointing at an
    /// account that is gone is a selection that cannot resolve.
    pub fn delete_account(&self, id: &str) -> Result<(), BookError> {
        let config = self.load();
        if config.account_is_codingplan_managed(id) {
            return Err(BookError::ManagedAccountDelete(id.to_string()));
        }
        if !config.provider_accounts.contains_key(id) && !config.providers.contains_key(id) {
            return Err(BookError::NotFound(id.to_string()));
        }
        let orphans: Vec<String> = config
            .models
            .iter()
            .filter(|(_, model)| model.account == id)
            .map(|(model_id, _)| model_id.clone())
            .collect();
        let gone = id.to_string();
        self.write(move |document| {
            provider_edit::remove_account(document, &gone);
            provider_edit::remove_legacy_provider(document, &gone);
            for model in &orphans {
                provider_edit::remove_model(document, model);
            }
            Ok(())
        })?;
        self.clear_dangling_default()
    }

    pub fn add_model(&self, input: &ModelInput<'_>) -> Result<String, BookError> {
        self.add_models(input.account, std::slice::from_ref(input))
            .map(|mut ids| ids.remove(0))
    }

    /// Several models under one account, in one write. `models[*].account` is
    /// ignored in favour of `account`.
    ///
    /// An offer — a preset id with no account in the file yet — becomes an
    /// account here: adding a model to it is the gesture that turns "this build
    /// knows about deepseek" into "you have one".
    pub fn add_models(
        &self,
        account: &str,
        models: &[ModelInput<'_>],
    ) -> Result<Vec<String>, BookError> {
        let config = self.load();
        if config.account_is_codingplan_managed(account) {
            return Err(BookError::ManagedAccountModels(account.to_string()));
        }
        if models.is_empty() || models.iter().any(|m| m.model.trim().is_empty()) {
            return Err(BookError::ModelNameEmpty);
        }
        let fresh = !config.provider_accounts.contains_key(account)
            && !config.providers.contains_key(account);
        let preset_id = config
            .logical_accounts()
            .get(account)
            .map(|account| account.provider.clone())
            .unwrap_or_else(|| account.to_string());
        let preset = provider_preset::preset_or_compatible(&preset_id);
        let planned = plan_models(&config, account, preset.provider_type.wire(), models);
        let ids: Vec<String> = planned.iter().map(|p| p.id.clone()).collect();
        // A key typed beside a model is the account's; the first one wins.
        let key = written_key(models.iter().find_map(|m| m.key));
        let account = account.to_string();
        let endpoint = preset.default_base_url;
        let provider = preset.id;
        self.write(move |document| {
            if fresh {
                provider_edit::put_account(
                    document,
                    &account,
                    &AccountPatch {
                        provider,
                        base_url: endpoint,
                        api_key: key,
                        display_name: Edit::Keep,
                    },
                )?;
            } else if matches!(key, KeyWrite::Set(_)) {
                let provider = provider_of(document, &account, provider);
                let base_url = base_url_of(document, &account);
                provider_edit::put_account(
                    document,
                    &account,
                    &AccountPatch {
                        provider: &provider,
                        base_url: base_url.as_deref(),
                        api_key: key,
                        display_name: Edit::Keep,
                    },
                )?;
            }
            write_planned(document, &account, &planned)
        })?;
        Ok(ids)
    }

    pub fn edit_model(&self, id: &str, edit: &ModelEdit<'_>) -> Result<(), BookError> {
        let config = self.load();
        if config.selection_is_codingplan_managed(id) {
            return Err(BookError::ManagedModelEdit(id.to_string()));
        }
        let model = edit.model.trim().to_string();
        if model.is_empty() {
            return Err(BookError::ModelNameEmpty);
        }
        let legacy = !config.models.contains_key(id) && config.providers.contains_key(id);
        if !legacy && !config.models.contains_key(id) {
            return Err(BookError::NotFound(id.to_string()));
        }
        let logical = config.logical_models();
        let existing = logical.get(id);
        let window = edit
            .window
            .unwrap_or_else(|| existing.map(|m| m.context_window).unwrap_or(128_000));
        // The account a model hangs off is not something a model edit moves.
        let account = existing.map(|m| m.account.clone()).unwrap_or_default();
        let id = id.to_string();
        let edit = edit.clone();
        let default = edit.default;
        self.write(move |document| {
            let patch = ModelPatch {
                account: &account,
                model: &model,
                context_window: window,
                supports_vision: edit.vision,
                reasoning_effort: edit.effort,
                reasoning_effort_levels: edit.levels,
                display_name: edit.display_name,
                max_tokens: edit.max_tokens,
            };
            match legacy {
                true => provider_edit::patch_legacy_model(document, &id, &patch)?,
                false => provider_edit::put_model(document, &id, &patch)?,
            }
            if default {
                provider_edit::set_default_model(document, Some(&id));
            }
            Ok(())
        })
        .map(|_| ())
    }

    pub fn delete_model(&self, id: &str) -> Result<(), BookError> {
        let config = self.load();
        if config.selection_is_codingplan_managed(id) {
            return Err(BookError::ManagedModelDelete(id.to_string()));
        }
        if !config.models.contains_key(id) && !config.providers.contains_key(id) {
            return Err(BookError::NotFound(id.to_string()));
        }
        let gone = id.to_string();
        self.write(move |document| {
            provider_edit::remove_model(document, &gone);
            provider_edit::remove_legacy_provider(document, &gone);
            Ok(())
        })?;
        self.clear_dangling_default()
    }

    /// Point new sessions at `id`.
    pub fn set_default(&self, id: &str) -> Result<(), BookError> {
        let config = self.load();
        if config.resolve_model(Some(id)).is_err() {
            return Err(BookError::NotFound(id.to_string()));
        }
        let id = id.to_string();
        self.write(move |document| {
            provider_edit::set_default_model(document, Some(&id));
            Ok(())
        })
        .map(|_| ())
    }

    /// Take a default that no longer resolves out of the file.
    ///
    /// Read after the change rather than predicted before it: whether a
    /// selection still resolves is a question about the configuration as a
    /// whole, and the honest way to answer it is to ask the one now on disk.
    pub fn clear_dangling_default(&self) -> Result<(), BookError> {
        let config = self.load();
        let stale_model = config
            .default_model
            .as_deref()
            .is_some_and(|id| config.resolve_model(Some(id)).is_err());
        let stale_legacy = !config.default_provider.is_empty()
            && config
                .resolve_model(Some(&config.default_provider))
                .is_err();
        if !stale_model && !stale_legacy {
            return Ok(());
        }
        self.write(move |document| {
            if stale_model {
                provider_edit::set_default_model(document, None);
            }
            if stale_legacy {
                provider_edit::clear_default_provider(document);
            }
            Ok(())
        })
        .map(|_| ())
    }
}

/// One model about to be written, decided against the file as it was read.
struct Planned {
    id: String,
    model: String,
    display_name: Option<String>,
    window: usize,
    max_tokens: Option<usize>,
    vision: Option<bool>,
    effort: Option<String>,
    levels: Option<Vec<String>>,
    default: bool,
}

/// Ids and settings for `models` under `account`, each id free of the file
/// *and* of the ones planned before it — two models of one batch must not be
/// given the same id.
fn plan_models(
    config: &Config,
    account: &str,
    wire: &str,
    models: &[ModelInput<'_>],
) -> Vec<Planned> {
    let mut taken: Vec<String> = Vec::new();
    models
        .iter()
        .map(|input| {
            let model = input.model.trim().to_string();
            let base = format!("{account}/{model}");
            let id = free_id(&base, |candidate| {
                config.models.contains_key(candidate)
                    || config.providers.contains_key(candidate)
                    || taken.iter().any(|t| t == candidate)
            });
            taken.push(id.clone());
            Planned {
                id,
                model,
                display_name: input
                    .display_name
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string),
                window: input
                    .window
                    .unwrap_or_else(|| default_context_window_for(wire)),
                max_tokens: input.max_tokens,
                vision: input.vision,
                effort: input.effort.map(str::to_string),
                levels: input.levels.map(<[String]>::to_vec),
                default: input.default,
            }
        })
        .collect()
}

fn write_planned(
    document: &mut toml_edit::DocumentMut,
    account: &str,
    planned: &[Planned],
) -> anyhow::Result<()> {
    for model in planned {
        provider_edit::put_model(
            document,
            &model.id,
            &ModelPatch {
                account,
                model: &model.model,
                context_window: model.window,
                supports_vision: Edit::from_option(model.vision),
                reasoning_effort: Edit::from_option(model.effort.as_deref()),
                reasoning_effort_levels: Edit::from_option(model.levels.as_deref()),
                // New: nothing to keep, so unset means "not written".
                display_name: match model.display_name.as_deref() {
                    Some(name) => Edit::Set(name),
                    None => Edit::Keep,
                },
                max_tokens: match model.max_tokens {
                    Some(n) => Edit::Set(n),
                    None => Edit::Keep,
                },
            },
        )?;
        if model.default {
            provider_edit::set_default_model(document, Some(&model.id));
        }
    }
    Ok(())
}

/// What a typed key field means for the file.
///
/// Empty is **keep**, not clear: the stored credential is never read back into
/// a form, so there is nothing to prefill the field with — and a person who
/// opened a form to change an endpoint has not asked for their key to be thrown
/// away.
fn written_key(typed: Option<&str>) -> KeyWrite<'_> {
    match typed.map(str::trim).filter(|key| !key.is_empty()) {
        Some(key) => KeyWrite::Set(key),
        None => KeyWrite::Keep,
    }
}

/// An id a TOML table can be keyed by, from whatever a person typed.
pub fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// `base`, or the first `base-N` nobody has taken.
fn free_id(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| base.to_string())
}

/// The endpoint worth storing: `None` when it is the protocol's own default,
/// which keeps a file from pinning a URL that should follow the build.
fn endpoint_override<'a>(endpoint: &'a str, preset_id: &str) -> Option<&'a str> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return None;
    }
    let default = provider_preset::preset(preset_id).and_then(|p| p.default_base_url);
    (Some(endpoint) != default).then_some(endpoint)
}

/// What an account already says it speaks, so setting a key does not also move
/// its protocol. Falls back to the preset the caller resolved.
fn provider_of(document: &toml_edit::DocumentMut, id: &str, fallback: &str) -> String {
    document
        .get("provider_accounts")
        .and_then(|table| table.get(id))
        .and_then(|account| account.get("provider"))
        .and_then(|item| item.as_str())
        .unwrap_or(fallback)
        .to_string()
}

/// The endpoint an account already stores, so setting a key does not also take
/// it out.
fn base_url_of(document: &toml_edit::DocumentMut, id: &str) -> Option<String> {
    document
        .get("provider_accounts")
        .and_then(|table| table.get(id))
        .and_then(|account| account.get("base_url"))
        .and_then(|item| item.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HAND: &str = r#"# my providers, by hand
default_model = "mine/a"

[provider_accounts.mine]
provider = "openai-compatible"
base_url = "https://one.example.com/v1"
api_key = "sk-one"

[models."mine/a"]
# tuned by hand
account = "mine"
model = "a"
context_window = 128000
max_tokens = 4096
reasoning_effort_levels = ["low", "high"]
"#;

    fn book(name: &str, text: &str) -> (ProviderBook, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "atomcode-book-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, text).unwrap();
        (ProviderBook::new(path.clone()), path)
    }

    fn text(path: &PathBuf) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    fn model<'a>(name: &'a str) -> ModelInput<'a> {
        ModelInput {
            model: name,
            ..Default::default()
        }
    }

    /// The web page's one-pass form: a vendor, a key and two models land in one
    /// write — the preset id kept as the account's `provider`, the models named
    /// after the account, the default moved, and the person's comments intact.
    #[test]
    fn an_account_and_its_models_are_written_together() {
        let (book, path) = book("create", HAND);
        let (id, models) = book
            .create_account(
                &AccountInput {
                    name: "deepseek",
                    protocol: "deepseek",
                    display_name: Some("我的 DeepSeek"),
                    endpoint: "",
                    key: Some("sk-ds"),
                },
                &[
                    ModelInput {
                        display_name: Some("V4 Flash"),
                        window: Some(1_000_000),
                        max_tokens: Some(32_000),
                        vision: Some(false),
                        default: true,
                        ..model("deepseek-v4-flash")
                    },
                    model("deepseek-v4-pro"),
                ],
            )
            .unwrap();
        assert_eq!(id, "deepseek");
        assert_eq!(
            models,
            vec!["deepseek/deepseek-v4-flash", "deepseek/deepseek-v4-pro"]
        );
        let config = book.load();
        let account = &config.provider_accounts["deepseek"];
        assert_eq!(account.provider, "deepseek", "the vendor, not its wire");
        assert_eq!(
            account.base_url, None,
            "the preset's endpoint is not pinned"
        );
        assert_eq!(account.display_name.as_deref(), Some("我的 DeepSeek"));
        let flash = &config.models["deepseek/deepseek-v4-flash"];
        assert_eq!(flash.display_name.as_deref(), Some("V4 Flash"));
        assert_eq!(flash.context_window, 1_000_000);
        assert_eq!(flash.max_tokens, Some(32_000));
        assert_eq!(
            config.default_model.as_deref(),
            Some("deepseek/deepseek-v4-flash")
        );
        let written = text(&path);
        assert!(written.starts_with("# my providers, by hand"), "{written}");
        assert!(written.contains("# tuned by hand"), "{written}");
    }

    /// A custom protocol has no endpoint of its own; the form is refused and
    /// nothing is written — not an account with no models, not anything.
    #[test]
    fn a_custom_account_with_no_endpoint_writes_nothing() {
        let (book, path) = book("no-endpoint", HAND);
        let before = text(&path);
        let refused = book.create_account(
            &AccountInput {
                name: "gw",
                protocol: "openai-compatible",
                endpoint: " ",
                ..Default::default()
            },
            &[model("m")],
        );
        assert_eq!(refused, Err(BookError::ProtocolNeedsEndpoint));
        let empty = book.create_account(
            &AccountInput {
                name: "gw",
                protocol: "openai-compatible",
                endpoint: "https://gw.example/v1",
                ..Default::default()
            },
            &[model(" ")],
        );
        assert_eq!(empty, Err(BookError::ModelNameEmpty));
        assert_eq!(text(&path), before);
    }

    /// Two models of one batch never share an id, even when the same name is
    /// picked twice.
    #[test]
    fn one_batch_never_gives_two_models_one_id() {
        let (book, _) = book("batch-ids", HAND);
        let ids = book.add_models("mine", &[model("b"), model("b")]).unwrap();
        assert_eq!(ids, vec!["mine/b", "mine/b-2"]);
        assert_eq!(book.load().models.len(), 3);
    }

    /// An edit from a form that does not show a field leaves it alone: the web
    /// page edits a name and a window, and the hand-written output cap and
    /// effort levels survive it.
    #[test]
    fn a_model_edit_keeps_what_it_was_not_given() {
        let (book, path) = book("keep", HAND);
        book.edit_model(
            "mine/a",
            &ModelEdit {
                model: "a",
                window: Some(256_000),
                display_name: Edit::Set("Model A"),
                ..Default::default()
            },
        )
        .unwrap();
        let config = book.load();
        let a = &config.models["mine/a"];
        assert_eq!(a.context_window, 256_000);
        assert_eq!(a.display_name.as_deref(), Some("Model A"));
        assert_eq!(a.max_tokens, Some(4096));
        assert_eq!(
            a.reasoning_effort_levels.as_deref(),
            Some(&["low".to_string(), "high".to_string()][..])
        );
        assert!(text(&path).contains("# tuned by hand"));

        book.edit_model(
            "mine/a",
            &ModelEdit {
                model: "a",
                display_name: Edit::Clear,
                max_tokens: Edit::Clear,
                ..Default::default()
            },
        )
        .unwrap();
        let a = &book.load().models["mine/a"];
        assert_eq!(a.display_name, None);
        assert_eq!(a.max_tokens, None);
        assert_eq!(a.context_window, 256_000, "an unset window is kept");
    }

    /// A key typed beside a new model goes on the account — and only the key:
    /// the account's own endpoint stays where it was.
    #[test]
    fn a_key_given_with_a_model_leaves_the_accounts_endpoint() {
        let (book, _) = book("key-endpoint", HAND);
        book.add_model(&ModelInput {
            account: "mine",
            key: Some("sk-new"),
            ..model("c")
        })
        .unwrap();
        let account = &book.load().provider_accounts["mine"];
        assert_eq!(account.api_key.as_deref(), Some("sk-new"));
        assert_eq!(
            account.base_url.as_deref(),
            Some("https://one.example.com/v1")
        );
    }

    /// An account's name can be set and cleared without moving its vendor.
    #[test]
    fn an_account_edit_names_it_without_moving_its_vendor() {
        let (book, _) = book("rename", HAND);
        book.create_account(
            &AccountInput {
                name: "deepseek",
                protocol: "deepseek",
                key: Some("sk"),
                ..Default::default()
            },
            &[model("deepseek-v4-flash")],
        )
        .unwrap();
        book.edit_account(
            "deepseek",
            &AccountEdit {
                // The page sends the wire it shows; same wire, same vendor.
                protocol: "openai-compatible",
                endpoint: "https://api.deepseek.com/v1",
                display_name: Edit::Set("公司账号"),
                ..Default::default()
            },
        )
        .unwrap();
        let account = &book.load().provider_accounts["deepseek"];
        assert_eq!(account.provider, "deepseek");
        assert_eq!(account.display_name.as_deref(), Some("公司账号"));
        assert_eq!(account.api_key.as_deref(), Some("sk"), "an empty key keeps");
    }

    /// Deleting an account takes its models and the default that pointed at
    /// one of them.
    #[test]
    fn deleting_an_account_takes_its_models_and_their_default() {
        let (book, _) = book("delete", HAND);
        book.delete_account("mine").unwrap();
        let config = book.load();
        assert!(config.provider_accounts.is_empty());
        assert!(config.models.is_empty());
        assert_eq!(config.default_model, None);
    }

    #[test]
    fn a_default_that_does_not_resolve_is_refused() {
        let (book, _) = book("default", HAND);
        assert_eq!(
            book.set_default("nobody/here"),
            Err(BookError::NotFound("nobody/here".into()))
        );
        book.add_model(&model("b")).ok();
        book.add_models("mine", &[model("b")]).unwrap();
        book.set_default("mine/b").unwrap();
        assert_eq!(book.load().default_model.as_deref(), Some("mine/b"));
    }
}
