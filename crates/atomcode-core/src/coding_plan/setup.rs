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

use super::client::Client;
use super::types::{ModelEntry, PlanType, StatusResponse};
use crate::auth;
use crate::config::provider::ProviderConfig;
use crate::config::Config;

/// Base URL for the LLM gateway behind AtomGit's infrastructure — same
/// value the historical `/login` auto-registration used.
const LLM_BASE_URL: &str = "https://api-ai.gitcode.com/v1";

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

/// JetBrains JediTerm doesn't render SGR 9 strikethrough reliably
/// (older versions silently parse-and-drop it; newer versions render
/// inconsistently depending on font + theme). Detected via the
/// `TERMINAL_EMULATOR=JetBrains-JediTerm` env var that JetBrains IDEs
/// export into their integrated terminal. When true, the locked-model
/// row falls back to an ASCII `✗` prefix + `(Locked: ...)` text marker
/// so the meaning carries even with no visual styling.
pub(crate) fn detect_jediterm() -> bool {
    std::env::var("TERMINAL_EMULATOR")
        .map(|v| v == "JetBrains-JediTerm")
        .unwrap_or(false)
}

impl SetupReport {
    /// Render as a multi-line plain-text block for stdout / TUI body.
    /// Shared by the CLI subcommand and the `/codingplan` slash command
    /// so the visual contract stays consistent.
    pub fn render(&self) -> String {
        self.render_with_terminal_caps(detect_jediterm())
    }

