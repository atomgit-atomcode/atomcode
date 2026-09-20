//! The providers port for `atomcode --tui`, filled by the launcher.
//!
//! The other half of `atomcode_tui::providers`: the screen draws the accounts
//! and models and works them; this knows which file they live in, what the
//! schema calls them, and what has to happen to the running graph when one
//! changes (`docs/adr/0022` §3).
//!
//! Three facts decide the shape:
//!
//! - **What a person sees is the *logical* configuration.** `logical_accounts`
//!   and `logical_models` are the one answer that folds the new
//!   `[provider_accounts]` / `[models]` schema together with the legacy flat
//!   `[providers.*]` table and the CodingPlan group. The previous `/provider`
//!   read `config.providers` directly and was therefore blind to every account
//!   in the new schema — this is the bug this module exists to end.
//! - **A write is a document patch.** `atomcode_config::provider_edit` touches
//!   the keys the panel owns and leaves a person's comments, ordering and
//!   hand-written keys alone, the way the settings port does.
//! - **A credential goes in and never comes out.** `api_key` is written when the
//!   person types one and is never read back into a row: what crosses the seam
//!   is [`AccountRow::has_key`], a bool. The same rule `ProviderChoice::about`
//!   and `HostConfig::identity` follow, for the same reason — these answers are
//!   drawn on a screen and kept in a log (`docs/adr/0021`).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_config::config::{provider_preset, Config};
use atomcode_config::provider_edit::{self, AccountPatch, KeyWrite, ModelPatch};
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::module::{Modules, Mounted};
use atomcode_tui::plugin::ModulesSvc;
use atomcode_tui::providers::{
    AccountDraft, AccountRow, ModelDraft, ModelRow, Protocol, Providers, ProvidersView,
};
use serde_json::Value;

/// The row's name, one string shared by the plugin and the layer that names it.
pub const ROW: &str = "tui-panel-providers";

/// The row that puts the providers panel on screen.
///
/// **The launcher's row, not the screen's**, the same as `tui-panel-settings`
/// and for the same reason: `atomcode-tui` ships the panel's *view* and knows
/// nothing about a configuration file. The row exists because *this* product has
/// providers to manage.
pub struct ProvidersRow;

#[async_trait]
impl Plugin for ProvidersRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    fn description(&self) -> &'static str {
        "the provider panel: accounts and models as this launcher reads them, and an edit going back over the seam"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
        let view = Arc::new(Mounted::<atomcode_tui::modules::providers::Providers>::new());
        let id = <atomcode_tui::modules::providers::Providers as atomcode_tui::module::View>::id();
        mods.add_view(view)?;
        let m: Arc<Modules> = mods.clone();
        let _ = ctx.effect(move || m.remove_view(id));
        Ok(())
    }
}

/// The layer that puts the row on screen.
///
/// `[[insert]]` rather than a patch: the screen's own tree does not name this
/// row at all, because a screen with no providers port has no providers to
/// manage. So the launcher inserts the row it owns, and the two travel together.
pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// The protocols the add/edit form cycles through, in display order.
///
/// The four `tuix` offers: the two generic custom endpoints, the keyless local
/// one, and OpenAI's newer wire. A vendor preset is *not* in here — a person
/// adding "deepseek" reaches it as an offer on the account list, and putting
/// forty vendors in a left-right toggle would make the toggle useless.
const PROTOCOLS: [&str; 4] = [
    "openai-compatible",
    "anthropic-compatible",
    "ollama",
    "openai-responses",
];

/// Where the providers live, and how to write them.
///
/// Holds the path rather than a loaded `Config`, for the reason
/// [`crate::tui_settings::ConfigSettings`] does: a value read once at start-up
/// goes stale the moment anything else writes the file, and the point of the
/// panel is that it shows what is *there*.
pub struct ConfigProviders {
    pub path: PathBuf,
}

impl ConfigProviders {
    pub fn new(path: PathBuf) -> Arc<Self> {
        Arc::new(Self { path })
    }

