//! Provider accounts found in another agent's configuration. Filled in by Task 7.

use std::path::Path;

use super::report::AssetScope;

/// How a credential is referenced.
///
/// `Debug` is written by hand: a derived one would print a literal key in
/// every `{:?}`, `dbg!` and `unwrap()` panic message.
#[derive(Clone, PartialEq, Eq)]
pub enum ApiKeyRef {
    /// The NAME of an environment variable, e.g. `"MY_KEY"`. The secret stays
    /// where it is and is resolved at run time.
    EnvVar(String),
    /// A literal secret found in the source configuration. Written only after
    /// the person confirms that one item.
    Plaintext(String),
}

impl ApiKeyRef {
    /// How the credential is written, for the report to say. The VALUE is
    /// never part of this — only its kind.
    pub fn kind(&self) -> &'static str {
        match self {
            ApiKeyRef::EnvVar(_) => "environment variable",
            ApiKeyRef::Plaintext(_) => "a literal key",
        }
    }
}

impl std::fmt::Debug for ApiKeyRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The variable's NAME, not a secret.
            ApiKeyRef::EnvVar(name) => f.debug_tuple("EnvVar").field(name).finish(),
            // The value IS the secret: only its kind is shown.
            ApiKeyRef::Plaintext(_) => f.debug_tuple("Plaintext").field(&"<redacted>").finish(),
        }
    }
}

/// A provider account an external agent has configured.
///
/// The derived `Debug` is safe: `api_key` goes through `ApiKeyRef`'s own
/// redacting `Debug`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAccountDraft {
    /// **The id the account WILL get** — already normalised by
    /// `account_id_for`, the same rule the writer uses. It is not the raw
    /// source id: `create_account` sanitises the name and may prefix it, so
    /// a draft built on the raw name would never be found again.
    pub id: String,
    /// The definition's name **as the source wrote it**.
    ///
    /// Two definitions in one file can normalise to the same id (`"foo bar"`
    /// and `"foo-bar"` both become `foo-bar`), so the id alone cannot say
    /// which definition a row is about. The original name can — and it is
    /// what the person recognises, so the report shows it.
    pub source_name: String,
    /// A preset or protocol id (`openai-compatible`, `anthropic-compatible`,
    /// `openai-responses`).
    pub protocol: String,
    /// The endpoint as it will be **stored** — normalised by
    /// `effective_endpoint`, so it is `None` when the endpoint is the
    /// protocol's own default (the writer stores nothing in that case).
    /// The endpoint in USE is `endpoint_in_use(protocol, base_url)`.
    pub base_url: Option<String>,
    /// The credential, when the source named one.
    pub api_key: Option<ApiKeyRef>,
    /// The models the source was using.
    pub models: Vec<String>,
    /// Where this account was configured — the file it was read from. The
    /// report shows it, so a person can see WHICH file to look at.
    pub source_path: std::path::PathBuf,
    /// Whether the account came from the person's own tree or from the
    /// current project. A project provider is not the user's provider.
    pub scope: AssetScope,
    /// Why this account CANNOT be imported, when that is the case.
    ///
    /// `None` means it can. A `Some` reason means the account is still
    /// **reported** — it is configured, the person should see it — but the
    /// importer refuses it.
    ///
    /// Two things land here:
    /// - the name cannot become an account id at all (`account_id_for`
    ///   returns `None`), so there is nothing to write it under;
    /// - the endpoint speaks a wire protocol we do not support.
    ///
    /// Both used to be either silently dropped or reported as importable,
    /// which is the one thing this report must never do.
    pub blocked: Option<String>,
}