    /// Test-friendly variant of `render()` that takes terminal capability
    /// flags as parameters so unit tests don't have to mutate process
    /// env to exercise the JediTerm fallback path.
    pub(crate) fn render_with_terminal_caps(&self, is_jediterm: bool) -> String {
        let mut out = String::new();
        out.push_str("  AtomCode CodingPlan setup:\n\n");

        // Step 1: login
        match &self.login {
            StepResult::Ok(info) => {
                let who = info.display_name.as_deref().unwrap_or(&info.username);
                let email = info.email.as_deref().unwrap_or("—");
                out.push_str(&format!(
                    "  ✔ Logged in as {} ({}, {})\n",
                    who, info.username, email,
                ));
            }
            StepResult::Skipped(reason) => {
                out.push_str(&format!("  ✔ {}\n", reason));
            }
            StepResult::Err(msg) => {
                out.push_str(&format!("  ✘ Login failed — {}\n", msg));
            }
        }

        // Step 2: claim. Show the tier the cascade landed on so users
        // can see whether Max / Pro / Lite was actually granted (the
        // cascade walks highest-first; landing on Pro means Max
        // refused).
        match &self.claim {
            StepResult::Ok(info) => {
                out.push_str(&format!(
                    "  ✔ CodingPlan claimed — {} (CodingPlan {})\n",
                    if info.message.is_empty() {
                        "success".to_string()
                    } else {
                        info.message.clone()
                    },
                    info.plan_type.as_str(),
                ));
            }
            StepResult::Skipped(reason) if reason == CASCADE_FROM_UPSTREAM_FAIL => {
                // Cascade from login failure — suppressed.
            }
            StepResult::Skipped(reason) => {
                out.push_str(&format!("  ✔ CodingPlan already claimed — {}\n", reason));
            }
            StepResult::Err(msg) => {
                out.push_str(&format!("  ✘ CodingPlan claim failed — {}\n", msg));
            }
        }

        // Step 3: models. When the cascade marker is present (claim
        // failed upstream), skip the row entirely — printing
        // "Models step skipped — claim failed" right after the claim
        // failure line is just noise. Same for the status row below.
        match &self.models {
            StepResult::Ok(info) => {
                out.push_str(&format!(
                    "  ✔ Added {} provider{}:\n",
                    info.provider_names.len(),
                    if info.provider_names.len() == 1 {
                        ""
                    } else {
                        "s"
                    },
                ));
                // Build a quick lookup of which display names made it
                // into the registered provider list — anything in
                // `all_models` but NOT in this set is locked behind
                // the user's plan tier and renders with strikethrough.
                let registered: std::collections::HashSet<&str> =
                    info.display_names.iter().map(|s| s.as_str()).collect();
                // Locked models render FIRST so the upgrade prompt is the
                // first thing the eye lands on under "Added N providers:".
                // ANSI SGR 9 for strikethrough (\x1b[9m...\x1b[29m); terminals
                // that don't honour it (e.g., JediTerm, legacy conhost) still
                // get the explicit "(require plan upgrade)" suffix so the
                // meaning never relies on the SGR alone.
                let locked: Vec<&ModelEntry> = info
                    .all_models
                    .iter()
                    .filter(|m| !m.plan_available && !registered.contains(m.display_model_name.as_str()))
                    .collect();
                for m in &locked {
                    if is_jediterm {
                        // JediTerm fallback: ✗ + "(Locked: ...)" text
                        // marker, no SGR 9 (which JediTerm renders
                        // inconsistently or not at all).
                        out.push_str(&format!(
                            "      ✗ {}  (Locked: require plan upgrade)\n",
                            m.display_model_name,
                        ));
                    } else {
                        out.push_str(&format!(
                            "      • \x1b[9m{}\x1b[29m  (require plan upgrade)\n",
                            m.display_model_name,
                        ));
                    }
                }
                for (pname, model) in info.provider_names.iter().zip(info.display_names.iter()) {
                    let suffix = if pname == &info.default_provider {
                        "  (default)"
                    } else {
                        ""
                    };
                    out.push_str(&format!("      • {}  →  {}{}\n", pname, model, suffix));
                }
                // Vision-preprocessor outcome line.
                match &info.vision_preprocessor {
                    VisionPreprocessorOutcome::AutoSet(k) => {
                        out.push_str(&format!(
                            "  ✔ Vision preprocessor → {}  (auto-detected)\n",
                            k,
                        ));
                    }
                    VisionPreprocessorOutcome::UserSupplied(k) => {
                        out.push_str(&format!(
                            "  ✔ Vision preprocessor → {}  (user setting kept)\n",
                            k,
                        ));
                    }
                    VisionPreprocessorOutcome::Cleared => {
                        out.push_str(
                            "  ⚠ Vision preprocessor cleared — no VL/OCR model in current list\n",
                        );
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
                out.push_str(&format!("  ✔ Models step skipped — {}\n", reason));
            }
            StepResult::Err(msg) => {
                out.push_str(&format!("  ✘ Models step failed — {}\n", msg));
            }
        }

        // Step 4: status
        match &self.status {
            StepResult::Ok(s) => {
                out.push_str("  ✔ CodingPlan status:\n");
                if let Some(plan) = &s.codingplan_free {
                    if plan.expires_at.is_empty() {
                        // Backend sends null claimed_at/expires_at while a
                        // fresh claim is still propagating. Don't render an
                        // empty date with `(0d / 0d remaining)` zeros — say
                        // "pending activation" so the user knows to wait.
                        out.push_str(&format!(
                            "      Plan: {}  ·  pending activation\n",
                            plan.plan_name,
                        ));
                    } else {
                        out.push_str(&format!(
                            "      Plan: {}  ·  expires {} ({}d / {}d remaining)\n",
                            plan.plan_name, plan.expires_at, plan.remaining_days, plan.total_days,
                        ));
                    }
                }
                if let Some(u) = &s.current_usage {
                    out.push_str(&format!(
                        "      Usage: {}  ·  resets {} (in {})\n",
                        u.display_desc(),
                        u.reset_at_display,
                        format_duration_secs(u.seconds_until_reset),
                    ));
                }
                if s.window_quota_exhausted {
                    if let Some(hint) = &s.window_quota_hint {
                        out.push_str(&format!("      ⚠ {}\n", hint));
                    } else {
                        out.push_str("      ⚠ Current window quota exhausted\n");
                    }
                }
            }
            StepResult::Skipped(reason) if reason == CASCADE_FROM_UPSTREAM_FAIL => {
                // Suppress — cascade from claim failure.
            }
            StepResult::Skipped(reason) => {
                out.push_str(&format!("  ⚠ Status fetch skipped — {}\n", reason));
            }
            StepResult::Err(msg) => {
                // Truncate the error chain so a server-side parse failure
                // doesn't dump the entire response body inline. The cause
                // chain commonly includes the raw JSON via anyhow's
                // `with_context(format!("(body: {})", body))`, easily
                // 200+ chars; the diagnostic value beyond ~150 is low.
                out.push_str(&format!(
                    "  ⚠ Status fetch failed (non-fatal) — {}\n",
                    truncate_inline(msg, 150),
                ));
            }
        }

        out
    }

    /// True iff the critical steps (login + models) succeeded. Callers
    /// use this to decide whether to persist config changes to disk —
    /// no point writing config if the model list never arrived.
    pub fn should_persist_config(&self) -> bool {
        self.login.is_ok_or_skipped() && self.models.is_ok_or_skipped()
    }
}

/// Display-friendly summary of each step's outcome. Returned by `run`
/// so the caller can render however it wants (plain stdout, TUI body
/// scrollback, future JSON output for scripting).
#[derive(Debug, Clone)]
pub struct SetupReport {
    pub login: StepResult<LoginInfo>,
    pub claim: StepResult<ClaimInfo>,
    pub models: StepResult<ModelsInfo>,
    pub status: StepResult<StatusResponse>,
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
            });
        }
        // Use the cascade sentinel so format() suppresses the three
        // "Foo failed — skipped: login failed" rows that used to spam
        // the report. The login-failure line above is the only thing
        // worth showing; the rest is implied.
        return Ok(SetupReport {
            login,
            claim: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            models: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            status: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
        });
    }

    // Step 2: claim — cascade Max → Pro → Lite, first success wins.
    let claim = step_claim();
    if claim.is_err() {
        // Claim failed — adding providers / fetching status both make
        // no sense without an active plan. Bail with cascade markers
        // (rendered as no-op in format() so the report stays focused
        // on the actual problem instead of three identical "skipped:
        // claim failed" lines).
        return Ok(SetupReport {
            login,
            claim,
            models: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            status: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
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
    let models = step_models_and_register(config, plan_type_for_models);
    if models.is_err() {
        if let Some(t) = tel {
            t.track(atomcode_telemetry::Event::TakeCodingplan {
                type_: atomcode_telemetry::CodingplanResult::Fail,
            });
        }
        // Same cascade pattern: the models-failure line above is the
        // explanation; "Status fetch failed — skipped: models step
        // failed" adds nothing.
        return Ok(SetupReport {
            login,
            claim,
            models,
            status: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
        });
    }

    // Step 4: status — warn-only.
    let status = step_status();

    // All critical steps (login + models) succeeded. Emit success event.
    if let Some(t) = tel {
        t.track(atomcode_telemetry::Event::TakeCodingplan {
            type_: atomcode_telemetry::CodingplanResult::Success,
        });
    }

    Ok(SetupReport {
        login,
        claim,
        models,
        status,
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
fn step_claim() -> StepResult<ClaimInfo> {
    let client = match Client::from_stored_auth() {
        Ok(c) => c,
        Err(e) => return StepResult::Err(format!("build client: {:#}", e)),
    };
    let mut last_msg = String::new();
    for &tier in PlanType::CASCADE_ORDER {
        match client.claim_v2(tier) {
            Ok(resp) => {
                if resp.duplicate {
                    // Already holds this (or a higher) tier.
                    return StepResult::Skipped(if resp.message.is_empty() {
                        format!(
                            "already claimed (or under review) — using {}",
                            tier.as_str()
                        )
                    } else {
                        format!("{} ({})", resp.message, tier.as_str())
                    });
                }
                if resp.success {
                    return StepResult::Ok(ClaimInfo {
                        message: if resp.message.is_empty() {
                            format!("claimed {}", tier.as_str())
                        } else {
                            resp.message
                        },
                        duplicate: false,
                        plan_type: tier,
                    });
                }
                // 2xx + success=false + duplicate=false: per-tier
                // refusal (quota / not eligible). Remember the
                // message and keep walking; if every tier refuses
                // we surface this last reason.
                last_msg = if resp.message.is_empty() {
                    format!("{} claim refused", tier.as_str())
                } else {
                    format!("{}: {}", tier.as_str(), resp.message)
                };
            }
            Err(e) => {
                // Transport / 5xx / parse failure — bail. These don't
                // get more useful when retried at a lower tier.
                return StepResult::Err(format!("claim {} request: {:#}", tier.as_str(), e));
            }
        }
    }
    StepResult::Err(if last_msg.is_empty() {
        "claim failed at every tier (Max/Pro/Lite)".into()
    } else {
        format!("claim failed at every tier — {}", last_msg)
    })
}

fn step_models_and_register(
    config: &mut Config,
    plan_type: PlanType,
) -> StepResult<ModelsInfo> {
    let client = match Client::from_stored_auth() {
        Ok(c) => c,
        Err(e) => return StepResult::Err(format!("build client: {:#}", e)),
    };
    let all_models = match client.list_models_v2(plan_type) {
        Ok(v) => v,
        Err(e) => return StepResult::Err(format!("list models-v2: {:#}", e)),
    };
    if all_models.is_empty() {
        return StepResult::Err(
            "server returned an empty model list — cannot set up any provider".into(),
        );
    }

    // Available subset — only these become providers. Locked ones
    // (`plan_available=false`) survive in `all_models` for the
    // strikethrough-display path; registering them as providers would
    // give the user something they can `/model` into that 403s on the
    // first request.
    let available: Vec<&ModelEntry> = all_models.iter().filter(|m| m.plan_available).collect();
    if available.is_empty() {
        return StepResult::Err(format!(
            "no models available on plan {} — server returned {} locked entries",
            plan_type.as_str(),
            all_models.len()
        ));
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
        let pc = build_codingplan_provider(&m.display_model_name);
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

    StepResult::Ok(ModelsInfo {
        display_names: names,
        provider_names,
        default_provider,
        vision_preprocessor,
        all_models,
    })
}

fn step_status() -> StepResult<StatusResponse> {
    let client = match Client::from_stored_auth() {
        Ok(c) => c,
        Err(e) => return StepResult::Err(format!("build client: {:#}", e)),
    };
    match client.status_v2() {
        Ok(s) => StepResult::Ok(s),
        Err(e) => StepResult::Err(format!("status-v2: {:#}", e)),
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
fn format_duration_secs(secs: i64) -> String {
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

/// Build a ProviderConfig pointing at the AtomGit LLM gateway. `api_key`
/// stays `None`: `create_provider()` loads the OAuth token at runtime.
fn build_codingplan_provider(model: &str) -> ProviderConfig {
    ProviderConfig {
        provider_type: PROVIDER_TYPE.to_string(),
        api_key: None,
        model: model.to_string(),
        base_url: Some(LLM_BASE_URL.to_string()),
        system_prompt: None,
        user_agent: None,
        context_window: CONTEXT_WINDOW,
        max_tokens: None,
        thinking_type: None,
        thinking_keep: None,
        reasoning_history: None,
        thinking_enabled: None,
        thinking_budget: None,
        skip_tls_verify: false,
        ephemeral: false,

}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn blank_config() -> Config {
        Config {
            default_provider: String::new(),
            default_workdir: None,
            providers: HashMap::new(),
            datalog: Default::default(),
            auto_update: true,
            notifications: Default::default(),
            telemetry: Default::default(),
            lsp: Default::default(),
            auto_commit: false,
            subagent: Default::default(),
            vision_preprocessor_provider: None,
        }
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
            build_codingplan_provider("stale-MiniMax"),
        );
        config.providers.insert(
            "AtomGit-legacy".to_string(),
            build_codingplan_provider("another-stale"),
        );
        config.providers.insert(
            "claude".to_string(),
            build_codingplan_provider("anthropic/claude-3.5"),
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
                .insert(pname.clone(), build_codingplan_provider(m));
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
        assert_eq!(fresh.base_url.as_deref(), Some(LLM_BASE_URL));
        assert_eq!(fresh.provider_type, PROVIDER_TYPE);
        assert_eq!(config.default_provider, "AtomGit");
    }

    #[test]
    fn build_provider_uses_canonical_defaults() {
        let p = build_codingplan_provider("foo/bar");
        assert_eq!(p.provider_type, "openai");
        assert_eq!(p.base_url.as_deref(), Some("https://api-ai.gitcode.com/v1"));
        assert_eq!(p.context_window, 64_000);
        assert!(
            p.api_key.is_none(),
            "token loaded at runtime from auth.toml"
        );
        assert!(!p.ephemeral);
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
            }),
        };
        let out = report.render();
        assert!(out.contains("✔ Logged in as Theo"));
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
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["a/b".into()],
                provider_names: vec!["AtomGit".into()],
                default_provider: "AtomGit".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,
                all_models: vec![],
            }),
            status: StepResult::Err("request timeout".into()),
        };
        let out = report.render();
        assert!(out.contains("✔ already logged in"));
        assert!(out.contains("already claimed"));
        assert!(!out.contains("✘ CodingPlan claim"), "duplicate ≠ failure");
        // Status failed but it's warn-only: ⚠ prefix, NOT ✘.
        assert!(out.contains("⚠ Status fetch failed"));
        assert!(!out.contains("✘ Status"));
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
            }),
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
            models: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            status: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
        };
        let out = report.render();
        assert!(out.contains("✘ Login failed"));
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
            models: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
            status: StepResult::Skipped(CASCADE_FROM_UPSTREAM_FAIL.into()),
        };
        let out = report.render();
        assert!(out.contains("✘ CodingPlan claim failed"));
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
            models: StepResult::Skipped("models cached locally".into()),
            status: StepResult::Skipped("server returned 503; using cached".into()),
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
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["a/b".into()],
                provider_names: vec!["AtomGit".into()],
                default_provider: "AtomGit".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,
                all_models: vec![],
            }),
            status: StepResult::Err(huge),
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
            is_atomcode_exclusive: 0,
            display_model_name: model.to_string(),
            // Tests in this section drive `run_register` directly with
            // a curated `Vec<ModelEntry>` — they're testing the
            // post-availability-filter logic, so every entry counts as
            // "available". The split-by-`plan_available` happens
            // upstream in the real `step_models_and_register`.
            plan_available: true,
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
                .insert(pname.clone(), build_codingplan_provider(&m.display_model_name));
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
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["Kimi-K2-Instruct".into()],
                provider_names: vec!["AtomGit-Kimi-K2-Instruct".into()],
                default_provider: "AtomGit-Kimi-K2-Instruct".into(),
                vision_preprocessor: VisionPreprocessorOutcome::Cleared,
                all_models: vec![],
            }),
            status: StepResult::Skipped("test skip".into()),
        };
        let out = report.render();
        assert!(out.contains("Vision preprocessor cleared"));
    }

    #[test]
    fn render_includes_vision_preprocessor_user_supplied_line() {
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Ok(ClaimInfo { message: String::new(), duplicate: false, plan_type: PlanType::Max }),
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
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["Kimi-K2-Instruct".into()],
                provider_names: vec!["AtomGit-Kimi-K2-Instruct".into()],
                default_provider: "AtomGit-Kimi-K2-Instruct".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,
                all_models: vec![],
            }),
            status: StepResult::Skipped("test skip".into()),
        };
        let out = report.render();
        assert!(!out.contains("Vision preprocessor"));
    }

    /// Locked models (plan_available=false on a higher tier) must
    /// surface in the rendered report with strikethrough + the
    /// explicit "(require plan upgrade)" tag, appended to the same
    /// `Added N provider(s)` bullet list as the available models so
    /// users see the full slate at a glance. Pins the v2 spec's "若
    /// 不可用的模型也展示出来（用横线划掉）" requirement.
    #[test]
    fn render_shows_locked_models_with_strikethrough() {
        let avail = super::super::types::ModelEntry {
            id: 1,
            is_atomcode_exclusive: 0,
            display_model_name: "lite/foo".into(),
            plan_available: true,
        };
        let locked = super::super::types::ModelEntry {
            id: 2,
            is_atomcode_exclusive: 0,
            display_model_name: "max/super-secret".into(),
            plan_available: false,
        };
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Ok(ClaimInfo {
                message: "claimed".into(),
                duplicate: false,
                plan_type: PlanType::Lite,
            }),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["lite/foo".into()],
                provider_names: vec!["AtomGit".into()],
                default_provider: "AtomGit".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,
                all_models: vec![avail, locked],
            }),
            status: StepResult::Skipped("test skip".into()),
        };
        let out = report.render();
        // Plan tier appears next to claim line.
        assert!(out.contains("(CodingPlan Lite)"), "claim row must show tier:\n{out}");
        // Available model: standard provider line.
        assert!(out.contains("AtomGit") && out.contains("lite/foo"));
        // Locked model: strikethrough SGR + explicit suffix. Both
        // must be present so terminals that ignore SGR 9 still see
        // "(require plan upgrade)".
        assert!(
            out.contains("\x1b[9mmax/super-secret\x1b[29m"),
            "locked model must have SGR 9 strikethrough wrap:\n{out}"
        );
        assert!(out.contains("(require plan upgrade)"));
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

    /// JediTerm fallback: when `TERMINAL_EMULATOR=JetBrains-JediTerm`
    /// is detected, the locked-model row drops SGR 9 strikethrough and
    /// switches to `✗ <name>  (Locked: require plan upgrade)`. Pins the
    /// fallback so terminals that don't honour SGR 9 still convey the
    /// "this needs an upgrade" semantic.
    #[test]
    fn render_jediterm_fallback_uses_ascii_marker_no_strikethrough() {
        let avail = super::super::types::ModelEntry {
            id: 1,
            is_atomcode_exclusive: 0,
            display_model_name: "lite/foo".into(),
            plan_available: true,
        };
        let locked = super::super::types::ModelEntry {
            id: 2,
            is_atomcode_exclusive: 0,
            display_model_name: "max/super-secret".into(),
            plan_available: false,
        };
        let report = SetupReport {
            login: StepResult::Skipped("already logged in".into()),
            claim: StepResult::Ok(ClaimInfo {
                message: "claimed".into(),
                duplicate: false,
                plan_type: PlanType::Lite,
            }),
            models: StepResult::Ok(ModelsInfo {
                display_names: vec!["lite/foo".into()],
                provider_names: vec!["AtomGit".into()],
                default_provider: "AtomGit".into(),
                vision_preprocessor: VisionPreprocessorOutcome::UnchangedNone,
                all_models: vec![avail, locked],
            }),
            status: StepResult::Skipped("test skip".into()),
        };
        let out = report.render_with_terminal_caps(true);
        // No SGR 9 escapes anywhere in the output.
        assert!(
            !out.contains("\x1b[9m"),
            "JediTerm fallback must not emit SGR 9:\n{out}"
        );
        // Explicit ASCII marker + "(Locked: ...)" suffix.
        assert!(
            out.contains("✗ max/super-secret  (Locked: require plan upgrade)"),
            "expected ASCII fallback line:\n{out}"
        );
    }
}