    fn load(&self) -> Config {
        // A file that will not parse is not an error here: the panel shows what
        // the build would use. Refusing to open it because the file is broken
        // would leave a person with no way to see what is wrong.
        Config::load(&self.path).unwrap_or_default()
    }

    fn read(&self) -> ProvidersView {
        let config = self.load();
        ProvidersView::new(
            accounts(&config),
            models(&config),
            protocols(),
            atomcode_config::config::REASONING_EFFORT_LEVELS
                .iter()
                .map(|level| level.to_string())
                .collect(),
        )
    }

    /// One patch, under the same lock every other config transaction takes.
    fn write<F>(&self, mutate: F) -> Result<(), String>
    where
        F: FnOnce(&mut toml_edit::DocumentMut) -> anyhow::Result<()>,
    {
        atomcode_config::ConfigStore::new(self.path.clone())
            .update_document(mutate)
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
    }
}

/// The account list: what is configured, then what could be.
///
/// Configured accounts first, by how many models they carry — an account a
/// person actually uses is the one they are looking for — then every preset
/// vendor that is not set up yet, as an offer. Pure-legacy `[providers.*]`
/// entries are **not** listed here: they are one row on the model list, which is
/// where a flat entry is one selectable thing rather than an account with models
/// under it.
fn accounts(config: &Config) -> Vec<AccountRow> {
    let logical = config.logical_accounts();
    let models = config.logical_models();
    let mut configured: Vec<(String, usize)> = logical
        .keys()
        .filter(|id| {
            config.provider_accounts.contains_key(*id)
                || atomcode_config::config::is_codingplan_provider_name(id)
        })
        .map(|id| {
            let count = models.values().filter(|m| &m.account == id).count();
            (id.clone(), count)
        })
        .collect();
    configured.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut out: Vec<AccountRow> = configured
        .into_iter()
        .map(|(id, count)| {
            let account = logical.get(&id);
            let preset = account
                .map(|a| provider_preset::preset_or_compatible(&a.provider))
                .unwrap_or(&provider_preset::OPENAI_COMPATIBLE);
            let managed = config.account_is_codingplan_managed(&id);
            AccountRow {
                label: label_for(config, &id),
                protocol: protocol_label(preset.provider_type),
                endpoint: account
                    .and_then(|a| a.base_url.clone())
                    .or_else(|| preset.default_base_url.map(str::to_string))
                    .unwrap_or_default(),
                models: count,
                // A gateway account signs with an OAuth token rather than a key
                // in the file, and a row saying "no credential" about an account
                // that works would be a row telling a person to fix nothing.
                has_key: managed || account.is_some_and(has_stored_key),
                managed,
                configured: true,
                id,
            }
        })
        .collect();

    // The offers. A vendor is only worth offering when it has an endpoint to
    // dispatch against that is not the gateway: the `*-compatible` presets have
    // none (they are reached through "add", where a person types one), and the
    // gateway's own account is signed in through `/login`, not typed in here.
    for preset in provider_preset::PRESETS {
        let dispatchable = preset
            .default_base_url
            .is_some_and(|url| !atomcode_auth::gateway_crypto::is_atomgit_gateway(url));
        if !dispatchable
            || PROTOCOLS.contains(&preset.id)
            || atomcode_config::config::is_codingplan_provider_name(preset.id)
            || out.iter().any(|row| row.id == preset.id)
        {
            continue;
        }
        out.push(AccountRow {
            id: preset.id.to_string(),
            label: preset.display_name.to_string(),
            protocol: protocol_label(preset.provider_type),
            endpoint: preset.default_base_url.unwrap_or_default().to_string(),
            models: 0,
            has_key: false,
            managed: false,
            configured: false,
        });
    }
    out
}

