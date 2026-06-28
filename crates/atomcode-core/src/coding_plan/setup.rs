// crates/atomcode-core/src/coding_plan/setup.rs
//
// Orchestrator for the 4-step CodingPlan flow. Single `run` entrypoint
// shared by the CLI subcommand and the TUI slash command; both render
// the returned `SetupReport` their own way (stdout vs. body scrollback).
//
// Failure policy (matches product spec D5):
//
//   Step 1 Login  — if not logged in and OAuth fails → bail out (nothing
//                   downstream works without a token).
//   Step 2 Claim  — `duplicate=true` means "already claimed / in review"
//                   — report it as a skip, NOT an error, and continue.
//                   Transport/5xx errors → bail (server is in a bad state).
//   Step 3 Models — empty list or request failure → bail. The whole point
//                   of the flow is setting up providers; without models
//                   we have nothing to install.
//   Step 4 Status — warn-only. The plan is already set up; a failed
//                   status fetch just means we can't show the quota
//                   widget. User can retry with `/codingplan` later.
//
// Provider mutation (D2 + D4):
//
//   - All previously-created `AtomGit*` entries are wiped before inserts.
//     Since CodingPlan is the authoritative source of truth for the
//     model list, keeping stale names around would confuse `/model`.
//   - Single model → one provider named `AtomGit`.
//   - Multiple models → one provider per model, named
//     `AtomGit-{display_model_name}` with `/` → `-` (keeps config.toml
//     section names clean — `[providers.AtomGit-moonshotai-Kimi-K2]`).
//   - `default_provider` is set to the first model in the API order.

use anyhow::Result;
use std::sync::Arc;

use super::client::{is_auth_expired, Client};
use super::types::{ModelEntry, PlanType, StatusResponse};
#[cfg(test)]
use super::types::RateLimitWindow;
use crate::auth;
use crate::config::provider::ProviderConfig;
use crate::config::Config;

/// Default LLM gateway base URL for CodingPlan-managed providers when
/// the `models-v2` payload doesn't carry a per-model `base_url`. Used
/// only inside [`codingplan_llm_base_url`] — call that, not this.
///
/// The signed gateway. `coding_plan::crypto::is_atomgit_gateway` matches
/// `llm-api.atomgit.com`, `pre-llm-api-cce.atomgit.com`, and the legacy
/// `api-ai.gitcode.com` host (see the host whitelist at `crypto.rs`), so
/// codingplan request signing engages for all three.
const DEFAULT_CODINGPLAN_LLM_BASE_URL: &str = "https://llm-api.atomgit.com/v1";

/// Resolve the LLM gateway base URL for CodingPlan-managed providers.
///
/// Read order:
///   1. `ATOMCODE_CODINGPLAN_LLM_BASE_URL` env var (trimmed, trailing
///      `/` stripped, empty value treated as unset). Set this when
///      pointing the client at a staging gateway.
///   2. [`DEFAULT_CODINGPLAN_LLM_BASE_URL`].
///
/// Cached once at first call via `OnceLock` — same shape as
/// [`auth::oauth::platform_base_url`] — so every provider registered
/// by `step_models_and_register` lands on the same host even if the
/// env var changes mid-flight, and the per-provider build cost is
/// one atomic read after the first call.
///
/// Returns `String` rather than `&'static str` because the cached
/// value's lifetime is tied to the `OnceLock`; callers that need an
/// owned URL (e.g. `ProviderConfig::base_url: Option<String>`) get
/// one without an extra clone.
fn codingplan_llm_base_url() -> String {
    use std::sync::OnceLock;
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        std::env::var("ATOMCODE_CODINGPLAN_LLM_BASE_URL")
            .ok()
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_CODINGPLAN_LLM_BASE_URL.to_string())
    })
    .clone()
}

/// Provider type for the AtomGit LLM gateway (it's OpenAI-compatible).
const PROVIDER_TYPE: &str = "openai";

/// Context window for each coding-plan provider. The models endpoint
/// doesn't currently return a per-model window, so we apply the same
/// 64k value that the legacy `/login` flow hard-coded.
const CONTEXT_WINDOW: usize = 64_000;

/// Prefix used for every coding-plan-managed provider name.
const PROVIDER_PREFIX: &str = "AtomGit";

/// Result of one orchestrator step. Distinct from `Result` because
/// "already done / idempotent skip" is a first-class outcome, not an
/// error — the report needs to tell the user "you already claimed this
/// last week" in the same place it'd tell them "just claimed".
#[derive(Debug, Clone)]
pub enum StepResult<T> {
    /// Step ran and completed with the carried payload.
    Ok(T),
    /// Step was idempotent-skipped (already logged in, already claimed).
    /// The string is a human-readable reason for display.
    Skipped(String),
    /// Step failed. The string is a human-readable error.
    Err(String),
}

impl<T> StepResult<T> {
    pub fn is_err(&self) -> bool {
        matches!(self, StepResult::Err(_))
    }
    pub fn is_ok_or_skipped(&self) -> bool {
        !self.is_err()
    }
}

/// Describes how the auto-detected vision_preprocessor_provider was
/// (or was not) updated by `step_models_and_register`. Surfaces in
/// `SetupReport::render` so the user can see what happened to that
/// config knob across the /codingplan flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisionPreprocessorOutcome {
    /// Field was None and remains None (no VL/OCR in list).
    UnchangedNone,
    /// Field was a non-AtomGit user-supplied value; preserved.
    /// Carries the value for display.
    UserSupplied(String),
    /// Field was None or a stale AtomGit-* key; auto-pointed at a
    /// vision-capable provider in the freshly-installed list.
    /// Carries the new key.
    AutoSet(String),
    /// Field was an AtomGit-* key but the new list has no VL/OCR
    /// candidate, so the field was cleared to None to avoid pointing
    /// at a wiped provider key.
    Cleared,
}

impl SetupReport {
    /// Render as a multi-line plain-text block for stdout / TUI body.
    /// Shared by the CLI subcommand and the `/codingplan` slash command
    /// so the visual contract stays consistent.
    pub fn render(&self) -> String {
        use crate::i18n::{t, Msg};

        let mut out = String::new();
        out.push_str(&t(Msg::CpSetupHeader));

        // Step 1: login
        match &self.login {
            StepResult::Ok(info) => {
                let who = info.display_name.as_deref().unwrap_or(&info.username);
                let email = info.email.as_deref().unwrap_or("—");
                out.push_str(&t(Msg::CpLoggedIn {
                    who,
                    username: &info.username,
                    email,
                }));
            }
            StepResult::Skipped(reason) => {
                out.push_str(&t(Msg::CpStepSkipped { reason }));
            }
            StepResult::Err(msg) => {
                out.push_str(&t(Msg::CpLoginFailed { error: msg }));
            }
        }

        // Step 2: claim. When `claim_attempts` is populated (production
        // path), emit one row per *successful* tier — refused / errored
        // intermediates (e.g. "Max 套餐尚未开放") are suppressed so the
        // happy-path output collapses to a single "Pro 生效" line. If
        // no tier succeeded, fall through to a single failure row using
        // `self.claim` so the user still gets a signal. When empty
        // (login-cascade suppression OR legacy test fixtures), fall
        // back to the legacy single-row path.
        if !self.claim_attempts.is_empty() {
            let mut any_success = false;
            for attempt in &self.claim_attempts {
                let tier = attempt.tier.as_str();
                // Server's `plan_name` already carries the "CodingPlan "
                // prefix and reflects the user's real entitlement; prefer
                // it over the requested cascade tier. Empty (legacy
                // gateway) → fall back to "CodingPlan {tier}".
                let plan_label = |plan_name: &str| -> String {
                    if plan_name.is_empty() {
                        format!("CodingPlan {}", tier)
                    } else {
                        plan_name.to_string()
                    }
                };
                match &attempt.outcome {
                    TierOutcome::Claimed { plan_name, .. } => {
                        let plan = plan_label(plan_name);
                        out.push_str(&t(Msg::CpClaimTierSucceeded { plan: &plan }));
                        any_success = true;
                    }
                    TierOutcome::AlreadyHeld { plan_name, .. } => {
                        let plan = plan_label(plan_name);
                        out.push_str(&t(Msg::CpClaimTierAlreadyHeld { plan: &plan }));
                        any_success = true;
                    }
                    TierOutcome::Refused { .. } | TierOutcome::Errored { .. } => {
                        // Silently skipped — failures in the
                        // tier cascade are noise once any other tier
                        // succeeds, and even an all-fail run gets one
                        // consolidated row below instead of three.
                    }
                }
            }
            if !any_success {
                if let StepResult::Err(msg) = &self.claim {
                    // Pick the freshest non-empty source for the body:
                    // overall Err first, then walk attempts in reverse
                    // so the last tier's server message wins. Avoids a
                    // dangling `— ` when the overall Err was scrubbed
                    // empty by the cascade builder.
                    let body = if !msg.is_empty() {
                        msg.clone()
                    } else {
                        self.claim_attempts
                            .iter()
                            .rev()
                            .find_map(|a| match &a.outcome {
                                TierOutcome::Refused { message } if !message.is_empty() => {
                                    Some(format!("{}: {}", a.tier.as_str(), message))
                                }
                                TierOutcome::Errored { error } if !error.is_empty() => {
                                    Some(format!("{}: {}", a.tier.as_str(), error))
                                }
                                _ => None,
                            })
                            .unwrap_or_default()
                    };
                    if body.is_empty() {
                        // Truly nothing to say — render the prefix
                        // without a trailing em-dash body. Edge case:
                        // every tier responded success=false with no
                        // message AND no error text.
                        out.push_str(&t(Msg::CpClaimFailedBare));
                    } else {
                        out.push_str(&t(Msg::CpClaimFailed {
                            error: &truncate_inline(&body, 150),
                        }));
                    }
                }
            }
        } else {
            match &self.claim {
                StepResult::Ok(info) => {
                    let fallback = t(Msg::CpClaimSuccessFallback);
                    let message = if info.message.is_empty() {
                        fallback.as_ref()
                    } else {
                        info.message.as_str()
                    };
                    out.push_str(&t(Msg::CpClaimed {
                        message,
                        plan_type: info.plan_type.as_str(),
                    }));
                }
                StepResult::Skipped(reason) if reason == CASCADE_FROM_UPSTREAM_FAIL => {
                    // Cascade from login failure — suppressed.
                }
                StepResult::Skipped(reason) => {
                    out.push_str(&t(Msg::CpAlreadyClaimed { reason }));
                }
                StepResult::Err(msg) => {
                    out.push_str(&t(Msg::CpClaimFailed { error: msg }));
                }
            }
        }

        // Step 3: models. When the cascade marker is present (claim
        // failed upstream), skip the row entirely — printing
        // "Models step skipped — claim failed" right after the claim
        // failure line is just noise. Same for the status row below.
        match &self.models {
            StepResult::Ok(info) => {
                let count = info.provider_names.len();
                let plural_s = if count == 1 { "" } else { "s" };
                out.push_str(&t(Msg::CpAddedProviders { count, plural_s }));
                // Build a quick lookup of which display names made it
                // into the registered provider list — anything in
                // `all_models` but NOT in this set is locked behind
                // the user's plan tier.
                let registered: std::collections::HashSet<&str> =
                    info.display_names.iter().map(|s| s.as_str()).collect();
                // Locked models render FIRST so the upgrade prompt is the
                // first thing the eye lands on under "Added N providers:".
                // Visual cue is an `×` prefix matching the existing
                // failure rows (`× CodingPlan Max claim failed — …`)
                // plus the explicit `(requires Pro plan or higher)` suffix —
                // both plain text, so every renderer (alt-screen /
                // retained / plain) and every terminal font carries
                // the meaning. An earlier U+0336 combining strikethrough
                // pass was dropped after a user report that fonts in
                // the wild silently skip the overlay glyph; the SGR 9
                // approach before that was eaten by the TUI's CSI
                // sanitizer (`tuix::sanitize::scrub_controls`). The
                // prefix-plus-suffix combo doesn't depend on either.
                let locked: Vec<&ModelEntry> = info
                    .all_models
                    .iter()
                    .filter(|m| !m.plan_available && !registered.contains(m.display_model_name.as_str()))
                    .collect();
                for m in &locked {
                    out.push_str(&t(Msg::CpLocked {
                        name: &m.display_model_name,
                    }));
                }
                let default_suffix_cow = t(Msg::CpDefaultSuffix);
                for (pname, model) in info.provider_names.iter().zip(info.display_names.iter()) {
                    let suffix = if pname == &info.default_provider {
                        default_suffix_cow.as_ref()
                    } else {
                        ""
                    };
                    out.push_str(&t(Msg::CpProviderRow {
                        provider: pname,
                        model,
                        default_suffix: suffix,
                    }));
                }
                // Vision-preprocessor outcome line.
                match &info.vision_preprocessor {
                    VisionPreprocessorOutcome::AutoSet(k) => {
                        out.push_str(&t(Msg::CpVisionAuto { kind: k }));
                    }
                    VisionPreprocessorOutcome::UserSupplied(k) => {
                        out.push_str(&t(Msg::CpVisionUserSupplied { kind: k }));
                    }
                    VisionPreprocessorOutcome::Cleared => {
                        out.push_str(&t(Msg::CpVisionCleared));
                    }
                    VisionPreprocessorOutcome::UnchangedNone => {
                        // No-op: nothing to say when both the previous and
                        // new state are "no preprocessor configured".
                    }
                }
            }
            StepResult::Skipped(reason) if reason == CASCADE_FROM_UPSTREAM_FAIL => {
                // Suppress — claim failure line above is the explanation.
            }
            StepResult::Skipped(reason) => {
                out.push_str(&t(Msg::CpModelsSkipped { reason }));
            }
            StepResult::Err(msg) => {
                out.push_str(&t(Msg::CpModelsFailed { error: msg }));
            }
        }

        // Step 4: status
        match &self.status {
            StepResult::Ok(s) => {
                out.push_str(&t(Msg::CpStatusHeader));
                if let Some(plan) = &s.codingplan_free {
                    if plan.expires_at.is_empty() {
                        // Backend sends null claimed_at/expires_at while a
                        // fresh claim is still propagating. Don't render an
                        // empty date with `(0d / 0d remaining)` zeros — say
                        // "pending activation" so the user knows to wait.
                        out.push_str(&t(Msg::CpPlanPending { plan: &plan.plan_name }));
                    } else {
                        out.push_str(&t(Msg::CpPlanActive {
                            plan: &plan.plan_name,
                            expires_at: &plan.expires_at,
                            remaining_days: plan.remaining_days,
                            total_days: plan.total_days,
                        }));
                    }
                }
                if !s.rate_limit_windows.is_empty() {
                    for w in s.rate_limit_windows.iter().filter(|w| w.show_enable == 1) {
                        // All visible windows are short rolling (≤5h) —
                        // standard usage line.
                        out.push_str(&t(Msg::CpUsageLine {
                            usage: &w.usage_status_desc,
                            reset_at: &w.reset_at_display,
                            duration: &format_duration_secs(w.seconds_until_reset),
                        }));
                    }
                } else {
                    // Backward-compat path for old server responses.
                    //
                    // `window_quota_exhausted=true` and `current_usage` are
                    // independent fields on the legacy response, and the
                    // server can — and visibly does — set BOTH simultaneously
                    // (typically `current_usage.usage_status_desc=0%` for a
                    // freshly-reset short window plus the exhaustion flag for
                    // a separately-tracked longer window). Rendering both
                    // produces the contradictory `用量 0% / ⚠额度已满` pair
                    // the user reported as "v4.23.2 怎么还是这么展示". When
                    // both fire, the user-actionable message is the
                    // exhaustion warning; the usage line at 0% reads as
                    // "you're fine" and just confuses things. Surface the
                    // warning alone — same precedence the new
                    // `rate_limit_windows` path already encodes (it picks
                    // exactly one row per visible window).
                    if s.window_quota_exhausted {
                        if let Some(hint) = &s.window_quota_hint {
                            out.push_str(&t(Msg::CpWindowQuotaHint { hint }));
                        } else {
                            out.push_str(&t(Msg::CpWindowQuotaExhausted));
                        }
                    } else if let Some(u) = &s.current_usage {
                        out.push_str(&t(Msg::CpUsageLine {
                            usage: &u.display_desc(),
                            reset_at: &u.reset_at_display,
                            duration: &format_duration_secs(u.seconds_until_reset),
                        }));
                    }
                }
            }
            StepResult::Skipped(reason) if reason == CASCADE_FROM_UPSTREAM_FAIL => {
                // Suppress — cascade from claim failure.
            }
            StepResult::Skipped(reason) => {
                out.push_str(&t(Msg::CpStatusFetchSkipped { reason }));
            }
            StepResult::Err(msg) => {
                // Truncate the error chain so a server-side parse failure
                // doesn't dump the entire response body inline. The cause
                // chain commonly includes the raw JSON via anyhow's
                // `with_context(format!("(body: {})", body))`, easily
                // 200+ chars; the diagnostic value beyond ~150 is low.
                out.push_str(&t(Msg::CpStatusFetchFailed {
                    error: &truncate_inline(msg, 150),
                }));
            }
        }

        out
    }