/// The protocol an external agent's endpoint speaks, mapped to ours.
///
/// **The SOURCE says its own default**, so this function does not ask who is
/// asking: a new source passes a different default, and the shared module
/// stays unchanged. That is what makes adding a source cheap.
///
/// **A missing `wire_api` means Responses, not chat/completions.** Codex's own
/// `WireApi` has `#[default] Responses`
/// (`codex-rs/model-provider-info/src/lib.rs:117`), so a config that names a
/// provider but no protocol speaks Responses. Mapping that to
/// `openai-compatible` (chat/completions) would send the request to an
/// endpoint that does not speak it.
///
/// A protocol we do not know is **not** guessed at — it becomes
/// `unsupported:…` and the report says so, instead of the import failing
/// later against an endpoint that speaks something else.
pub fn protocol_for_wire(wire_api: Option<&str>, default: &str) -> String {
    match wire_api {
        Some("responses") => "openai-responses".into(),
        Some("anthropic") => "anthropic-compatible".into(),
        Some("chat") | Some("openai") => "openai-compatible".into(),
        Some(other) => unsupported_protocol(other),
        None => default.into(),
    }
}

/// The protocol id for a wire family we do not support.
///
/// The report says the endpoint speaks something we cannot, so the person
/// sees it instead of the import silently failing later.
pub fn unsupported_protocol(wire_api: &str) -> String {
    format!("unsupported:{wire_api}")
}

/// The id the importer will give this account.
///
/// **The writer normalizes the name**, so the report must use the SAME id:
/// `ProviderBook::create_account` runs the name through `base_id`
/// (`provider_book.rs:700`), which sanitises it and prefixes `custom-` when
/// it belongs to CodingPlan. A report built on the raw source id would never
/// match the account on the next run — the re-check would keep saying
/// "can be brought over" about an account that is already there.
///
/// `base_id` is exposed for this (`provider_book::account_id_for`), so there
/// is **one** rule, not two that can drift.
pub fn account_id_for(name: &str) -> Option<String> {
    match atomcode_config::provider_book::account_id_for(name) {
        Ok(id) => Some(id),
        // An unusable name cannot become an account at all.
        Err(_) => None,
    }
}

/// The endpoint as it will be **stored**.
///
/// `endpoint_override` (`provider_book.rs:785`) trims the endpoint and stores
/// NOTHING when it equals the protocol's own default — so a file does not pin
/// a URL that should follow the build. Comparing the raw source endpoint
/// against the stored one would never match: the account ends up with
/// `base_url = None` while the draft still says the default URL.
///
/// `endpoint_override` is exposed for this
/// (`provider_book::effective_endpoint`).
///
/// **This delegates to the writer's rule** rather than re-implementing it. A
/// copy would be correct today and wrong the day the writer changes — and the
/// symptom would be a re-check that never matches.
pub fn effective_endpoint(protocol: &str, endpoint: Option<&str>) -> Option<String> {
    atomcode_config::provider_book::effective_endpoint(protocol, endpoint.unwrap_or_default())
}

/// Whether this protocol REQUIRES an endpoint.
///
/// `ProviderBook::create_account` refuses a custom-compatible protocol with no
/// endpoint (`provider_book.rs:198`: `ProtocolNeedsEndpoint`) — only a preset
/// that has its own default can do without one.
///
/// The report has to know this **before** the write, or it would advertise an
/// account as importable and then fail to write it.
pub fn protocol_needs_endpoint(protocol: &str) -> bool {
    atomcode_config::config::provider_preset::preset(protocol)
        .map(|preset| preset.default_base_url.is_none())
        .unwrap_or(true)
}

/// The endpoint the account will actually talk to.
///
/// The stored form may be `None` — that means "follow the protocol's
/// default" — but the person still needs to see WHICH endpoint that is.
pub fn endpoint_in_use(protocol: &str, endpoint: Option<&str>) -> Option<String> {
    if let Some(stored) = effective_endpoint(protocol, endpoint) {
        return Some(stored);
    }
    atomcode_config::config::provider_preset::preset(protocol)
        .and_then(|preset| preset.default_base_url)
        .map(str::to_string)
}

/// Read a JSON file that may carry comments.
///
/// **Comments are stripped first**, the way our own MCP loader does
/// (`mcp/config.rs:624`: `serde_json::from_str(&crate::jsonc::strip_comments(content))`).
/// A hand-edited file with a comment in it is a perfectly good file; calling
/// it broken would be wrong.
///
/// `None` when the file cannot be read or parsed — the caller reports that
/// as blocked, and does NOT treat it as "there is nothing here".
pub fn read_jsonc(path: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&crate::jsonc::strip_comments(&raw)).ok()
}