/// The model list, grouped by account — the same order `/model` lists them in.
fn models(config: &Config) -> Vec<ModelRow> {
    let logical = config.logical_models();
    let mut ids: Vec<&String> = logical.keys().collect();
    ids.sort_by_key(|id| {
        logical
            .get(*id)
            .map(|m| (m.account.clone(), m.model.clone()))
            .unwrap_or_else(|| ((*id).clone(), String::new()))
    });
    let default = config.effective_model_selection();
    ids.into_iter()
        .filter_map(|id| {
            let model = logical.get(id)?;
            Some(ModelRow {
                id: id.clone(),
                account: model.account.clone(),
                model: model.model.clone(),
                window: model.context_window,
                vision: model.supports_vision,
                effort: model.reasoning_effort.clone(),
                levels: model.reasoning_effort_levels.clone().unwrap_or_default(),
                // What the *file* would start on. The screen overwrites this
                // with the session's own model, which is the one a person means
                // by "the one I am on" — see `ProvidersView::with_current`.
                current: default.as_deref() == Some(id.as_str()),
                managed: config.selection_is_codingplan_managed(id),
            })
        })
        .collect()
}

fn protocols() -> Vec<Protocol> {
    PROTOCOLS
        .iter()
        .filter_map(|id| provider_preset::preset(id))
        .map(|preset| Protocol {
            id: preset.id.to_string(),
            label: protocol_label(preset.provider_type),
            endpoint: preset.default_base_url.map(str::to_string),
            needs_key: !matches!(preset.auth_kind, provider_preset::AuthKind::None),
        })
        .collect()
}

/// The human name for a wire protocol. Exhaustive, so a new `ProviderType`
/// cannot silently be mislabelled as an old one.
fn protocol_label(ty: provider_preset::ProviderType) -> String {
    match ty {
        provider_preset::ProviderType::Anthropic => "Anthropic",
        provider_preset::ProviderType::Ollama => "Ollama",
        provider_preset::ProviderType::OpenAi => "OpenAI",
        provider_preset::ProviderType::Responses => "OpenAI Responses",
    }
    .to_string()
}

/// What a typed key field means for the file.
///
/// Empty is **keep**, not clear: the stored credential is not readable from the
/// panel, so there is nothing to prefill the field with — and a person who
/// opened a form to change an endpoint has not asked for their key to be thrown
/// away. Clearing one is what moving to a keyless protocol does, and that is
/// decided from the protocol rather than from an empty field.
fn written_key(typed: Option<&str>) -> KeyWrite<'_> {
    match typed.map(str::trim).filter(|key| !key.is_empty()) {
        Some(key) => KeyWrite::Set(key),
        None => KeyWrite::Keep,
    }
}

/// Whether a credential is stored for this account.
///
/// A bool, and only a bool: this is the whole of what the screen is told about a
/// key, so "there is one" can be drawn without the key ever leaving the file.
fn has_stored_key(account: &atomcode_config::config::provider::ProviderAccountConfig) -> bool {
    !account
        .api_key
        .as_deref()
        .unwrap_or_default()
        .trim()
        .is_empty()
}

/// What to call an account on screen.
///
/// Its own `display_name` first, then the id — never the preset's name for a
/// custom account, or two accounts a person made on one vendor would be drawn
/// with one label and become indistinguishable.
fn label_for(config: &Config, id: &str) -> String {
    if let Some(account) = config.provider_accounts.get(id) {
        if let Some(name) = account
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            return name.to_string();
        }
        if account.provider != id {
            return id.to_string();
        }
    }
    provider_preset::preset(id)
        .map(|preset| preset.display_name.to_string())
        .unwrap_or_else(|| id.to_string())
}