    /// True iff every persist-relevant step (login + claim + models)
    /// either succeeded outright or was skipped non-fatally (server
    /// reported `duplicate=true`, model list already current, etc.).
    /// Callers use this to decide whether to persist config changes
    /// to disk.
    ///
    /// `claim` MUST be in the predicate: when claim returns `Err`
    /// (e.g. backend 500 like the AtomGit `claim-v2` transaction-
    /// rollback bug), `run()` short-circuits and parks `models` as
    /// `Skipped(CASCADE_FROM_UPSTREAM_FAIL)` so the report stays
    /// focused on the actual failure. Without the claim check the
    /// gate flipped to `true` on every claim-failure path —
    /// triggering `save_and_reload` to rewrite `config.toml`
    /// unconditionally. That clobbered any manual edits the user
    /// made between TUI startup and `/codingplan`, and read as
    /// "claim failed but it still wrote models to my config".
    pub fn should_persist_config(&self) -> bool {
        self.login.is_ok_or_skipped()
            && self.claim.is_ok_or_skipped()
            && self.models.is_ok_or_skipped()
    }
}

/// Display-friendly summary of each step's outcome. Returned by `run`
/// so the caller can render however it wants (plain stdout, TUI body
/// scrollback, future JSON output for scripting).
#[derive(Debug, Clone)]
pub struct SetupReport {
    pub login: StepResult<LoginInfo>,
    pub claim: StepResult<ClaimInfo>,
    /// Per-tier cascade history. Populated by `step_claim` with one
    /// entry per tier actually attempted (in cascade order Max → Pro
    /// → Lite). Empty when the cascade never ran (e.g. login failed
    /// upstream — claim is `Skipped(CASCADE_FROM_UPSTREAM_FAIL)`) or
    /// when a legacy test fixture wants the old single-row claim
    /// summary. `render` walks this to emit one row per tier so
    /// refused / errored intermediate tiers are visible, not hidden
    /// behind a single "claim failed" summary.
    pub claim_attempts: Vec<TierAttempt>,
    pub models: StepResult<ModelsInfo>,
    pub status: StepResult<StatusResponse>,
    /// True when any API call rejected the stored bearer token
    /// (401/403). `is_logged_in()` only checks "does auth.toml exist"
    /// and `get_valid_token` only refreshes when the recorded
    /// `expires_in` says so — neither catches a server-side revocation
    /// or a refresh-token that the broker no longer accepts. Shells
    /// (TUI `/codingplan`, CLI `atomcode codingplan`) read this flag
    /// to drive an inline re-OAuth + retry instead of leaving the user
    /// staring at a "claim failed — run `atomcode login` again" line
    /// when `/login` would have fixed it in one step.
    pub auth_expired: bool,
}

#[derive(Debug, Clone)]
pub struct LoginInfo {
    pub username: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ClaimInfo {
    pub message: String,
    /// true when server reported `duplicate=true` — surfaces in the
    /// rendered report as "(already claimed)" rather than "(just claimed)".
    pub duplicate: bool,
    /// The CodingPlan tier the cascade landed on. `Max` if the
    /// highest-tier claim succeeded, `Pro` / `Lite` for fallbacks.
    /// Threaded into `step_models_and_register` as the `?plan_type=`
    /// argument so the model list comes back with availability gated
    /// to the user's actual entitlement.
    pub plan_type: PlanType,
}

/// Per-tier outcome captured while `step_claim` walks the cascade.
/// Surfaces in `SetupReport::render` as one row per attempted tier so
/// the user can see exactly why the cascade stopped where it did — a
/// single "claim failed: Lite: 暂无开放" line hid the Max / Pro tier
/// rejections users wanted to see.
#[derive(Debug, Clone)]
pub enum TierOutcome {
    /// `success=true` on this tier — cascade winner. `plan_name` is the
    /// server's view of the user's actual plan (e.g. "CodingPlan Pro");
    /// empty on legacy gateways, in which case the renderer falls back
    /// to the requested tier.
    Claimed { message: String, plan_name: String },
    /// `duplicate=true` — user already held this (or a higher) tier;
    /// cascade treats this as winner and stops. `plan_name` as above.
    AlreadyHeld { message: String, plan_name: String },
    /// `2xx success=false duplicate=false` — per-tier refusal (e.g.
    /// `额度已满` / `暂无开放`). Cascade walks past to the next tier.
    Refused { message: String },
    /// Transport / 5xx / parse failure. Cascade aborts.
    Errored { error: String },
}

#[derive(Debug, Clone)]
pub struct TierAttempt {
    pub tier: PlanType,
    pub outcome: TierOutcome,
}

#[derive(Debug, Clone)]
pub struct ModelsInfo {
    /// Model names of the **available** subset, in server order.
    /// Parallel to `provider_names` — these are the entries that
    /// actually got registered as providers.
    pub display_names: Vec<String>,
    /// Provider keys actually inserted into Config (available only).
    pub provider_names: Vec<String>,
    /// Which of `provider_names` was set as `default_provider`.
    pub default_provider: String,
    /// Outcome of vision_preprocessor_provider auto-config. Drives the
    /// "Vision preprocessor → ..." line in the rendered report.
    pub vision_preprocessor: VisionPreprocessorOutcome,
    /// Full v2 model list — including `plan_available=false` entries
    /// that we didn't register as providers. Renderer iterates this
    /// to show locked models with strikethrough so users see what
    /// upgrading the plan would unlock.
    pub all_models: Vec<ModelEntry>,
}

/// Entry point. Mutates `config` in place (providers + default_provider);
/// the caller is responsible for persisting it to disk after a successful
/// run. This keeps the core free of I/O concerns — tests can call `run`
/// against a `Config::default()` without touching the filesystem.
///
/// Emits exactly one `TakeCodingplan { Success | Fail }` event at each exit path.
pub fn run(
    config: &mut Config,
    tel: Option<&Arc<atomcode_telemetry::Telemetry>>,
) -> Result<SetupReport> {
    // Step 1: login
    let login = step_login(tel);
    if login.is_err() {
        // No point continuing — every downstream call needs a token.
        if let Some(t) = tel {
            t.track(atomcode_telemetry::Event::TakeCodingplan {
                type_: atomcode_telemetry::CodingplanResult::Fail,
                error_kind: Some(atomcode_telemetry::CodingplanErrorKind::AuthError),
                error_data: Some(serde_json::json!({
                    "step": "login",
                    "message": "Not logged in",
                }).to_string()),
            });
        }
        // Use the cascade sentinel so format() suppresses the three
        // "Foo failed — skipped: login failed" rows that used to spam
        // the report. The login-failure line above is the only thing
        // worth showing; the rest is implied.
        return Ok(SetupReport {
            login,
            claim: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            claim_attempts: Vec::new(),
            models: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            status: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            auth_expired: false,
        });
    }

    // Step 2: claim — cascade Max → Pro → Lite, first success wins.
    let (claim, claim_attempts, claim_auth_expired) = step_claim();
    if claim.is_err() {
        // Claim failed at every tier — adding providers / fetching
        // status both make no sense without an active plan. Bail
        // with cascade markers for models/status; `claim_attempts`
        // still carries every tier's outcome so the renderer shows
        // the per-tier rows that explain WHY the cascade gave up.
        return Ok(SetupReport {
            login,
            claim,
            claim_attempts,
            models: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            status: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            auth_expired: claim_auth_expired,
        });
    }

    // Decide the plan_type to send to /models-v2. Three sources:
    //   * Fresh `Ok` claim: use the tier the cascade landed on.
    //   * `Skipped` (server returned `duplicate=true` at one of the
    //     tiers): step_claim picked the tier it stopped at; we don't
    //     have the structured value here, so fall back to Max — the
    //     server will gate availability the same way regardless. Pro
    //     and Lite users will see Pro/Max-tier models marked
    //     `plan_available=false` and rendered with strikethrough,
    //     which matches the spec ("show locked models too").
    //   * (Err is unreachable here — handled above.)
    let plan_type_for_models = match &claim {
        StepResult::Ok(info) => info.plan_type,
        _ => PlanType::Max,
    };

    // Step 3: models — critical. Without models there's nothing to set up.
    let (models, models_auth_expired) = step_models_and_register(config, plan_type_for_models);
    if models.is_err() {
        if let Some(t) = tel {
            t.track(atomcode_telemetry::Event::TakeCodingplan {
                type_: atomcode_telemetry::CodingplanResult::Fail,
                error_kind: Some(atomcode_telemetry::CodingplanErrorKind::NetworkError),
                error_data: Some(serde_json::json!({
                    "step": "models",
                    "message": "Failed to fetch model list",
                }).to_string()),
            });
        }
        // Same cascade pattern: the models-failure line above is the
        // explanation; "Status fetch failed — skipped: models step
        // failed" adds nothing.
        return Ok(SetupReport {
            login,
            claim,
            claim_attempts,
            models,
            status: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            auth_expired: models_auth_expired,
        });
    }

    // Step 4: status — warn-only. A 401 here is rare (claim+models
    // both passed) but still worth surfacing so a retry has a chance
    // to capture the warm token.
    let (status, status_auth_expired) = step_status();

    // All critical steps (login + models) succeeded. Emit success event.
    if let Some(t) = tel {
        t.track(atomcode_telemetry::Event::TakeCodingplan {
            type_: atomcode_telemetry::CodingplanResult::Success,
            error_kind: None,
            error_data: Some(serde_json::json!({
                "step": null,
            }).to_string()),
        });
    }

    Ok(SetupReport {
        login,
        claim,
        claim_attempts,
        models,
        status,
        auth_expired: status_auth_expired,
    })
}

/// Sentinel reason used when downstream steps are skipped because an
/// earlier required step failed (login / claim / models). `format()`
/// recognises this exact string and renders nothing — the upstream
/// failure line above already explains why nothing came after it.
const CASCADE_FROM_UPSTREAM_FAIL: &str = "__cascade_upstream_fail__";

fn step_login(tel: Option<&Arc<atomcode_telemetry::Telemetry>>) -> StepResult<LoginInfo> {
    if auth::is_logged_in() {
        // Already authed — surface the stored identity so the report
        // shows *who* we're running as, not a bare "skipped". When
        // display-name and username differ (the common case), show
        // both so the user can tell them apart: `TheoCui(saulcy)`.
        if let Some(info) = auth::get_stored_auth() {
            let display = match info.user.name.as_deref() {
                Some(name) if !name.is_empty() && name != info.user.username => {
                    format!("{}({})", name, info.user.username)
                }
                _ => info.user.username.clone(),
            };
            return StepResult::Skipped(format!("already logged in as {}", display));
        }
        // Weird: is_logged_in said yes but stored auth is None. Treat
        // as "login succeeded, details unavailable" rather than failing.
        return StepResult::Skipped("already logged in".into());
    }
    // Not logged in — run OAuth. This prints to stdout + opens a browser.
    // Callers in TUI context must have already suspended raw mode before
    // calling `run`.
    match auth::login(tel).and_then(|a| auth::save_auth(&a).map(|_| a)) {
        Ok(auth_info) => StepResult::Ok(LoginInfo {
            username: auth_info.user.username.clone(),
            display_name: auth_info.user.name.clone(),
            email: auth_info.user.email.clone(),
        }),
        Err(e) => StepResult::Err(format!("login failed: {:#}", e)),
    }
}

/// Walk `PlanType::CASCADE_ORDER` (Max → Pro → Lite), POSTing
/// `claim-v2` for each tier, and stop at the first that lands the
/// user with an entitlement. Two outcomes count as "stop":
///
///   * `success=true`              — fresh claim of this tier.
///   * `duplicate=true`            — user already holds this tier (or
///                                   higher). Treat as success and use
///                                   this tier as the working tier;
///                                   trying lower tiers wouldn't help.
///
/// `success=false && duplicate=false` for a 2xx response is a per-tier
/// "you can't have this" signal (e.g. quota exhausted at the Max tier
/// but Pro/Lite slots still open). Try the next tier with the message
/// preserved as the "last error" we'll show if everything below also
/// fails.
///
/// Transport / 5xx errors abort the whole cascade — those mean the
/// server is in a bad state, not "this tier is unavailable", so
/// retrying lower tiers would just stack identical failures.
/// Walk the cascade and capture every tier's outcome.
///
/// Returns `(overall, attempts, auth_expired)`:
/// * `overall` — the legacy single-summary view of what happened
///   (`Ok` / `Skipped` / `Err`). Drives `should_persist_config` and
///   the downstream `step_models_and_register` plan-type selection.
/// * `attempts` — every tier actually attempted, in cascade order.
///   Renderer walks this to emit one row per tier (refused /
///   errored / claimed) so users can see the full picture instead
///   of just the winner.
/// * `auth_expired` — true iff the failure was a 401/403 from
///   `claim-v2` (or a `from_stored_auth` refresh failure). Bubbled
///   up to `SetupReport.auth_expired` so the shell knows to
///   re-OAuth and retry instead of just printing the failure.
fn step_claim() -> (StepResult<ClaimInfo>, Vec<TierAttempt>, bool) {
    let client = match Client::from_stored_auth() {
        Ok(c) => c,
        Err(e) => {
            let auth_expired = is_auth_expired(&e);
            return (
                StepResult::Err(format!("build client: {:#}", e)),
                Vec::new(),
                auth_expired,
            );
        }
    };
    let mut attempts: Vec<TierAttempt> = Vec::with_capacity(PlanType::CASCADE_ORDER.len());
    let mut last_msg = String::new();
    for &tier in PlanType::CASCADE_ORDER {
        match client.claim_v2(tier) {
            Ok(resp) => {
                if resp.duplicate {
                    attempts.push(TierAttempt {
                        tier,
                        outcome: TierOutcome::AlreadyHeld {
                            message: resp.message.clone(),
                            plan_name: resp.plan_name.clone(),
                        },
                    });
                    let skipped = StepResult::Skipped(if resp.message.is_empty() {
                        format!(
                            "already claimed (or under review) — using {}",
                            tier.as_str()
                        )
                    } else {
                        format!("{} ({})", resp.message, tier.as_str())
                    });
                    return (skipped, attempts, false);
                }
                if resp.success {
                    attempts.push(TierAttempt {
                        tier,
                        outcome: TierOutcome::Claimed {
                            message: resp.message.clone(),
                            plan_name: resp.plan_name.clone(),
                        },
                    });
                    let ok = StepResult::Ok(ClaimInfo {
                        message: if resp.message.is_empty() {
                            format!("claimed {}", tier.as_str())
                        } else {
                            resp.message
                        },
                        duplicate: false,
                        plan_type: tier,
                    });
                    return (ok, attempts, false);
                }
                // 2xx + success=false + duplicate=false: per-tier
                // refusal (quota / not eligible / 暂无开放).
                attempts.push(TierAttempt {
                    tier,
                    outcome: TierOutcome::Refused {
                        message: resp.message.clone(),
                    },
                });
                last_msg = if resp.message.is_empty() {
                    format!("{} claim refused", tier.as_str())
                } else {
                    format!("{}: {}", tier.as_str(), resp.message)
                };
            }
            Err(e) => {
                // Transport / 5xx / parse failure — bail. These don't
                // get more useful when retried at a lower tier. Capture
                // the auth-expired bit BEFORE flattening `e` to a string
                // so the shell layer can retry with a fresh OAuth.
                let auth_expired = is_auth_expired(&e);
                let err_text = format!("{:#}", e);
                attempts.push(TierAttempt {
                    tier,
                    outcome: TierOutcome::Errored {
                        error: err_text.clone(),
                    },
                });
                return (
                    StepResult::Err(format!("claim {} request: {}", tier.as_str(), err_text)),
                    attempts,
                    auth_expired,
                );
            }
        }
    }
    // Surface the server's last response message verbatim — already in
    // the user's language. The render layer wraps it in a localized
    // prefix (`× CodingPlan 套餐配置失败 — …`), so a hardcoded English
    // diagnostic wrapper here ("claim failed at every tier — …") would
    // bleed into zh-CN output. Empty → empty; render guards the dangling
    // em-dash.
    let overall = StepResult::Err(last_msg.clone());
    (overall, attempts, false)
}

fn step_models_and_register(
    config: &mut Config,
    plan_type: PlanType,
) -> (StepResult<ModelsInfo>, bool) {
    let client = match Client::from_stored_auth() {
        Ok(c) => c,
        Err(e) => {
            let auth_expired = is_auth_expired(&e);
            return (
                StepResult::Err(format!("build client: {:#}", e)),
                auth_expired,
            );
        }
    };
    let all_models = match client.list_models_v2(plan_type) {
        Ok(v) => v,
        Err(e) => {
            let auth_expired = is_auth_expired(&e);
            return (
                StepResult::Err(format!("list models-v2: {:#}", e)),
                auth_expired,
            );
        }
    };
    if all_models.is_empty() {
        return (
            StepResult::Err(
                "server returned an empty model list — cannot set up any provider".into(),
            ),
            false,
        );
    }

    // Available subset — only these become providers. Locked ones
    // (`plan_available=false`) survive in `all_models` for the
    // strikethrough-display path; registering them as providers would
    // give the user something they can `/model` into that 403s on the
    // first request.
    let available: Vec<&ModelEntry> = all_models.iter().filter(|m| m.plan_available).collect();
    if available.is_empty() {
        return (
            StepResult::Err(format!(
                "no models available on plan {} — server returned {} locked entries",
                plan_type.as_str(),
                all_models.len()
            )),
            false,
        );
    }

    // Wipe any stale AtomGit* entries so we don't accumulate old names.
    let stale: Vec<String> = config
        .providers
        .keys()
        .filter(|k| is_codingplan_provider_name(k))
        .cloned()
        .collect();
    for k in stale {
        config.providers.remove(&k);
    }

    let names: Vec<String> = available
        .iter()
        .map(|m| m.display_model_name.clone())
        .collect();
    let provider_names = provider_names_for(&names);
    let default_provider = provider_names
        .first()
        .cloned()
        .unwrap_or_else(|| PROVIDER_PREFIX.to_string());

    for (pname, m) in provider_names.iter().zip(available.iter()) {
        let pc = build_codingplan_provider(m);
        config.providers.insert(pname.clone(), pc);
    }
    config.default_provider = default_provider.clone();

    // Auto-detect a vision_preprocessor candidate from the freshly
    // installed list. Precedence:
    //   - User-supplied non-AtomGit value: leave alone.
    //   - None / AtomGit-* (i.e. previous /codingplan run): replace
    //     with first VL/OCR model's provider key from the new list,
    //     or clear to None when the new list has no VL candidate.
    let vl_idx = names
        .iter()
        .position(|n| crate::provider::model_name_suggests_vision(n));
    let new_vl_key = vl_idx.map(|i| provider_names[i].clone());

    let vision_preprocessor = {
        let current = config.vision_preprocessor_provider.clone();
        let user_supplied_non_atomgit = current
            .as_deref()
            .map(|k| !k.is_empty() && !is_codingplan_provider_name(k))
            .unwrap_or(false);

        if user_supplied_non_atomgit {
            VisionPreprocessorOutcome::UserSupplied(current.unwrap())
        } else {
            match new_vl_key {
                Some(k) => {
                    config.vision_preprocessor_provider = Some(k.clone());
                    VisionPreprocessorOutcome::AutoSet(k)
                }
                None => {
                    if current.is_some() {
                        config.vision_preprocessor_provider = None;
                        VisionPreprocessorOutcome::Cleared
                    } else {
                        VisionPreprocessorOutcome::UnchangedNone
                    }
                }
            }
        }
    };

    (
        StepResult::Ok(ModelsInfo {
            display_names: names,
            provider_names,
            default_provider,
            vision_preprocessor,
            all_models,
        }),
        false,
    )
}

fn step_status() -> (StepResult<StatusResponse>, bool) {
    let client = match Client::from_stored_auth() {
        Ok(c) => c,
        Err(e) => {
            let auth_expired = is_auth_expired(&e);
            return (
                StepResult::Err(format!("build client: {:#}", e)),
                auth_expired,
            );
        }
    };
    match client.status_v2() {
        Ok(s) => (StepResult::Ok(s), false),
        Err(e) => {
            let auth_expired = is_auth_expired(&e);
            (
                StepResult::Err(format!("status-v2: {:#}", e)),
                auth_expired,
            )
        }
    }
}

/// Truncate a single-line message to at most `max` chars, appending `…`
/// when shortened. Char-boundary safe (won't split a UTF-8 codepoint).
/// Used when rendering error messages whose source includes a server
/// response body — useful diagnostic prefix, useless multi-KB tail.
fn truncate_inline(msg: &str, max: usize) -> String {
    if msg.chars().count() <= max {
        return msg.to_string();
    }
    let mut out: String = msg.chars().take(max).collect();
    out.push('…');
    out
}

/// Format a duration in seconds as a short human-readable label —
/// `90s`, `5m`, `2h 30m`, `3d 4h`. Replaces the previous "{N}s" which
/// was unreadable for anything past a minute (e.g. "in 86340s" instead
/// of "in 23h 59m").
///
/// Pick the rate-limit window that is *actually blocking* the user, if any.
///
/// `pub` so the `/status` rendering in atomcode-tuix can share the same
/// formatter — keeps the `用量 重置于 ...（2h 后）` line consistent
/// between `/login`'s CodingPlan setup output and `/status`'s
/// CodingPlan section. (Pre-fix they diverged: setup said `2h`, status
/// said `5984s`.)
pub fn format_duration_secs(secs: i64) -> String {
    if secs < 0 {
        return "—".into();
    }
    let s = secs as u64;
    if s < 60 {
        return format!("{}s", s);
    }
    let (m, sr) = (s / 60, s % 60);
    if m < 60 {
        return if sr == 0 { format!("{}m", m) } else { format!("{}m {}s", m, sr) };
    }
    let (h, mr) = (m / 60, m % 60);
    if h < 24 {
        return if mr == 0 { format!("{}h", h) } else { format!("{}h {}m", h, mr) };
    }
    let (d, hr) = (h / 24, h % 24);
    if hr == 0 { format!("{}d", d) } else { format!("{}d {}h", d, hr) }
}

/// Decide the config-key name for each model. Single model → bare
/// `AtomGit` (keeps the name tidy for the common case); 2+ models →
/// `AtomGit-{name with / replaced by -}`.
fn provider_names_for(model_names: &[String]) -> Vec<String> {
    if model_names.len() == 1 {
        vec![PROVIDER_PREFIX.to_string()]
    } else {
        model_names
            .iter()
            .map(|m| format!("{}-{}", PROVIDER_PREFIX, sanitize_model_for_name(m)))
            .collect()
    }
}

/// Turn `moonshotai/Kimi-K2-Instruct` → `moonshotai-Kimi-K2-Instruct`.
/// Only swaps `/`; other punctuation stays verbatim (model names in the
/// wild use `.` and digits freely, and TOML keys handle those fine).
fn sanitize_model_for_name(model: &str) -> String {
    model.replace('/', "-")
}

/// Match `AtomGit` OR `AtomGit-<anything>` — the set of config keys
/// owned by the coding-plan flow. Used to wipe stale entries before
/// re-populating from the fresh model list.
fn is_codingplan_provider_name(name: &str) -> bool {
    name == PROVIDER_PREFIX || name.starts_with(&format!("{}-", PROVIDER_PREFIX))
}

/// Build a ProviderConfig from a model-list entry. The server's
/// per-model fields take precedence; missing fields fall back to the
/// historical fallbacks ([`codingplan_llm_base_url`] / `PROVIDER_TYPE`
/// / `CONTEXT_WINDOW`) so older `models-v2` payloads without the new
/// columns continue to work without code changes.
///
/// `api_key` stays `None` regardless — `create_provider()` loads the
/// OAuth token at runtime via `auth.toml` so we never persist it into
/// the user's `config.toml`.
fn build_codingplan_provider(entry: &ModelEntry) -> ProviderConfig {
    ProviderConfig {
        provider_type: entry
            .provider_type
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| PROVIDER_TYPE.to_string()),
        api_key: None,
        model: entry.display_model_name.clone(),
        base_url: Some(
            entry
                .base_url
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(codingplan_llm_base_url),
        ),
        system_prompt: None,
        user_agent: None,
        // `context_window: 0` from a misconfigured row would degrade
        // every request to a zero-token window; treat that as
        // "missing" and fall back rather than ship a broken provider.
        context_window: entry
            .context_window
            .filter(|n| *n > 0)
            .unwrap_or(CONTEXT_WINDOW),
        max_tokens: None,
        thinking_type: None,
        thinking_keep: None,
        reasoning_history: None,
        reasoning_effort: None,
        thinking_enabled: None,
        thinking_budget: None,
        skip_tls_verify: false,
        ephemeral: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Build a `ModelEntry` for tests that only care about the
    /// model name and want every other field to take its fallback
    /// (`base_url` → [`codingplan_llm_base_url`], `provider_type` →
    /// `PROVIDER_TYPE`, `context_window` → `CONTEXT_WINDOW`,
    /// `plan_available: true`).
    /// Lets the bulk of the test suite stay short while the
    /// per-field-override behaviour gets its own dedicated tests
    /// further down.
    fn entry(display_model_name: &str) -> super::super::types::ModelEntry {
        super::super::types::ModelEntry {
            display_model_name: display_model_name.to_string(),
            plan_available: true,
            ..Default::default()
        }
    }

    fn blank_config() -> Config {
        Config::default()
    }

    #[test]
    fn single_model_uses_bare_prefix() {
        let names = vec!["moonshotai/Kimi-K2-Instruct".into()];
        let p = provider_names_for(&names);
        assert_eq!(p, vec!["AtomGit".to_string()]);
    }

    #[test]
    fn multiple_models_expand_to_prefix_suffixes() {
        let names = vec![
            "moonshotai/Kimi-K2-Instruct".into(),
            "anthropic/claude-3.5-sonnet".into(),
            "openai/gpt-5".into(),
        ];
        let p = provider_names_for(&names);
        assert_eq!(
            p,
            vec![
                "AtomGit-moonshotai-Kimi-K2-Instruct".to_string(),
                "AtomGit-anthropic-claude-3.5-sonnet".to_string(),
                "AtomGit-openai-gpt-5".to_string(),
            ]
        );
    }

    #[test]
    fn sanitize_replaces_slash_only() {
        // `/` becomes `-`; `.` and digits stay (valid in TOML keys).
        assert_eq!(
            sanitize_model_for_name("anthropic/claude-3.5-sonnet"),
            "anthropic-claude-3.5-sonnet"
        );
    }

    #[test]
    fn is_codingplan_name_matches_prefix_and_exact() {
        assert!(is_codingplan_provider_name("AtomGit"));
        assert!(is_codingplan_provider_name("AtomGit-foo"));
        assert!(is_codingplan_provider_name("AtomGit-moonshotai-Kimi-K2"));
        assert!(!is_codingplan_provider_name("AtomGitPlus"));
        assert!(!is_codingplan_provider_name("atomgit")); // case-sensitive
        assert!(!is_codingplan_provider_name("claude"));
    }

    #[test]
    fn step_models_wipes_stale_atomgit_entries() {
        // Simulate a user who previously ran `/login` (old MiniMax entry)
        // and a manual `/provider` session (custom Anthropic entry). After
        // coding-plan setup, only fresh AtomGit* entries should remain;
        // the manual Anthropic one stays.
        let mut config = blank_config();
        config.providers.insert(
            "AtomGit".to_string(),
            build_codingplan_provider(&entry("stale-MiniMax")),
        );
        config.providers.insert(
            "AtomGit-legacy".to_string(),
            build_codingplan_provider(&entry("another-stale")),
        );
        config.providers.insert(
            "claude".to_string(),
            build_codingplan_provider(&entry("anthropic/claude-3.5")),
        );

        // Manually drive the "install" side without network — mirror
        // what step_models_and_register does after a successful API call.
        let names = vec!["meta-llama/Llama-3-70B".to_string()];
        let stale: Vec<String> = config
            .providers
            .keys()
            .filter(|k| is_codingplan_provider_name(k))
            .cloned()
            .collect();
        for k in stale {
            config.providers.remove(&k);
        }
        let provider_names = provider_names_for(&names);
        for (pname, m) in provider_names.iter().zip(names.iter()) {
            config
                .providers
                .insert(pname.clone(), build_codingplan_provider(&entry(m)));
        }
        config.default_provider = provider_names[0].clone();

        assert_eq!(config.providers.len(), 2, "claude + one fresh AtomGit");
        assert!(
            config.providers.contains_key("claude"),
            "unrelated entry kept"
        );
        assert!(
            config.providers.contains_key("AtomGit"),
            "fresh AtomGit added"
        );
        assert!(
            !config.providers.contains_key("AtomGit-legacy"),
            "stale removed"
        );
        let fresh = &config.providers["AtomGit"];
        assert_eq!(fresh.model, "meta-llama/Llama-3-70B");
        assert_eq!(
            fresh.base_url.as_deref(),
            Some(codingplan_llm_base_url().as_str())
        );
        assert_eq!(fresh.provider_type, PROVIDER_TYPE);
        assert_eq!(config.default_provider, "AtomGit");
    }

    #[test]
    fn codingplan_llm_base_url_defaults_to_new_signed_gateway() {
        // Lock in the default. If `ATOMCODE_CODINGPLAN_LLM_BASE_URL` is
        // set in the test environment (CI / staging override / dev box
        // with a stray export), honour it — otherwise the default must
        // be the modern `llm-api.atomgit.com` host. Anything else (most
        // notably the legacy `api-ai.gitcode.com`) silently disables
        // codingplan request signing because `is_atomgit_gateway` in
        // `coding_plan::crypto` also whitelists the legacy host.
        //
        // OnceLock caches across test threads, so this test reflects
        // whatever the env was at the FIRST call site in the process.
        // That's deliberate — it ensures every test in this module
        // agrees on the URL, mirroring production behaviour where the
        // value is fixed for the lifetime of one `atomcode` run.
        let actual = codingplan_llm_base_url();
        let env_override = std::env::var("ATOMCODE_CODINGPLAN_LLM_BASE_URL")
            .ok()
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty());
        if let Some(want) = env_override {
            assert_eq!(actual, want, "env override must win when set");
        } else {
            assert_eq!(
                actual, "https://llm-api.atomgit.com/v1",
                "default must point at the signed gateway (NOT legacy api-ai.gitcode.com); \
                 otherwise codingplan signing never engages"
            );
        }
    }