/// An id a TOML table can be keyed by, from whatever a person typed.
fn sanitize(name: &str) -> String {
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

impl Providers for ConfigProviders {
    fn rows(&self) -> ProvidersView {
        self.read()
    }

    fn add_account(&self, draft: &AccountDraft) -> Result<String, String> {
        let base = sanitize(&draft.name);
        if base.is_empty() {
            return Err("给它起个名字:字母、数字、`-`、`_`、`.`".into());
        }
        // Not into the gateway's namespace, or it would be taken for a managed
        // account: undeletable here, and never asked for a key.
        let base = match atomcode_config::config::is_codingplan_provider_name(&base) {
            true => format!("custom-{base}"),
            false => base,
        };
        let config = self.load();
        let id = free_id(&base, |candidate| {
            config.provider_accounts.contains_key(candidate)
                || config.providers.contains_key(candidate)
        });
        let preset = provider_preset::preset_or_compatible(&draft.protocol);
        if draft.endpoint.trim().is_empty() && preset.default_base_url.is_none() {
            return Err("这个协议没有默认地址,得填一个".into());
        }
        let endpoint = endpoint_override(&draft.endpoint, &draft.protocol);
        let key = written_key(draft.key.as_deref());
        let written = id.clone();
        self.write(move |document| {
            provider_edit::put_account(
                document,
                &written,
                &AccountPatch {
                    provider: preset.id,
                    base_url: endpoint,
                    api_key: key,
                    display_name: None,
                },
            )
        })?;
        Ok(id)
    }

    fn edit_account(&self, id: &str, draft: &AccountDraft) -> Result<(), String> {
        let config = self.load();
        if config.account_is_codingplan_managed(id) {
            return Err(format!("{id} 归登录管理,这儿改不了;用 /login"));
        }
        let wanted = provider_preset::preset_or_compatible(&draft.protocol);
        let legacy =
            !config.provider_accounts.contains_key(id) && config.providers.contains_key(id);
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
        // The panel offers four *protocols*; a stored account may name a vendor
        // preset (`deepseek`) that speaks one of them. Leaving the protocol
        // where it was must not rewrite `deepseek` into `openai-compatible`, so
        // the wire is compared rather than the id — the guard tuix keeps as
        // `vendor_changed`.
        let moved =
            provider_preset::preset_or_compatible(&stored).provider_type != wanted.provider_type;
        let key = written_key(draft.key.as_deref());
        let endpoint = draft.endpoint.trim();
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
        let endpoint = endpoint_override(&draft.endpoint, &provider).map(str::to_string);
        // A protocol with no credential of its own drops a key left over from
        // the one before it: a local Ollama carrying an OpenAI key is a file
        // holding a secret nothing will ever send.
        let clears = moved && matches!(wanted.auth_kind, provider_preset::AuthKind::None);
        let id = id.to_string();
        self.write(move |document| {
            provider_edit::put_account(
                document,
                &id,
                &AccountPatch {
                    provider: &provider,
                    base_url: endpoint.as_deref(),
                    api_key: if clears { KeyWrite::Clear } else { key },
                    display_name: None,
                },
            )
        })
    }

    fn delete_account(&self, id: &str) -> Result<(), String> {
        let config = self.load();
        if config.account_is_codingplan_managed(id) {
            return Err(format!("{id} 归登录管理,这儿删不了;用 /logout"));
        }
        if !config.provider_accounts.contains_key(id) && !config.providers.contains_key(id) {
            return Err(format!("配置里没有 {id}"));
        }
        // Its models go with it: a model profile pointing at an account that is
        // gone is a selection that cannot resolve, and leaving those behind
        // would be leaving the file in a state nothing can start from.
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

    fn add_model(&self, draft: &ModelDraft) -> Result<String, String> {
        let config = self.load();
        if config.account_is_codingplan_managed(&draft.account) {
            return Err(format!("{} 的模型归登录管理", draft.account));
        }
        let model = draft.model.trim().to_string();
        if model.is_empty() {
            return Err("模型名不能是空的".into());
        }
        // An offer picked off the account list has no account in the file yet.
        // Adding a model to it is what configures it, which is the one gesture
        // that turns "this build knows about deepseek" into "you have one".
        let fresh = !config.provider_accounts.contains_key(&draft.account)
            && !config.providers.contains_key(&draft.account);
        let preset_id = config
            .logical_accounts()
            .get(&draft.account)
            .map(|account| account.provider.clone())
            .unwrap_or_else(|| draft.account.clone());
        let preset = provider_preset::preset_or_compatible(&preset_id);
        let window = draft.window.unwrap_or_else(|| {
            atomcode_config::config::provider::default_context_window_for(
                preset.provider_type.wire(),
            )
        });
        let base = format!("{}/{model}", draft.account);
        let id = free_id(&base, |candidate| {
            config.models.contains_key(candidate) || config.providers.contains_key(candidate)
        });
        let key = written_key(draft.key.as_deref());
        let account = draft.account.clone();
        let levels = draft.levels.clone();
        let effort = draft.effort.clone();
        let vision = draft.vision;
        let default = draft.default;
        let written = id.clone();
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
                        display_name: None,
                    },
                )?;
            } else if matches!(key, KeyWrite::Set(_)) {
                provider_edit::put_account(
                    document,
                    &account,
                    &AccountPatch {
                        provider: &provider_of(document, &account, provider),
                        base_url: None,
                        api_key: key,
                        display_name: None,
                    },
                )?;
            }
            provider_edit::put_model(
                document,
                &written,
                &ModelPatch {
                    account: &account,
                    model: &model,
                    context_window: window,
                    supports_vision: vision,
                    reasoning_effort: effort.as_deref(),
                    reasoning_effort_levels: levels.as_deref(),
                },
            )?;
            if default {
                provider_edit::set_default_model(document, Some(&written));
            }
            Ok(())
        })?;
        Ok(id)
    }

    fn edit_model(&self, id: &str, draft: &ModelDraft) -> Result<(), String> {
        let config = self.load();
        if config.selection_is_codingplan_managed(id) {
            return Err(format!("{id} 归登录管理,这儿改不了"));
        }
        let model = draft.model.trim().to_string();
        if model.is_empty() {
            return Err("模型名不能是空的".into());
        }
        let legacy = !config.models.contains_key(id) && config.providers.contains_key(id);
        if !legacy && !config.models.contains_key(id) {
            return Err(format!("配置里没有 {id}"));
        }
        let window = draft.window.unwrap_or_else(|| {
            config
                .logical_models()
                .get(id)
                .map(|m| m.context_window)
                .unwrap_or(128_000)
        });
        let id = id.to_string();
        let account = draft.account.clone();
        let levels = draft.levels.clone();
        let effort = draft.effort.clone();
        let vision = draft.vision;
        let default = draft.default;
        self.write(move |document| {
            let patch = ModelPatch {
                account: &account,
                model: &model,
                context_window: window,
                supports_vision: vision,
                reasoning_effort: effort.as_deref(),
                reasoning_effort_levels: levels.as_deref(),
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
    }

    fn delete_model(&self, id: &str) -> Result<(), String> {
        let config = self.load();
        if config.selection_is_codingplan_managed(id) {
            return Err(format!("{id} 归登录管理,这儿删不了"));
        }
        if !config.models.contains_key(id) && !config.providers.contains_key(id) {
            return Err(format!("配置里没有 {id}"));
        }
        let gone = id.to_string();
        self.write(move |document| {
            provider_edit::remove_model(document, &gone);
            provider_edit::remove_legacy_provider(document, &gone);
            Ok(())
        })?;
        self.clear_dangling_default()
    }
}

impl ConfigProviders {
    /// Take a default that no longer resolves out of the file.
    ///
    /// Read after the delete rather than predicted before it: whether a
    /// selection still resolves is a question about the configuration as a
    /// whole, and the honest way to answer it is to ask the configuration that
    /// is now on disk. A default left pointing at something deleted is a session
    /// that will not start.
    fn clear_dangling_default(&self) -> Result<(), String> {
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
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A file in the shape a real one is in: one legacy flat entry, one
    /// new-schema account with two models under it, and a default.
    const MIXED: &str = r#"# hand written
default_model = "mine/a"

[providers.deepseek]
type = "openai"
model = "deepseek-chat"
api_key = "sk-legacy"
base_url = "https://api.deepseek.com/v1"

[provider_accounts.mine]
provider = "openai-compatible"
base_url = "https://one.example.com/v1"
api_key = "sk-one"

[models."mine/a"]
account = "mine"
model = "a"
context_window = 128000

[models."mine/b"]
account = "mine"
model = "b"
context_window = 64000
"#;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atomcode-providers-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("scratch");
        let path = dir.join("config.toml");
        std::fs::write(&path, MIXED).expect("fixture");
        path
    }

    fn port(name: &str) -> (Arc<ConfigProviders>, PathBuf) {
        let path = scratch(name);
        (ConfigProviders::new(path.clone()), path)
    }

    fn text(path: &PathBuf) -> String {
        std::fs::read_to_string(path).expect("the file is there")
    }

    /// The bug this module was written to end: `/provider` used to read
    /// `config.providers` — the legacy flat table — so an account in the new
    /// schema was invisible to it. Point this at `config.providers` again and
    /// `mine` disappears while its two models stay on the other list.
    #[test]
    fn an_account_in_the_new_schema_is_listed() {
        let (port, _path) = port("new-schema");
        let view = port.rows();
        let ids: Vec<&str> = view
            .accounts()
            .iter()
            .filter(|a| a.configured)
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(ids, vec!["mine"], "the account, not the flat entry");
        let models: Vec<&str> = view.models().iter().map(|m| m.id.as_str()).collect();
        assert!(
            models.contains(&"mine/a") && models.contains(&"mine/b"),
            "its models are listed: {models:?}"
        );
        // And the legacy flat entry is still reachable — as what it is, one
        // selectable model rather than an account with models under it.
        assert!(models.contains(&"deepseek"), "{models:?}");
    }

    /// Nothing that crosses the seam carries a key, however the row is printed.
    #[test]
    fn a_credential_never_leaves_the_port() {
        let (port, _path) = port("no-credential");
        let view = port.rows();
        let drawn = format!("{:?}{:?}", view.accounts(), view.models());
        assert!(!drawn.contains("sk-one"), "{drawn}");
        assert!(!drawn.contains("sk-legacy"), "{drawn}");
        let mine = view.account("mine").expect("the account is listed");
        assert!(mine.has_key, "that it has one is all that crosses");
    }

    #[test]
    fn what_a_person_could_set_up_is_offered_beside_what_they_have() {
        let (port, _path) = port("offers");
        let view = port.rows();
        let offers: Vec<&str> = view
            .accounts()
            .iter()
            .filter(|a| !a.configured)
            .map(|a| a.id.as_str())
            .collect();
        assert!(!offers.is_empty(), "there are vendors to offer");
        assert!(
            !offers.contains(&"openai-compatible"),
            "the custom endpoints are reached through add, not offered: {offers:?}"
        );
    }

    #[test]
    fn adding_an_account_writes_it_and_leaves_the_file_alone() {
        let (port, path) = port("add");
        let id = port
            .add_account(&AccountDraft {
                name: "My Vendor!".into(),
                protocol: "openai-compatible".into(),
                endpoint: "https://two.example.com/v1".into(),
                key: Some("sk-two".into()),
            })
            .expect("it writes");
        assert_eq!(id, "My-Vendor", "an id a TOML table can be keyed by");
        let out = text(&path);
        assert!(out.starts_with("# hand written"), "{out}");
        assert!(out.contains("[provider_accounts.My-Vendor]"), "{out}");
        assert!(out.contains(r#"api_key = "sk-two""#), "{out}");
        assert!(
            out.contains(r#"api_key = "sk-one""#),
            "and the old one: {out}"
        );
    }

    #[test]
    fn an_endpoint_this_protocol_has_no_default_for_is_required() {
        let (port, _path) = port("endpoint");
        let refused = port.add_account(&AccountDraft {
            name: "x".into(),
            protocol: "openai-compatible".into(),
            endpoint: "  ".into(),
            key: None,
        });
        assert!(refused.is_err(), "{refused:?}");
    }

    #[test]
    fn a_name_that_sanitises_to_nothing_is_refused_rather_than_written() {
        let (port, _path) = port("empty-name");
        assert!(port
            .add_account(&AccountDraft {
                name: "！！！".into(),
                protocol: "ollama".into(),
                endpoint: String::new(),
                key: None,
            })
            .is_err());
    }

    #[test]
    fn a_second_account_by_the_same_name_gets_its_own_id() {
        let (port, _path) = port("collide");
        let draft = AccountDraft {
            name: "mine".into(),
            protocol: "openai-compatible".into(),
            endpoint: "https://three.example.com/v1".into(),
            key: None,
        };
        assert_eq!(port.add_account(&draft).unwrap(), "mine-2");
    }

    #[test]
    fn an_edit_with_no_key_typed_keeps_the_stored_one() {
        let (port, path) = port("keep-key");
        port.edit_account(
            "mine",
            &AccountDraft {
                name: String::new(),
                protocol: "openai-compatible".into(),
                endpoint: "https://moved.example.com/v1".into(),
                key: None,
            },
        )
        .expect("it writes");
        let out = text(&path);
        assert!(out.contains("https://moved.example.com/v1"), "{out}");
        assert!(out.contains(r#"api_key = "sk-one""#), "{out}");
    }

    /// The guard tuix keeps as `vendor_changed`: a person who opens a
    /// `deepseek` entry, changes the endpoint and saves must not find their
    /// vendor rewritten to the generic protocol the toggle happened to be on.
    #[test]
    fn leaving_the_protocol_alone_does_not_rewrite_the_vendor() {
        let (port, path) = port("vendor");
        port.edit_account(
            "deepseek",
            &AccountDraft {
                name: String::new(),
                // The protocol `deepseek` already speaks.
                protocol: "openai-compatible".into(),
                endpoint: "https://api.deepseek.com/v2".into(),
                key: None,
            },
        )
        .expect("it writes");
        let out = text(&path);
        assert!(out.contains(r#"type = "openai""#), "the wire stays: {out}");
        assert!(out.contains("https://api.deepseek.com/v2"), "{out}");
    }

    /// Moving an account to a protocol that has no credential of its own takes
    /// the old key **out** of the file. An `api_key = ""` left behind is not
    /// "no credential": it is one whose value is empty, which is what would go
    /// out on the wire.
    #[test]
    fn moving_to_a_keyless_protocol_takes_the_credential_out() {
        let (port, path) = port("keyless");
        port.edit_account(
            "mine",
            &AccountDraft {
                name: String::new(),
                protocol: "ollama".into(),
                endpoint: "http://localhost:11434".into(),
                key: None,
            },
        )
        .expect("it writes");
        let out = text(&path);
        assert!(out.contains(r#"provider = "ollama""#), "{out}");
        assert!(
            !out.contains(r#"api_key = "sk-one""#) && !out.contains(r#"api_key = """#),
            "gone, not blanked: {out}"
        );
        // And the other account's is untouched.
        assert!(out.contains(r#"api_key = "sk-legacy""#), "{out}");
    }

    #[test]
    fn deleting_an_account_takes_its_models_and_the_default_it_left_dangling() {
        let (port, path) = port("delete");
        port.delete_account("mine").expect("it writes");
        let out = text(&path);
        assert!(!out.contains("[provider_accounts.mine]"), "{out}");
        assert!(!out.contains(r#"[models."mine/a"]"#), "{out}");
        assert!(!out.contains(r#"[models."mine/b"]"#), "{out}");
        assert!(
            !out.contains("default_model"),
            "a default pointing at what is gone is a session that will not start: {out}"
        );
        assert!(
            out.contains("[providers.deepseek]"),
            "and nothing else: {out}"
        );
    }

    #[test]
    fn deleting_a_model_leaves_its_account_standing() {
        let (port, path) = port("delete-model");
        port.delete_model("mine/b").expect("it writes");
        let out = text(&path);
        assert!(!out.contains(r#"[models."mine/b"]"#), "{out}");
        assert!(out.contains(r#"[models."mine/a"]"#), "{out}");
        assert!(out.contains("[provider_accounts.mine]"), "{out}");
        assert!(
            out.contains(r#"default_model = "mine/a""#),
            "still resolves: {out}"
        );
    }

    #[test]
    fn adding_a_model_names_it_after_its_account_and_can_take_the_session_with_it() {
        let (port, path) = port("add-model");
        let id = port
            .add_model(&ModelDraft {
                account: "mine".into(),
                model: "c".into(),
                window: Some(32_000),
                vision: Some(true),
                effort: Some("high".into()),
                levels: Some(vec!["low".into(), "high".into()]),
                default: true,
                key: None,
            })
            .expect("it writes");
        assert_eq!(id, "mine/c");
        let out = text(&path);
        assert!(out.contains(r#"[models."mine/c"]"#), "{out}");
        assert!(out.contains("context_window = 32000"), "{out}");
        assert!(out.contains("supports_vision = true"), "{out}");
        assert!(
            out.contains(r#"reasoning_effort_levels = ["low", "high"]"#),
            "{out}"
        );
        assert!(out.contains(r#"default_model = "mine/c""#), "{out}");
    }

    /// Picking a vendor off the offers and giving it a model is what configures
    /// it: the account is written on the way past, with the preset's endpoint.
    #[test]
    fn adding_a_model_to_an_offer_configures_its_account_too() {
        let (port, path) = port("materialise");
        let offer = port
            .rows()
            .accounts()
            .iter()
            .find(|a| !a.configured)
            .map(|a| a.id.clone())
            .expect("there is an offer");
        port.add_model(&ModelDraft {
            account: offer.clone(),
            model: "some-model".into(),
            window: None,
            vision: None,
            effort: None,
            levels: None,
            default: false,
            key: Some("sk-offer".into()),
        })
        .expect("it writes");
        let out = text(&path);
        assert!(
            out.contains(&format!("[provider_accounts.{offer}]")),
            "{out}"
        );
        assert!(out.contains(r#"api_key = "sk-offer""#), "{out}");
        assert!(
            out.contains("context_window"),
            "a window was decided: {out}"
        );
    }

    /// What the first real run of the panel lost: saving a model with "use it
    /// now" ticked rewrites `default_model`, and rewriting a key that is already
    /// there used to take the comment above it with it.
    #[test]
    fn taking_the_session_to_a_new_model_keeps_the_comment_above_the_default() {
        let (port, path) = port("comment");
        port.add_model(&ModelDraft {
            account: "mine".into(),
            model: "c".into(),
            window: None,
            vision: None,
            effort: None,
            levels: None,
            default: true,
            key: None,
        })
        .expect("it writes");
        let out = text(&path);
        assert!(out.starts_with("# hand written"), "{out}");
        assert!(out.contains(r#"default_model = "mine/c""#), "{out}");
    }

    #[test]
    fn a_legacy_entry_is_edited_as_the_one_thing_it_is() {
        let (port, path) = port("legacy-model");
        port.edit_model(
            "deepseek",
            &ModelDraft {
                account: "deepseek".into(),
                model: "deepseek-reasoner".into(),
                window: Some(96_000),
                vision: None,
                effort: None,
                levels: None,
                default: false,
                key: None,
            },
        )
        .expect("it writes");
        let out = text(&path);
        assert!(out.contains(r#"model = "deepseek-reasoner""#), "{out}");
        assert!(out.contains("context_window = 96000"), "{out}");
        assert!(
            out.contains(r#"api_key = "sk-legacy""#),
            "its credential is untouched: {out}"
        );
        assert!(
            !out.contains(r#"[models."deepseek"]"#),
            "and it is not silently migrated into the new schema: {out}"
        );
    }

    #[test]
    fn a_model_that_is_not_there_is_refused_rather_than_created() {
        let (port, _path) = port("missing");
        assert!(port
            .edit_model(
                "nope/x",
                &ModelDraft {
                    account: "mine".into(),
                    model: "x".into(),
                    window: None,
                    vision: None,
                    effort: None,
                    levels: None,
                    default: false,
                    key: None,
                },
            )
            .is_err());
        assert!(port.delete_model("nope/x").is_err());
        assert!(port.delete_account("nope").is_err());
    }
}