    #[test]
    fn build_provider_uses_canonical_defaults() {
        // All optional server fields missing → fall back to the
        // historical constants. Pins the back-compat path for
        // older `models-v2` builds that don't yet emit `base_url`,
        // `type`, or `context_window`.
        let p = build_codingplan_provider(&entry("foo/bar"));
        assert_eq!(p.provider_type, "openai");
        assert_eq!(
            p.base_url.as_deref(),
            Some(codingplan_llm_base_url().as_str())
        );
        assert_eq!(p.context_window, 64_000);
        assert!(
            p.api_key.is_none(),
            "token loaded at runtime from auth.toml"
        );
        assert!(!p.ephemeral);
    }

    #[test]
    fn build_provider_uses_server_overrides_when_present() {
        // Per-model server fields take precedence over the
        // hard-coded fallbacks. Mirrors the new wire shape:
        // `base_url`, `type`, `context_window` all populated.
        let e = super::super::types::ModelEntry {
            id: 2052994857682014210,
            is_infinity: 2,
            is_atomcode_exclusive: 1,
            display_model_name: "GLM-5.1".into(),
            base_url: Some("https://custom.example.com/v1".into()),
            provider_type: Some("claude".into()),
            context_window: Some(128_000),
            plan_available: true,
        };
        let p = build_codingplan_provider(&e);
        assert_eq!(p.model, "GLM-5.1");
        assert_eq!(p.provider_type, "claude");
        assert_eq!(p.base_url.as_deref(), Some("https://custom.example.com/v1"));
        assert_eq!(p.context_window, 128_000);
    }

    #[test]
    fn build_provider_treats_empty_or_zero_overrides_as_missing() {
        // Defensive: a malformed server row (empty string base_url /
        // type, zero context window) shouldn't ship a provider that
        // refuses every request. Fall back to constants instead.
        let e = super::super::types::ModelEntry {
            display_model_name: "weird".into(),
            base_url: Some(String::new()),
            provider_type: Some(String::new()),
            context_window: Some(0),
            plan_available: true,
            ..Default::default()
        };
        let p = build_codingplan_provider(&e);
        assert_eq!(p.provider_type, "openai");
        assert_eq!(
            p.base_url.as_deref(),
            Some(codingplan_llm_base_url().as_str())
        );
        assert_eq!(p.context_window, 64_000);
    }

    #[test]
    fn model_entry_deserialises_new_wire_shape() {
        // The exact JSON payload from the spec —
        // every new field must parse without error.
        let raw = r#"[{
            "id": 2052994857682014210,
            "is_infinity": 2,
            "is_atomcode_exclusive": 1,
            "display_model_name": "GLM-5.1",
            "base_url": "https://api-ai.gitcode.com/v1",
            "type": "openai",
            "context_window": 64000,
            "plan_available": true
        }]"#;
        let list: Vec<super::super::types::ModelEntry> =
            serde_json::from_str(raw).expect("payload deserialises");
        assert_eq!(list.len(), 1);
        let m = &list[0];
        assert_eq!(m.id, 2052994857682014210);
        assert_eq!(m.is_infinity, 2);
        assert_eq!(m.is_atomcode_exclusive, 1);
        assert_eq!(m.display_model_name, "GLM-5.1");
        assert_eq!(m.base_url.as_deref(), Some("https://api-ai.gitcode.com/v1"));
        assert_eq!(m.provider_type.as_deref(), Some("openai"));
        assert_eq!(m.context_window, Some(64_000));
        assert!(m.plan_available);
    }

    #[test]
    fn model_entry_deserialises_legacy_wire_shape() {
        // Older server build with only the v2-minimum fields. New
        // fields default to `None` / `0` so older payloads keep
        // working — the orchestrator falls back to the constants.
        let raw = r#"[{
            "id": 1,
            "is_atomcode_exclusive": 0,
            "display_model_name": "legacy/model",
            "plan_available": true
        }]"#;
        let list: Vec<super::super::types::ModelEntry> =
            serde_json::from_str(raw).expect("legacy payload deserialises");
        let m = &list[0];
        assert_eq!(m.display_model_name, "legacy/model");
        assert!(m.base_url.is_none());
        assert!(m.provider_type.is_none());
        assert!(m.context_window.is_none());
        assert_eq!(m.is_infinity, 0);
    }

    /// Render exercise: every step Ok. Verifies the three-line output
    /// structure the user sees on a fresh happy-path run.
    #[test]
    fn render_happy_path_has_all_checkmarks() {
        let report = SetupReport {
            login: StepResult::Ok(LoginInfo {
                username: "theo".into(),
                display_name: Some("Theo".into()),
                email: Some("theo@example.com".into()),
            }),
            claim: StepResult::Ok(ClaimInfo {
                message: "领取成功".into(),
                duplicate: false,
                plan_type: PlanType::Max,
            }),
            claim_attempts: Vec::new(),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["moonshotai/Kimi-K2-Instruct".into()],
                provider_names: vec!["AtomGit".into()],
                default_provider: "AtomGit".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,
                all_models: vec![],
            }),
            status: StepResult::Ok(crate::coding_plan::types::StatusResponse {
                codingplan_free: Some(crate::coding_plan::types::PlanInfo {
                    plan_name: "CodingPlan Free".into(),
                    status: 1,
                    claimed_at: "2026-04-22".into(),
                    expires_at: "2026-05-22".into(),
                    remaining_days: 29,
                    total_days: 30,
                    apply_id: 1,
                }),
                current_usage: Some(crate::coding_plan::types::UsageInfo {
                    placeholder: false,
                    window_token_limit: 50000,
                    window_tokens_used: 0,
                    usage_percent: 0.0,
                    window_hours: 1,
                    reset_at: "2026-04-23T12:13:14".into(),
                    reset_at_display: "12:13".into(),
                    seconds_until_reset: 693,
                    reset_label: String::new(),
                    usage_status_desc: String::new(),
                }),
                audit_status: 1,
                expires_at: Some("2026-05-22".into()),
                window_quota_exhausted: false,
                window_quota_hint: None,
                rate_limit_windows: vec![],
            }),
            auth_expired: false,
        };
        let out = report.render();
        assert!(out.contains("✓ Logged in as Theo"));
        assert!(out.contains("theo@example.com"));
        assert!(out.contains("CodingPlan claimed"));
        assert!(out.contains("Kimi-K2-Instruct"));
        assert!(out.contains("AtomGit"));
        assert!(out.contains("(default)"));
        assert!(out.contains("CodingPlan Free"));
        assert!(out.contains("12:13"));
        assert!(report.should_persist_config());
    }

    /// Render exercise: claim returned duplicate=true. Must render as
    /// a skipped checkmark, NOT a failure — user already had the plan.
    #[test]
    fn render_claim_duplicate_renders_as_success() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in as theo".into()),
            claim: StepResult::Skipped("already claimed / in review".into()),
            claim_attempts: Vec::new(),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["a/b".into()],
                provider_names: vec!["AtomGit".into()],
                default_provider: "AtomGit".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,
                all_models: vec![],
            }),
            status: StepResult::Err("request timeout".into()),
            auth_expired: false,
        };
        let out = report.render();
        assert!(out.contains("✓ already logged in"));
        assert!(out.contains("already claimed"));
        assert!(!out.contains("× CodingPlan claim"), "duplicate ≠ failure");
        // Status failed but it's warn-only: ⚠ prefix, NOT ✗.
        assert!(out.contains("⚠ Status fetch failed"));
        assert!(!out.contains("× Status"));
        // Login skipped + models ok ⇒ config should still be persisted.
        assert!(report.should_persist_config());
    }

    /// Regression: when a fresh claim hasn't activated yet the backend
    /// returns `claimed_at: null, expires_at: null, total_days: 0,
    /// remaining_days: 0`. Pre-fix the render line came out as
    /// `Plan: CodingPlan Free  ·  expires  (0d / 0d remaining)` — empty
    /// gap in the middle + bogus zeros, looked like a parser bug. Now
    /// the empty-expiry case shows a meaningful "pending activation"
    /// state instead.
    #[test]
    fn render_status_pending_activation_omits_zero_expiry() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Ok(ClaimInfo {
                message: "claimed".into(),
                duplicate: false,
                plan_type: PlanType::Max,
            }),
            claim_attempts: Vec::new(),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["a/b".into()],
                provider_names: vec!["AtomGit".into()],
                default_provider: "AtomGit".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,
                all_models: vec![],
            }),
            status: StepResult::Ok(crate::coding_plan::types::StatusResponse {
                codingplan_free: Some(crate::coding_plan::types::PlanInfo {
                    plan_name: "CodingPlan Free".into(),
                    status: 0,
                    claimed_at: String::new(),
                    expires_at: String::new(),
                    remaining_days: 0,
                    total_days: 0,
                    apply_id: 0,
                }),
                current_usage: None,
                audit_status: 0,
                expires_at: None,
                window_quota_exhausted: false,
                window_quota_hint: None,
                rate_limit_windows: vec![],
            }),
            auth_expired: false,
        };
        let out = report.render();
        assert!(out.contains("Plan: CodingPlan Free"), "plan name still shown: {}", out);
        assert!(
            out.contains("pending activation"),
            "must surface pending state to user: {}",
            out
        );
        assert!(
            !out.contains("(0d / 0d"),
            "bogus zero countdown must not render: {}",
            out
        );
        assert!(
            !out.contains("expires  ("),
            "empty expires-date with double space must not render: {}",
            out
        );
    }

    /// Render exercise: login failed. Downstream steps are pre-marked
    /// with the cascade sentinel; format() suppresses them so only the
    /// login-failure line appears. Config must NOT be persisted.
    #[test]
    fn render_login_failed_blocks_persist_and_suppresses_cascade() {
        let report = SetupReport {
            login: StepResult::Err("browser handshake timed out".into()),
            claim: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            claim_attempts: Vec::new(),
            models: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            status: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            auth_expired: false,
        };
        let out = report.render();
        assert!(out.contains("× Login failed"));
        // Cascade rows must NOT appear.
        assert!(!out.contains("CodingPlan claim"), "no cascade claim row on login fail");
        assert!(!out.contains("Models step"), "no cascade models row on login fail");
        assert!(!out.contains("Status fetch"), "no cascade status row on login fail");
        // Login Err ⇒ should_persist_config = false (login.is_ok_or_skipped() is false).
        assert!(
            !report.should_persist_config(),
            "don't write config on login failure"
        );
    }

    /// Regression: claim returned Err (e.g. AtomGit `claim-v2` 500
    /// with the Spring `UnexpectedRollbackException` payload). `run()`
    /// short-circuits and stamps the cascade sentinel into `models` /
    /// `status`. Before this fix, `should_persist_config` only
    /// checked `login` and `models` — both `is_ok_or_skipped()` =
    /// `true` here — so the gate flipped open and `save_and_reload`
    /// rewrote `config.toml`. Surfaced to the user as "claim failed
    /// but models still got written to my config". Now the predicate
    /// also requires `claim.is_ok_or_skipped()` so any real claim
    /// failure blocks the persist.
    #[test]
    fn claim_err_blocks_persist() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Err(
                "claim Pro request: claim-v2 returned 500 Internal Server Error \
                 — Transaction rolled back because it has been marked as rollback-only"
                    .into(),
            ),
            claim_attempts: Vec::new(),
            models: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            status: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            auth_expired: false,
        };
        assert!(
            !report.should_persist_config(),
            "claim Err must block save_and_reload — config rewrite was overwriting \
             manual edits between TUI startup and /codingplan",
        );
        // Sanity-check: the duplicate-claim Skipped path (server says
        // "already claimed") must STILL persist so two-runs-in-a-row
        // /codingplan keeps working as a model-list sync.
        let dup = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Skipped("already claimed / using Max".into()),
            claim_attempts: Vec::new(),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["a/b".into()],
                provider_names: vec!["AtomGit".into()],
                default_provider: "AtomGit".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,
                all_models: vec![],
            }),
            status: StepResult::Err("status fetch timeout".into()),
            auth_expired: false,
        };
        assert!(
            dup.should_persist_config(),
            "duplicate-claim Skipped must still allow persist (it's the model-sync path)",
        );
    }

    /// `auth_expired = true` MUST NOT flip `should_persist_config()`
    /// open on its own — the gate already requires every critical step
    /// to be `is_ok_or_skipped`, and that's where the actual safety
    /// lives. `auth_expired` is a side-channel for the shell to decide
    /// "retry with fresh OAuth"; it's orthogonal to "is this report
    /// good enough to write to disk". Regression guard: a future
    /// refactor that ANDs `auth_expired` into the predicate would
    /// double-gate (claim Err + auth_expired both block) but a future
    /// refactor that ORs it the wrong way would open the persist gate
    /// on an auth-expired-but-otherwise-skipped report. Lock the
    /// orthogonality in.
    #[test]
    fn auth_expired_alone_does_not_change_persist_gate() {
        // All-Skipped report (login skipped, no claim attempted, etc.)
        // with auth_expired=true. Persist gate is driven by the step
        // outcomes — Skipped counts as "ok or skipped" — so this should
        // still allow persist.
        let allow = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Skipped("already claimed".into()),
            claim_attempts: Vec::new(),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["a/b".into()],
                provider_names: vec!["AtomGit".into()],
                default_provider: "AtomGit".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,
                all_models: vec![],
            }),
            status: StepResult::Skipped("ok".into()),
            auth_expired: true,
        };
        assert!(
            allow.should_persist_config(),
            "auth_expired must not gate persist when every critical step \
             is ok/skipped — it's a side-channel for retry, not safety",
        );

        // Claim Err report. Persist gate already false, auth_expired
        // doesn't matter.
        let block = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Err("auth failed".into()),
            claim_attempts: Vec::new(),
            models: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            status: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            auth_expired: true,
        };
        assert!(
            !block.should_persist_config(),
            "claim Err already blocks persist — auth_expired doesn't \
             relax it",
        );
    }

    /// Per-tier cascade rendering: Max refused (额度已满) → Pro
    /// refused (额度已满) → Lite claimed. Only the winning tier
    /// surfaces — intermediate refusals like "Max 套餐尚未开放" are
    /// noise once a higher tier succeeds, and users will pay for
    /// upgrades on the web rather than via the CLI cascade.
    #[test]
    fn render_per_tier_cascade_shows_only_winning_tier() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in as Code_dh".into()),
            claim: StepResult::Ok(ClaimInfo {
                message: "claimed".into(),
                duplicate: false,
                plan_type: PlanType::Lite,
            }),
            claim_attempts: vec![
                TierAttempt {
                    tier: PlanType::Max,
                    outcome: TierOutcome::Refused {
                        message: "额度已满".into(),
                    },
                },
                TierAttempt {
                    tier: PlanType::Pro,
                    outcome: TierOutcome::Refused {
                        message: "额度已满".into(),
                    },
                },
                TierAttempt {
                    tier: PlanType::Lite,
                    outcome: TierOutcome::Claimed {
                        message: "领取成功".into(),
                        plan_name: String::new(),
                    },
                },
            ],
            models: StepResult::Skipped("models step not exercised here".into()),
            status: StepResult::Skipped("status not exercised here".into()),
            auth_expired: false,
        };
        let out = report.render();
        // Lite must surface as 生效 / active (the winner).
        assert!(
            out.contains("CodingPlan Lite 生效")
                || out.contains("CodingPlan Lite active"),
            "Lite success row missing: {}",
            out
        );
        // Max + Pro refusal rows must NOT appear — the cascade
        // intermediates are suppressed by design ("Max 套餐尚未开放"
        // was spamming every successful run before this change).
        assert!(
            !out.contains("CodingPlan Max"),
            "Max refusal row must be suppressed: {}",
            out
        );
        assert!(
            !out.contains("CodingPlan Pro"),
            "Pro refusal row must be suppressed: {}",
            out
        );
        // 「领取」字样应彻底消失 — neither successes nor failures
        // should still carry the old claim wording.
        assert!(
            !out.contains("领取"),
            "no '领取' wording should remain in rendered output: {}",
            out
        );
        // The legacy single-line summary must NOT appear when
        // claim_attempts is populated — would be a duplicate row.
        assert!(
            !out.contains("CodingPlan claimed"),
            "legacy claim-summary row must be suppressed when per-tier rows present: {}",
            out
        );
    }

    /// When the server returns `plan_name`, the success row shows the
    /// user's *actual* plan ("CodingPlan Pro") rather than the requested
    /// cascade tier ("Max"). Fixes the misleading "CodingPlan Max 生效"
    /// line that appeared while the status block below showed Pro.
    #[test]
    fn render_success_row_uses_server_plan_name_over_requested_tier() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Skipped("already claimed — using Max".into()),
            claim_attempts: vec![TierAttempt {
                tier: PlanType::Max,
                outcome: TierOutcome::AlreadyHeld {
                    message: "已领取".into(),
                    plan_name: "CodingPlan Pro".into(),
                },
            }],
            models: StepResult::Skipped("models step not exercised here".into()),
            status: StepResult::Skipped("status not exercised here".into()),
            auth_expired: false,
        };
        let out = report.render();
        assert!(
            out.contains("CodingPlan Pro 生效") || out.contains("CodingPlan Pro active"),
            "server plan_name (Pro) must drive the 生效 row: {}",
            out
        );
        assert!(
            !out.contains("CodingPlan Max"),
            "requested tier (Max) must not surface when plan_name is present: {}",
            out
        );
    }

    /// Legacy gateway: no `plan_name` → fall back to the requested
    /// cascade tier so old servers keep rendering "CodingPlan Lite 生效".
    #[test]
    fn render_success_row_falls_back_to_tier_when_plan_name_empty() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Ok(ClaimInfo {
                message: "claimed".into(),
                duplicate: false,
                plan_type: PlanType::Lite,
            }),
            claim_attempts: vec![TierAttempt {
                tier: PlanType::Lite,
                outcome: TierOutcome::Claimed {
                    message: "领取成功".into(),
                    plan_name: String::new(),
                },
            }],
            models: StepResult::Skipped("models step not exercised here".into()),
            status: StepResult::Skipped("status not exercised here".into()),
            auth_expired: false,
        };
        let out = report.render();
        assert!(
            out.contains("CodingPlan Lite 生效") || out.contains("CodingPlan Lite active"),
            "empty plan_name must fall back to requested tier (Lite): {}",
            out
        );
    }

    /// Per-tier cascade where every tier refused — winning tier is
    /// `None`, overall claim is `Err`. Per-tier failure rows are
    /// suppressed; a single consolidated failure line surfaces so the
    /// user still gets a signal that no plan was activated.
    #[test]
    fn render_per_tier_cascade_all_refused() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            // Bare server message — no English wrapper. Mirrors the
            // new try_claim_with_cascade() output shape.
            claim: StepResult::Err("Lite: 暂无开放".into()),
            claim_attempts: vec![
                TierAttempt {
                    tier: PlanType::Max,
                    outcome: TierOutcome::Refused {
                        message: "暂无开放".into(),
                    },
                },
                TierAttempt {
                    tier: PlanType::Pro,
                    outcome: TierOutcome::Refused {
                        message: "暂无开放".into(),
                    },
                },
                TierAttempt {
                    tier: PlanType::Lite,
                    outcome: TierOutcome::Refused {
                        message: "暂无开放".into(),
                    },
                },
            ],
            models: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            status: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            auth_expired: false,
        };
        let out = report.render();
        // No per-tier failure rows — `Max/Pro/Lite` strings must not
        // appear anywhere in the output.
        for tier in &["Max", "Pro", "Lite"] {
            let needle = format!("CodingPlan {}", tier);
            assert!(
                !out.contains(&needle),
                "{} tier row must be suppressed in all-refused case: {}",
                tier,
                out
            );
        }
        // 「领取」字样应彻底消失.
        assert!(
            !out.contains("领取"),
            "no '领取' wording should remain in rendered output: {}",
            out
        );
        // BUT we must still emit a single consolidated failure line so
        // the user knows the step didn't succeed — driven by the
        // overall `claim` Err falling through to CpClaimFailed.
        assert!(
            out.contains("CodingPlan 套餐配置失败") || out.contains("CodingPlan tier setup failed"),
            "consolidated failure row must appear when no tier succeeds: {}",
            out
        );
        // Models row also suppressed (cascade sentinel).
        assert!(
            !out.contains("Models step"),
            "cascade-from-claim-fail must hide models row: {}",
            out
        );
    }

    /// Per-tier cascade where Max errored (5xx). Per-tier failure
    /// rows are suppressed, but the overall `claim` Err falls through
    /// to a single consolidated failure line — and that line must
    /// truncate a long error so a 500-char stack trace doesn't blow
    /// up the row.
    #[test]
    fn render_all_errored_consolidated_row_truncates_long_message() {
        let long_err = "x".repeat(500);
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Err(format!("claim Max request: {}", long_err)),
            claim_attempts: vec![TierAttempt {
                tier: PlanType::Max,
                outcome: TierOutcome::Errored {
                    error: long_err.clone(),
                },
            }],
            models: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            status: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            auth_expired: false,
        };
        let out = report.render();
        // No per-tier "Max" row.
        assert!(
            !out.contains("CodingPlan Max"),
            "per-tier Max row must be suppressed: {}",
            out
        );
        // Consolidated failure row is present (driven by claim Err).
        assert!(
            out.contains("CodingPlan 套餐配置失败") || out.contains("CodingPlan tier setup failed"),
            "consolidated failure row must appear: {}",
            out
        );
        // The full 500-char error must NOT appear verbatim — truncated.
        assert!(
            !out.contains(&long_err),
            "long error must be truncated, not pasted whole: {}",
            out
        );
    }

    /// All-fail with no server detail anywhere — overall Err is empty
    /// AND every TierAttempt has empty message/error. Render must
    /// emit the bare-prefix variant (no dangling `— `).
    #[test]
    fn render_all_failed_with_no_detail_uses_bare_prefix() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Err(String::new()),
            claim_attempts: vec![
                TierAttempt {
                    tier: PlanType::Max,
                    outcome: TierOutcome::Refused {
                        message: String::new(),
                    },
                },
                TierAttempt {
                    tier: PlanType::Pro,
                    outcome: TierOutcome::Refused {
                        message: String::new(),
                    },
                },
                TierAttempt {
                    tier: PlanType::Lite,
                    outcome: TierOutcome::Refused {
                        message: String::new(),
                    },
                },
            ],
            models: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            status: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            auth_expired: false,
        };
        let out = report.render();
        // Prefix renders.
        assert!(
            out.contains("CodingPlan 套餐配置失败") || out.contains("CodingPlan tier setup failed"),
            "bare failure prefix must appear: {}",
            out
        );
        // No dangling em-dash — body is empty so the suffix is skipped.
        assert!(
            !out.contains("套餐配置失败 — ") && !out.contains("tier setup failed — "),
            "must not render `— ` with empty body: {:?}",
            out
        );
    }

    /// Render exercise: multi-model report. Verifies each provider
    /// name gets its own bullet + `(default)` marks only the first.
    #[test]
    fn render_multi_model_lists_all_providers_with_default_mark() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in as theo".into()),
            claim: StepResult::Ok(ClaimInfo {
                message: String::new(),
                duplicate: false,
                plan_type: PlanType::Max,
            }),
            claim_attempts: Vec::new(),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec![
                    "moonshotai/Kimi-K2-Instruct".into(),
                    "anthropic/claude-3.5-sonnet".into(),
                    "openai/gpt-5".into(),
                ],
                provider_names: vec![
                    "AtomGit-moonshotai-Kimi-K2-Instruct".into(),
                    "AtomGit-anthropic-claude-3.5-sonnet".into(),
                    "AtomGit-openai-gpt-5".into(),
                ],
                default_provider: "AtomGit-moonshotai-Kimi-K2-Instruct".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,
                all_models: vec![],
            }),
            status: StepResult::Err("status endpoint 500".into()),
            auth_expired: false,
        };
        let out = report.render();
        assert!(out.contains("Added 3 providers"));
        assert!(out.contains(
            "AtomGit-moonshotai-Kimi-K2-Instruct  →  moonshotai/Kimi-K2-Instruct  (default)"
        ));
        assert!(
            out.contains("AtomGit-anthropic-claude-3.5-sonnet  →  anthropic/claude-3.5-sonnet\n")
        );
        assert!(
            !out.contains("anthropic/claude-3.5-sonnet  (default)"),
            "only first is default"
        );
    }

    /// Render exercise: claim failed. The cascade markers on models +
    /// status must render as nothing — the claim-failed line is the
    /// explanation, repeating it twice more is noise.
    #[test]
    fn render_claim_failed_suppresses_cascade_rows() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in as theo".into()),
            claim: StepResult::Err("今日codingplan申请额度已满，请明天再试".into()),
            claim_attempts: Vec::new(),
            models: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            status: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            auth_expired: false,
        };
        let out = report.render();
        assert!(out.contains("× CodingPlan tier setup failed"));
        assert!(out.contains("今日codingplan申请额度已满"));
        // The cascade rows must NOT appear.
        assert!(!out.contains("Models step skipped"), "no cascade row for models");
        assert!(!out.contains("Status fetch skipped"), "no cascade row for status");
        assert!(!out.contains("Added "), "must not say 'Added N providers' on claim fail");
        // The huge JSON body that used to leak through here must NOT appear.
        assert!(!out.contains("invalid type: null"));
        assert!(!out.contains("plan_name"));
    }

    /// Non-cascade Skipped reasons still render — only the sentinel
    /// (`__cascade_upstream_fail__`) is suppressed.
    #[test]
    fn render_skipped_with_non_cascade_reason_still_shows() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in as theo".into()),
            claim: StepResult::Skipped("already claimed".into()),
            claim_attempts: Vec::new(),
            models: StepResult::Skipped("models cached locally".into()),
            status: StepResult::Skipped("server returned 503; using cached".into()),
            auth_expired: false,
        };
        let out = report.render();
        assert!(out.contains("Models step skipped — models cached locally"));
        assert!(out.contains("Status fetch skipped — server returned 503"));
    }

    /// Render exercise: status fetch failed with a multi-KB body chain.
    /// Output must be truncated to keep the report readable.
    #[test]
    fn render_status_error_truncates_long_message() {
        let huge = format!(
            "status: parse status response (body: {}): invalid type",
            "x".repeat(1000),
        );
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Ok(ClaimInfo {
                message: "ok".into(),
                duplicate: false,
                plan_type: PlanType::Max,
            }),
            claim_attempts: Vec::new(),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["a/b".into()],
                provider_names: vec!["AtomGit".into()],
                default_provider: "AtomGit".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,
                all_models: vec![],
            }),
            status: StepResult::Err(huge),
            auth_expired: false,
        };
        let out = report.render();
        // Find the status line and check its length is bounded.
        let line = out.lines().find(|l| l.contains("Status fetch failed")).unwrap();
        // 150 chars + ellipsis + prefix + leading spaces ⇒ comfortably under 250.
        assert!(line.chars().count() < 250, "line still ~{} chars long", line.chars().count());
        assert!(line.contains('…'), "truncation marker present");
    }

    #[test]
    fn format_duration_secs_human_readable() {
        assert_eq!(format_duration_secs(0), "0s");
        assert_eq!(format_duration_secs(45), "45s");
        assert_eq!(format_duration_secs(60), "1m");
        assert_eq!(format_duration_secs(90), "1m 30s");
        assert_eq!(format_duration_secs(3600), "1h");
        assert_eq!(format_duration_secs(3660), "1h 1m");
        assert_eq!(format_duration_secs(86400), "1d");
        assert_eq!(format_duration_secs(90060), "1d 1h");
        assert_eq!(format_duration_secs(-1), "—");
    }

    #[test]
    fn truncate_inline_passes_short_strings_through() {
        assert_eq!(truncate_inline("short", 10), "short");
        assert_eq!(truncate_inline("exactly_ten", 11), "exactly_ten");
    }

    #[test]
    fn truncate_inline_appends_ellipsis_when_long() {
        let r = truncate_inline("abcdefghijklmnop", 5);
        assert_eq!(r, "abcde…");
    }

    #[test]
    fn truncate_inline_handles_unicode_safely() {
        // 5 CJK chars = 5 chars (regardless of byte count). No char-boundary panic.
        let r = truncate_inline("一二三四五六七八", 5);
        assert_eq!(r, "一二三四五…");
    }

    // ── Vision-preprocessor auto-config tests ────────────────────────────

    fn vl_model_entry(model: &str) -> super::super::types::ModelEntry {
        super::super::types::ModelEntry {
            id: 1,
            display_model_name: model.to_string(),
            // Tests in this section drive `run_register` directly with
            // a curated `Vec<ModelEntry>` — they're testing the
            // post-availability-filter logic, so every entry counts as
            // "available". The split-by-`plan_available` happens
            // upstream in the real `step_models_and_register`.
            plan_available: true,
            // The new wire-shape optional fields default to None/0 —
            // these tests only care about the model name and the
            // availability flag, so let them fall back to the
            // constants via `Default`.
            ..Default::default()
        }
    }

    /// Helper that mirrors `step_models_and_register`'s wipe-and-insert
    /// + auto-detect body, sans network call. Tests the precedence logic
    /// in isolation.
    fn run_register(
        config: &mut Config,
        models: Vec<super::super::types::ModelEntry>,
    ) -> ModelsInfo {
        let stale: Vec<String> = config
            .providers
            .keys()
            .filter(|k| is_codingplan_provider_name(k))
            .cloned()
            .collect();
        for k in stale {
            config.providers.remove(&k);
        }
        let names: Vec<String> = models.iter().map(|m| m.display_model_name.clone()).collect();
        let provider_names = provider_names_for(&names);
        let default_provider = provider_names
            .first()
            .cloned()
            .unwrap_or_else(|| PROVIDER_PREFIX.to_string());
        for (pname, m) in provider_names.iter().zip(models.iter()) {
            config
                .providers
                .insert(pname.clone(), build_codingplan_provider(m));
        }
        config.default_provider = default_provider.clone();

        let vl_idx = names
            .iter()
            .position(|n| crate::provider::model_name_suggests_vision(n));
        let new_vl_key = vl_idx.map(|i| provider_names[i].clone());
        let vision_preprocessor = {
            let current = config.vision_preprocessor_provider.clone();
            let user_supplied_non_atomgit = current
                .as_deref()
                .map(|k| !k.is_empty() && !is_codingplan_provider_name(k))
                .unwrap_or(false);
            if user_supplied_non_atomgit {
                VisionPreprocessorOutcome::UserSupplied(current.unwrap())
            } else {
                match new_vl_key {
                    Some(k) => {
                        config.vision_preprocessor_provider = Some(k.clone());
                        VisionPreprocessorOutcome::AutoSet(k)
                    }
                    None => {
                        if current.is_some() {
                            config.vision_preprocessor_provider = None;
                            VisionPreprocessorOutcome::Cleared
                        } else {
                            VisionPreprocessorOutcome::UnchangedNone
                        }
                    }
                }
            }
        };

        ModelsInfo {
            display_names: names,
            provider_names,
            default_provider,
            vision_preprocessor,
            // Test helper doesn't exercise the locked-model rendering
            // path; mirror the input slice into all_models so the
            // shape stays consistent if any future assertion peeks.
            all_models: models,
        }
    }

    #[test]
    fn vision_preprocessor_auto_set_when_none_and_list_has_vl() {
        let mut config = blank_config();
        let models = vec![
            vl_model_entry("moonshotai/Kimi-K2-Instruct"),
            vl_model_entry("Qwen/Qwen3-VL-32B-Instruct"),
            vl_model_entry("deepseek/deepseek-v4-flash"),
        ];
        let info = run_register(&mut config, models);
        let expected = "AtomGit-Qwen-Qwen3-VL-32B-Instruct".to_string();
        assert_eq!(
            info.vision_preprocessor,
            VisionPreprocessorOutcome::AutoSet(expected.clone())
        );
        assert_eq!(config.vision_preprocessor_provider, Some(expected));
    }

    #[test]
    fn vision_preprocessor_unchanged_none_when_list_has_no_vl() {
        let mut config = blank_config();
        let models = vec![vl_model_entry("moonshotai/Kimi-K2-Instruct")];
        let info = run_register(&mut config, models);
        assert_eq!(info.vision_preprocessor, VisionPreprocessorOutcome::UnchangedNone);
        assert_eq!(config.vision_preprocessor_provider, None);
    }

    #[test]
    fn vision_preprocessor_overwrites_stale_atomgit_value() {
        let mut config = blank_config();
        config.vision_preprocessor_provider = Some("AtomGit-Qwen-Qwen2-VL-72B".into());
        let models = vec![
            vl_model_entry("Kimi-K2-Instruct"),
            vl_model_entry("Qwen/Qwen3-VL-32B-Instruct"),
        ];
        let info = run_register(&mut config, models);
        let expected = "AtomGit-Qwen-Qwen3-VL-32B-Instruct".to_string();
        assert_eq!(
            info.vision_preprocessor,
            VisionPreprocessorOutcome::AutoSet(expected.clone())
        );
        assert_eq!(config.vision_preprocessor_provider, Some(expected));
    }

    #[test]
    fn vision_preprocessor_cleared_when_stale_atomgit_and_list_has_no_vl() {
        let mut config = blank_config();
        config.vision_preprocessor_provider = Some("AtomGit-Qwen-Qwen2-VL-72B".into());
        let models = vec![vl_model_entry("moonshotai/Kimi-K2-Instruct")];
        let info = run_register(&mut config, models);
        assert_eq!(info.vision_preprocessor, VisionPreprocessorOutcome::Cleared);
        assert_eq!(config.vision_preprocessor_provider, None);
    }

    #[test]
    fn vision_preprocessor_preserves_user_set_non_atomgit() {
        let mut config = blank_config();
        config.vision_preprocessor_provider = Some("Qwen3-VL-32B-Instruct".into());
        let models = vec![
            vl_model_entry("Kimi-K2-Instruct"),
            vl_model_entry("Qwen/Qwen3-VL-32B-Instruct"),
        ];
        let info = run_register(&mut config, models);
        assert_eq!(
            info.vision_preprocessor,
            VisionPreprocessorOutcome::UserSupplied("Qwen3-VL-32B-Instruct".into())
        );
        assert_eq!(
            config.vision_preprocessor_provider.as_deref(),
            Some("Qwen3-VL-32B-Instruct")
        );
    }

    #[test]
    fn vision_preprocessor_recognises_pure_ocr_model_name() {
        let mut config = blank_config();
        let models = vec![
            vl_model_entry("Kimi-K2-Instruct"),
            vl_model_entry("PaddleOCR-2.0"),
        ];
        let info = run_register(&mut config, models);
        let expected = "AtomGit-PaddleOCR-2.0".to_string();
        assert_eq!(
            info.vision_preprocessor,
            VisionPreprocessorOutcome::AutoSet(expected.clone())
        );
        assert_eq!(config.vision_preprocessor_provider, Some(expected));
    }

    #[test]
    fn render_includes_vision_preprocessor_auto_set_line() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Ok(ClaimInfo { message: String::new(), duplicate: false, plan_type: PlanType::Max }),
            claim_attempts: Vec::new(),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec![
                    "Kimi-K2-Instruct".into(),
                    "Qwen/Qwen3-VL-32B-Instruct".into(),
                ],
                provider_names: vec![
                    "AtomGit-Kimi-K2-Instruct".into(),
                    "AtomGit-Qwen-Qwen3-VL-32B-Instruct".into(),
                ],
                default_provider: "AtomGit-Kimi-K2-Instruct".into(),
                vision_preprocessor: VisionPreprocessorOutcome::AutoSet(
                    "AtomGit-Qwen-Qwen3-VL-32B-Instruct".into(),
                ),
                all_models: vec![],
            }),
            status: StepResult::Skipped("status check skipped for this test".into()),
            auth_expired: false,
        };
        let out = report.render();
        assert!(
            out.contains("Vision preprocessor → AtomGit-Qwen-Qwen3-VL-32B-Instruct"),
            "render must include the auto-detected line: {out}",
        );
        assert!(out.contains("(auto-detected)"));
    }

    #[test]
    fn render_includes_vision_preprocessor_cleared_line_when_stale_dropped() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Ok(ClaimInfo { message: String::new(), duplicate: false, plan_type: PlanType::Max }),
            claim_attempts: Vec::new(),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["Kimi-K2-Instruct".into()],
                provider_names: vec!["AtomGit-Kimi-K2-Instruct".into()],
                default_provider: "AtomGit-Kimi-K2-Instruct".into(),
                vision_preprocessor: VisionPreprocessorOutcome::Cleared,
                all_models: vec![],
            }),
            status: StepResult::Skipped("test skip".into()),
            auth_expired: false,
        };
        let out = report.render();
        assert!(out.contains("Vision preprocessor cleared"));
    }

    #[test]
    fn render_includes_vision_preprocessor_user_supplied_line() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Ok(ClaimInfo { message: String::new(), duplicate: false, plan_type: PlanType::Max }),
            claim_attempts: Vec::new(),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec![
                    "Kimi-K2-Instruct".into(),
                    "Qwen/Qwen3-VL-32B-Instruct".into(),
                ],
                provider_names: vec![
                    "AtomGit-Kimi-K2-Instruct".into(),
                    "AtomGit-Qwen-Qwen3-VL-32B-Instruct".into(),
                ],
                default_provider: "AtomGit-Kimi-K2-Instruct".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UserSupplied(
                    "Qwen3-VL-32B-Instruct".into(),
                ),
                all_models: vec![],
            }),
            status: StepResult::Skipped("test skip".into()),
            auth_expired: false,
        };
        let out = report.render();
        assert!(out.contains("Vision preprocessor → Qwen3-VL-32B-Instruct"));
        assert!(out.contains("(user setting kept)"));
    }

    #[test]
    fn render_omits_vision_preprocessor_line_when_unchanged_none() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Ok(ClaimInfo { message: String::new(), duplicate: false, plan_type: PlanType::Max }),
            claim_attempts: Vec::new(),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["Kimi-K2-Instruct".into()],
                provider_names: vec!["AtomGit-Kimi-K2-Instruct".into()],
                default_provider: "AtomGit-Kimi-K2-Instruct".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,
                all_models: vec![],
            }),
            status: StepResult::Skipped("test skip".into()),
            auth_expired: false,
        };
        let out = report.render();
        assert!(!out.contains("Vision preprocessor"));
    }

    // ── rate_limit_windows rendering tests ─────────────────────────────

    fn blank_status_response() -> crate::coding_plan::types::StatusResponse {
        crate::coding_plan::types::StatusResponse {
            codingplan_free: None,
            current_usage: None,
            audit_status: 0,
            expires_at: None,
            window_quota_exhausted: false,
            window_quota_hint: None,
            rate_limit_windows: vec![],
        }
    }

    fn status_only_report(
        s: crate::coding_plan::types::StatusResponse,
    ) -> SetupReport {
        SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Skipped("already claimed".into()),
            claim_attempts: Vec::new(),
            models: StepResult::Skipped("models not exercised".into()),
            status: StepResult::Ok(s),
            auth_expired: false,
        }
    }

    #[test]
    fn render_uses_rate_limit_windows_when_present() {
        // rate_limit_windows populated → prefers new field over current_usage.
        let s = crate::coding_plan::types::StatusResponse {
            rate_limit_windows: vec![
                RateLimitWindow {
                    rule_index: 0,
                    show_enable: 1,
                    window_size_seconds: 18000,
                    window_hours: 5,
                    call_limit: 1000,
                    calls_used: 20,
                    usage_percent: 2.0,
                    quota_exhausted: false,
                    reset_at: "2026-05-26T18:09:30".into(),
                    reset_at_display: "18:09".into(),
                    seconds_until_reset: 16080,
                    reset_label: String::new(),
                    usage_status_desc: "当前时间窗口用量约 2%".into(),
                },
            ],
            ..blank_status_response()
        };
        let out = status_only_report(s).render();
        assert!(out.contains("当前时间窗口用量约 2%"), "usage_status_desc missing: {}", out);
        assert!(out.contains("18:09"), "reset_at_display missing: {}", out);
    }

    #[test]
    fn render_falls_back_to_current_usage_when_windows_empty() {
        // rate_limit_windows empty → backward-compat path using current_usage.
        let s = crate::coding_plan::types::StatusResponse {
            current_usage: Some(crate::coding_plan::types::UsageInfo {
                placeholder: false,
                window_token_limit: 0,
                window_tokens_used: 0,
                usage_percent: 5.0,
                window_hours: 5,
                reset_at: "2026-05-26T18:09:30".into(),
                reset_at_display: "18:09".into(),
                seconds_until_reset: 16080,
                reset_label: String::new(),
                usage_status_desc: "当前时间窗口用量约 5%".into(),
            }),
            rate_limit_windows: vec![], // empty → backward-compat path
            ..blank_status_response()
        };
        let out = status_only_report(s).render();
        assert!(out.contains("当前时间窗口用量约 5%"), "fallback usage missing: {}", out);
        assert!(out.contains("18:09"), "reset_at_display missing: {}", out);
    }

    #[test]
    fn render_rate_limit_window_hidden_when_show_enable_zero() {
        // show_enable=0 windows must be skipped; only show_enable=1 rendered.
        let s = crate::coding_plan::types::StatusResponse {
            rate_limit_windows: vec![
                RateLimitWindow {
                    rule_index: 0,
                    show_enable: 1,
                    window_size_seconds: 18000,
                    window_hours: 5,
                    call_limit: 1000,
                    calls_used: 50,
                    usage_percent: 5.0,
                    quota_exhausted: false,
                    reset_at: "2026-05-26T18:09:30".into(),
                    reset_at_display: "18:09".into(),
                    seconds_until_reset: 16080,
                    reset_label: String::new(),
                    usage_status_desc: "visible window".into(),
                },
                RateLimitWindow {
                    rule_index: 1,
                    show_enable: 0, // must NOT render
                    window_size_seconds: 2592000,
                    window_hours: 720,
                    call_limit: 16000,
                    calls_used: 5000,
                    usage_percent: 31.0,
                    quota_exhausted: false,
                    reset_at: "2026-06-20T23:09:30".into(),
                    reset_at_display: "23:09".into(),
                    seconds_until_reset: 2194080,
                    reset_label: String::new(),
                    usage_status_desc: "hidden window".into(),
                },
            ],
            ..blank_status_response()
        };
        let out = status_only_report(s).render();
        assert!(out.contains("visible window"), "show_enable=1 window missing: {}", out);
        assert!(!out.contains("hidden window"), "show_enable=0 window must not render: {}", out);
        assert!(!out.contains("23:09"), "hidden window reset_at must not appear: {}", out);
    }

    /// Locked models (plan_available=false on a higher tier) must
    /// surface in the rendered report with a distinctive `×` prefix
    /// + the explicit "(requires Pro plan or higher)" suffix, the whole row
    /// wrapped in SGR 31 (terminal-theme red), and appended to the
    /// same `Added N provider(s)` bullet list as the available models
    /// so users see the full slate at a glance. Pins the v2 spec's
    /// "若不可用的模型也展示出来" requirement.
    ///
    /// Three layered signals — colour, prefix glyph, suffix text —
    /// because each can fail independently:
    ///   * SGR 31 only fires when the renderer's sanitizer keeps SGR
    ///     (plain) — retained's strict strip pathway drops the colour
    ///     but the glyph + text still carry the meaning.
    ///   * The `×` glyph (U+00D7 MULTIPLICATION SIGN) is Latin-1 and
    ///     present in every terminal font (unlike U+2717 ✗ which is
    ///     missing from macOS Terminal.app's default font).
    ///   * The "(requires Pro plan or higher)" suffix is plain ASCII / CJK
    ///     and survives even font-fallback-tofu rendering.
    ///
    /// Earlier attempts at strikethrough (SGR 9 then U+0336
    /// combining mark) were both dropped — SGR 9 was eaten by the
    /// universal CSI sanitizer, and U+0336 was silently skipped by
    /// some fonts in the wild — so this test also pins that those
    /// markers do NOT regress back into the template.
    #[test]
    fn render_shows_locked_models_with_prefix_marker() {
        let avail = super::super::types::ModelEntry {
            id: 1,
            display_model_name: "lite/foo".into(),
            plan_available: true,
            ..Default::default()
        };
        let locked = super::super::types::ModelEntry {
            id: 2,
            display_model_name: "max/super-secret".into(),
            plan_available: false,
            ..Default::default()
        };
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Ok(ClaimInfo {
                message: "claimed".into(),
                duplicate: false,
                plan_type: PlanType::Lite,
            }),
            claim_attempts: Vec::new(),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["lite/foo".into()],
                provider_names: vec!["AtomGit".into()],
                default_provider: "AtomGit".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,
                all_models: vec![avail, locked],
            }),
            status: StepResult::Skipped("test skip".into()),
            auth_expired: false,
        };
        let out = report.render();
        // Plan tier appears next to claim line.
        assert!(out.contains("(CodingPlan Lite)"), "claim row must show tier:\n{out}");
        // Available model: standard provider line.
        assert!(out.contains("AtomGit") && out.contains("lite/foo"));
        // Locked model: `×` prefix immediately before the name, plus
        // the explicit `(requires Pro plan or higher)` suffix, all wrapped
        // in SGR 31 (red fg) → SGR 39 (default fg) so the terminal
        // renders the whole row in the theme's red.
        assert!(
            out.contains("\x1b[31m× max/super-secret"),
            "locked model must open with SGR 31 + × prefix:\n{out}"
        );
        assert!(out.contains("(requires Pro plan or higher)\x1b[39m"));
        // Strikethrough is intentionally NOT used (SGR 9 was eaten by
        // the renderer's CSI sanitizer; U+0336 was font-dependent and
        // silently dropped on some setups). Lock those decisions in.
        assert!(
            !out.contains("\x1b[9m"),
            "locked-model line must not emit SGR 9 strikethrough:\n{out}"
        );
        assert!(
            !out.contains('\u{0336}'),
            "locked-model line must not emit U+0336 combining strikethrough:\n{out}"
        );
        // Locked model appears INSIDE the providers bullet list — its
        // line must come after the "Added N provider(s):" header and
        // before the next top-level section (Vision preprocessor /
        // CodingPlan status). The strikethrough + suffix already mark
        // it as unavailable; no separate "locked model" header.
        assert!(
            !out.contains("locked model"),
            "no separate locked-model section expected:\n{out}"
        );
        let added_idx = out.find("Added 1 provider").expect("Added header");
        let locked_idx = out.find("max/super-secret").expect("locked model line");
        let avail_idx = out.find("lite/foo").expect("available model line");
        assert!(
            locked_idx > added_idx,
            "locked model must render after the Added header:\n{out}"
        );
        assert!(
            locked_idx < avail_idx,
            "locked model must render BEFORE available providers (top-of-list upgrade prompt):\n{out}"
        );
    }

}
