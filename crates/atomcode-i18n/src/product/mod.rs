//! The product's own sentences: everything the CLI, the daemon, the setup flow
//! and the previous front end say to a person.
//!
//! The screen's sentences are next door in [`crate::screen`]. The split is by
//! **who says it**, not by language: both tables carry both languages, and both
//! read the one locale in [`crate::runtime`].

mod en;
mod messages;
mod zh_cn;

pub use crate::locale::Locale;
pub use messages::Msg;

// The shared runtime, re-exported so `atomcode_config::i18n::{set_locale, …}`
// keeps naming what it always named. Callers outside this crate reach these
// through whichever table they already use; there is only one of each.
pub use crate::runtime::{
    current_locale, fmt_tokens, resolve_initial_locale, resolve_initial_locale_with_env, set_brand,
    set_locale, substitute_placeholders, test_lock, LocaleTestGuard,
};

use std::borrow::Cow;

/// Translate a message using the current global locale.
///
/// Returns a `Cow<'static, str>` — static for literal translations,
/// owned for interpolated ones.
pub fn t(msg: Msg<'_>) -> Cow<'static, str> {
    t_with(current_locale(), msg)
}

/// Look up against an explicit locale.
pub fn t_with(locale: Locale, msg: Msg<'_>) -> Cow<'static, str> {
    let raw = match locale {
        Locale::En => en::en(msg),
        Locale::ZhCn => zh_cn::zh_cn(msg),
    };
    substitute_placeholders(raw)
}

/// Format the localized marker shown after a committed compaction.
pub fn format_compaction_mark(
    removed_messages: usize,
    estimated_tokens_before: usize,
    estimated_tokens_after: usize,
) -> String {
    if removed_messages > 0 {
        let before = fmt_compaction_tokens(estimated_tokens_before);
        let after = fmt_compaction_tokens(estimated_tokens_after);
        t(Msg::CompactMarkDrain {
            messages: removed_messages,
            before: &before,
            after: &after,
        })
        .into_owned()
    } else {
        let saved =
            fmt_compaction_tokens(estimated_tokens_before.saturating_sub(estimated_tokens_after));
        t(Msg::CompactMarkStub { saved: &saved }).into_owned()
    }
}

/// Format the localized acknowledgement for a MANUAL `/compact` that committed
/// but only shaved a negligible amount of context (a trivial stub fold). Reads
/// as a clean "nothing to do" so it doesn't look like a "success" the way the
/// stub mark does, and avoids the misleading "conversation is short" wording.
pub fn format_compaction_negligible() -> String {
    t(Msg::CompactNegligibleSavings).into_owned()
}

/// Format the localized acknowledgement for a user-requested compaction that
/// left the conversation unchanged.
pub fn format_compaction_noop(
    estimated_tokens_before: usize,
    estimated_tokens_after: usize,
    summary_would_grow: bool,
) -> String {
    if summary_would_grow {
        let before = fmt_compaction_tokens(estimated_tokens_before);
        let after = fmt_compaction_tokens(estimated_tokens_after);
        t(Msg::CompactNothingNoSavings {
            before: &before,
            after: &after,
        })
        .into_owned()
    } else {
        t(Msg::CompactNothingShort).into_owned()
    }
}

/// Format the localized acknowledgement for an accepted compaction that was
/// interrupted by runtime replacement or shutdown.
pub fn format_compaction_interrupted() -> String {
    t(Msg::CompactInterrupted).into_owned()
}

fn fmt_compaction_tokens(tokens: usize) -> String {
    if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t_with_returns_english_for_en() {
        let s = t_with(Locale::En, Msg::WelcomeBannerLine1);
        // Placeholder must be replaced; the settled brand (or "AtomCode"
        // default) is already substituted by `substitute_placeholders`.
        assert!(s.starts_with("Welcome to "), "en render: {s}");
        assert!(!s.contains("{brand}"), "en leaked placeholder: {s}");
    }

    #[test]
    fn t_with_returns_chinese_for_zh_cn() {
        let s = t_with(Locale::ZhCn, Msg::WelcomeBannerLine1);
        assert!(s.starts_with("欢迎使用 "), "zh render: {s}");
        assert!(!s.contains("{brand}"), "zh leaked placeholder: {s}");
    }

    #[test]
    fn set_locale_flips_global() {
        let _g = test_lock();
        set_locale(Locale::ZhCn);
        assert_eq!(current_locale(), Locale::ZhCn);
        let s = t(Msg::WelcomeBannerLine1);
        assert!(s.starts_with("欢迎使用"));
        assert!(!s.contains("{brand}"), "zh leaked placeholder: {s}");

        set_locale(Locale::En);
        assert_eq!(current_locale(), Locale::En);
        let s = t(Msg::WelcomeBannerLine1);
        assert!(s.starts_with("Welcome to "), "en render: {s}");
        assert!(!s.contains("{brand}"), "en leaked placeholder: {s}");
    }

    #[test]
    fn err_unsupported_locale_includes_input() {
        let s = t_with(Locale::En, Msg::ErrUnsupportedLocale { input: "fr" });
        assert!(s.contains("fr"));
        let s = t_with(Locale::ZhCn, Msg::ErrUnsupportedLocale { input: "fr" });
        assert!(s.contains("fr"));
    }

    fn has_cjk(s: &str) -> bool {
        s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
    }

    #[test]
    fn turn_summary_appends_cached_pct_only_when_present() {
        let with = t_with(
            Locale::En,
            Msg::TurnSummary {
                done: "Dialed in",
                turn_count: 30,
                tool_call_count: 32,
                duration: "435.3s",
                total_tokens: 152_000,
                cached_pct: Some(97),
            },
        );
        assert!(with.contains("152.00K tokens · 97% cached"), "got: {with}");
        let without = t_with(
            Locale::En,
            Msg::TurnSummary {
                done: "Dialed in",
                turn_count: 30,
                tool_call_count: 32,
                duration: "435.3s",
                total_tokens: 152_000,
                cached_pct: None,
            },
        );
        assert!(
            without.trim_end().ends_with("152.00K tokens"),
            "got: {without}"
        );
        assert!(
            !without.contains("cached"),
            "no annotation when None: {without}"
        );
    }

    #[test]
    fn gateway_auth_unavailable_is_localized_and_keeps_url() {
        let url = "https://llm-api.atomgit.com/v1";
        let en = t_with(Locale::En, Msg::GatewayAuthUnavailable { base_url: url });
        assert!(en.contains(url), "EN must echo the base_url: {en}");
        assert!(en.to_lowercase().contains("gateway"), "EN keyword: {en}");
        let zh = t_with(Locale::ZhCn, Msg::GatewayAuthUnavailable { base_url: url });
        assert!(zh.contains(url), "ZH must echo the base_url: {zh}");
        assert!(has_cjk(&zh), "ZH must actually be Chinese: {zh}");
    }

    #[test]
    fn provider_init_frame_keeps_detail_both_locales() {
        let en = t_with(Locale::En, Msg::ProviderInitFailed { detail: "DETAIL_X" });
        assert!(en.contains("DETAIL_X"));
        let zh = t_with(Locale::ZhCn, Msg::ProviderInitFailed { detail: "DETAIL_X" });
        assert!(zh.contains("DETAIL_X"));
        assert!(has_cjk(&zh), "ZH frame must be Chinese: {zh}");
    }

    #[test]
    fn plugin_manager_empty_hints_advertise_esc() {
        // Regression: every plugin-manager screen advertises Esc-to-go-back in
        // its hint, EXCEPT these empty-state hints once did not — so an empty
        // list (e.g. /plugin → Installed with 0 plugins) looked frozen with no
        // visible way out. Keep the Esc affordance on the empty states too.
        fn has_esc(s: &str) -> bool {
            s.to_lowercase().contains("esc")
        }
        for (en, zh) in [
            (
                t_with(Locale::En, Msg::PluginMgrEmptyInstalled),
                t_with(Locale::ZhCn, Msg::PluginMgrEmptyInstalled),
            ),
            (
                t_with(Locale::En, Msg::PluginMgrEmptyMarketplaces),
                t_with(Locale::ZhCn, Msg::PluginMgrEmptyMarketplaces),
            ),
            (
                t_with(Locale::En, Msg::PluginMgrEmptyPlugins),
                t_with(Locale::ZhCn, Msg::PluginMgrEmptyPlugins),
            ),
        ] {
            assert!(has_esc(&en), "EN empty hint missing esc: {en}");
            assert!(has_esc(&zh), "ZH empty hint missing esc: {zh}");
        }
    }

    #[test]
    fn cli_flag_wins_over_everything() {
        let env = |_: &str| Some("zh_CN.UTF-8".to_string());
        assert_eq!(
            resolve_initial_locale_with_env(Some("en"), Some(Locale::ZhCn), &env),
            Locale::En
        );
    }

    #[test]
    fn config_beats_env() {
        let env = |_: &str| Some("zh_CN.UTF-8".to_string());
        assert_eq!(
            resolve_initial_locale_with_env(None, Some(Locale::En), &env),
            Locale::En
        );
    }

    #[test]
    fn env_zh_cn_resolves_to_zh_cn() {
        let env = |k: &str| {
            if k == "LANG" {
                Some("zh_CN.UTF-8".into())
            } else {
                None
            }
        };
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &env),
            Locale::ZhCn
        );
    }

    #[test]
    fn env_zh_tw_maps_to_zh_cn() {
        let env = |k: &str| {
            if k == "LANG" {
                Some("zh_TW".into())
            } else {
                None
            }
        };
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &env),
            Locale::ZhCn
        );
    }

    #[test]
    fn env_c_or_english_resolves_to_en() {
        let mk = |val: &'static str| {
            move |k: &str| {
                if k == "LANG" {
                    Some(val.to_string())
                } else {
                    None
                }
            }
        };
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &mk("C")),
            Locale::En
        );
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &mk("en_US.UTF-8")),
            Locale::En
        );
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &mk("")),
            Locale::En
        );
    }

    #[test]
    fn env_no_locale_vars_resolves_to_en() {
        let env = |_: &str| None;
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &env),
            Locale::En
        );
    }

    #[test]
    fn lc_all_overrides_lc_messages_and_lang() {
        let env = |k: &str| match k {
            "LC_ALL" => Some("zh_CN.UTF-8".into()),
            "LANG" => Some("en_US.UTF-8".into()),
            _ => None,
        };
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &env),
            Locale::ZhCn
        );
    }

    #[test]
    fn lc_messages_overrides_lang() {
        let env = |k: &str| match k {
            "LC_MESSAGES" => Some("zh_CN.UTF-8".into()),
            "LANG" => Some("en_US.UTF-8".into()),
            _ => None,
        };
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &env),
            Locale::ZhCn
        );
    }

    #[test]
    fn compact_mark_drain_renders_numbers_and_arrow() {
        // Locale-invariant assertion (numbers + the → arrow appear in both en & zh).
        let s = crate::product::t(crate::product::Msg::CompactMarkDrain {
            messages: 12,
            before: "48.2K",
            after: "9.1K",
        });
        assert!(s.contains("12"), "message count missing: {s}");
        assert!(
            s.contains("48.2K") && s.contains("9.1K"),
            "token figures missing: {s}"
        );
        assert!(s.contains('→'), "before→after arrow missing: {s}");
        assert!(s.contains('~'), "estimate marker missing: {s}");
        assert!(s.contains("tok"), "token unit missing: {s}");
    }

    #[test]
    fn compact_mark_stub_renders_saved_without_arrow() {
        let s = crate::product::t(crate::product::Msg::CompactMarkStub { saved: "6.0K" });
        assert!(s.contains("6.0K"), "saved figure missing: {s}");
        assert!(
            !s.contains('→'),
            "stub marker shows a single figure, no arrow: {s}"
        );
        assert!(s.contains("tok"), "token unit missing: {s}");
    }

    #[test]
    fn format_compaction_mark_renders_drain_estimates() {
        let s = format_compaction_mark(129, 42_900, 11_103);

        assert!(s.contains("129") && s.contains("42.9K") && s.contains("11.1K"));
    }

    #[test]
    fn format_compaction_mark_renders_stub_savings() {
        let s = format_compaction_mark(0, 42_900, 34_320);

        assert!(s.contains("8.6K") && !s.contains('→'));
    }

    #[test]
    fn format_compaction_noop_distinguishes_net_loss() {
        let s = format_compaction_noop(5_000, 7_500, true);

        assert!(s.contains("5.0K") && s.contains("7.5K") && s.contains('→'));
    }

    #[test]
    fn format_compaction_negligible_is_a_clean_no_op_without_success_wording() {
        // A manual /compact that only shaved a trivial amount gets a clean
        // "no need to compact" line — NOT the "已折叠 · 节省" success mark, and NOT
        // the misleading "conversation is short".
        let s = format_compaction_negligible();

        assert!(
            s.contains("无需压缩") || s.contains("doesn't need compacting"),
            "clean no-op wording: {s}"
        );
        assert!(
            !s.contains("已折叠") && !s.contains("folded"),
            "must not reuse the stub-fold success mark: {s}"
        );
        assert!(
            !s.contains("较短") && !s.contains("is short"),
            "must not claim the conversation is short: {s}"
        );
    }

    #[test]
    fn mcp_help_lists_the_core_subcommands() {
        let s = crate::product::t(crate::product::Msg::McpHelp);
        for sub in ["tools", "reload", "trust", "help"] {
            assert!(s.contains(sub), "help must mention `{sub}`: {s}");
        }
    }

    #[test]
    fn mcp_unknown_server_names_key_and_lists_available_without_not_configured() {
        let s = crate::product::t(crate::product::Msg::McpUnknownServer {
            name: "rvs new-file",
            available: "filesystem, rvs",
        });
        assert!(
            s.contains("rvs new-file") && s.contains("filesystem"),
            "{s}"
        );
        assert!(
            !s.contains("not configured") && !s.contains("未配置"),
            "{s}"
        );
    }

    #[test]
    fn format_compaction_interrupted_is_not_a_noop_message() {
        let s = format_compaction_interrupted();

        assert!(s.contains("interrupt") || s.contains("中断"));
        assert!(!s.contains("nothing to compact") && !s.contains("无需压缩"));
    }

    #[test]
    fn cli_flag_unparseable_falls_through() {
        let env = |_: &str| None;
        assert_eq!(
            resolve_initial_locale_with_env(Some("fr"), Some(Locale::ZhCn), &env),
            Locale::ZhCn
        );
        assert_eq!(
            resolve_initial_locale_with_env(Some("fr"), None, &env),
            Locale::En
        );
    }

    #[test]
    fn todo_panel_labels_render() {
        // Default locale (En); exact copy is locale-dependent, assert non-empty + digit.
        assert!(!t(Msg::TodoPanelTitle).is_empty());
        assert!(t(Msg::TodoPanelCompleted { n: 3 }).contains('3'));
        assert!(t(Msg::TodoPanelMore { n: 2 }).contains('2'));
    }

    #[test]
    fn welcome_tip_descriptions_present_both_langs() {
        // Check a representative subset of the new welcome-tips variants in both locales.
        macro_rules! check {
            ($variant:expr) => {{
                let en = t_with(Locale::En, $variant);
                assert!(!en.is_empty(), "EN empty for {}", stringify!($variant));
                let zh = t_with(Locale::ZhCn, $variant);
                assert!(!zh.is_empty(), "ZH empty for {}", stringify!($variant));
            }};
        }
        check!(Msg::WelcomeTipsHeading);
        check!(Msg::WelcomeTipLogin);
        check!(Msg::WelcomeTipGoal);
        check!(Msg::WelcomeTipLoop);
        check!(Msg::WelcomeTipSession);
        check!(Msg::WelcomeTipInit);
    }

    #[test]
    fn model_copy_explains_default_and_current_session_scope() {
        let en_desc = t_with(Locale::En, Msg::CmdDescModel);
        let zh_desc = t_with(Locale::ZhCn, Msg::CmdDescModel);
        assert!(en_desc.contains("default") && en_desc.contains("this session"));
        assert!(zh_desc.contains("默认") && zh_desc.contains("当前会话"));

        let en_switched = t_with(
            Locale::En,
            Msg::ModelSwitchedAndDefault {
                provider: "provider",
                model: "model",
            },
        );
        let zh_switched = t_with(
            Locale::ZhCn,
            Msg::ModelSwitchedAndDefault {
                provider: "provider",
                model: "model",
            },
        );
        assert!(en_switched.contains("default for new sessions"));
        assert!(zh_switched.contains("新会话默认"));

        let ephemeral = t_with(
            Locale::En,
            Msg::ModelSwitched {
                provider: "provider",
                model: "model",
            },
        );
        assert!(ephemeral.contains("this session"));
        assert!(!ephemeral.contains("default"));
    }

    #[test]
    fn provider_panel_copy_is_localized_in_both_languages() {
        let en_tabs = (
            t_with(Locale::En, Msg::ProviderPanelTabAccounts),
            t_with(Locale::En, Msg::ProviderPanelTabModels),
        );
        let zh_tabs = (
            t_with(Locale::ZhCn, Msg::ProviderPanelTabAccounts),
            t_with(Locale::ZhCn, Msg::ProviderPanelTabModels),
        );
        assert_eq!(en_tabs.0, "Accounts");
        assert_eq!(en_tabs.1, "Models");
        assert_eq!(zh_tabs.0, "账号");
        assert_eq!(zh_tabs.1, "模型");

        let en_hint = t_with(Locale::En, Msg::ProviderPanelAccountsHint);
        let zh_hint = t_with(Locale::ZhCn, Msg::ProviderPanelAccountsHint);
        assert!(en_hint.contains("add") && en_hint.contains("delete"));
        assert!(zh_hint.contains("添加") && zh_hint.contains("删除"));
        assert!(en_hint.contains("Ctrl+Dx2"));
        assert!(zh_hint.contains("Ctrl+Dx2"));
        assert!(en_hint.contains("Ctrl+A") && en_hint.contains("Ctrl+E"));
        assert!(zh_hint.contains("Ctrl+A") && zh_hint.contains("Ctrl+E"));
        assert_eq!(
            t_with(Locale::En, Msg::ProviderPanelAddModelRow),
            "+ Add model"
        );
        assert_eq!(
            t_with(Locale::ZhCn, Msg::ProviderPanelAddModelRow),
            "＋ 添加模型"
        );

        assert_eq!(
            t_with(Locale::En, Msg::ProviderPanelEmptyModels),
            "(No models yet — press Ctrl+A to add one)"
        );
        assert_eq!(
            t_with(Locale::ZhCn, Msg::ProviderPanelEmptyModels),
            "（尚无模型 — 按 Ctrl+A 添加）"
        );

        assert_eq!(
            t_with(
                Locale::En,
                Msg::ProviderPanelModelSaved {
                    model: "deepseek-chat"
                }
            ),
            "Saved model \"deepseek-chat\"."
        );
        assert_eq!(
            t_with(
                Locale::ZhCn,
                Msg::ProviderPanelModelSaved {
                    model: "deepseek-chat"
                }
            ),
            "已保存模型“deepseek-chat”。"
        );

        let en_row = t_with(Locale::En, Msg::ProviderPanelModelCount { count: 3 });
        let zh_row = t_with(Locale::ZhCn, Msg::ProviderPanelModelCount { count: 3 });
        assert_eq!(en_row, "3 models");
        assert_eq!(zh_row, "3 个模型");
    }

    #[test]
    fn usage_modal_i18n_present_both_langs() {
        macro_rules! check {
            ($variant:expr) => {{
                let en = t_with(Locale::En, $variant);
                assert!(!en.is_empty(), "EN empty for {}", stringify!($variant));
                let zh = t_with(Locale::ZhCn, $variant);
                assert!(!zh.is_empty(), "ZH empty for {}", stringify!($variant));
            }};
        }
        check!(Msg::UsageTabCurrent);
        check!(Msg::UsageTabOverview);
        check!(Msg::UsageTabModels);
        check!(Msg::UsageCurrentTitle);
        check!(Msg::UsageResetsIn { hms: "01:23:45" });
        check!(Msg::UsageWindowUnavailable);
        check!(Msg::UsageStatFavorite);
        check!(Msg::UsageStatTotal);
        check!(Msg::UsageStatRequests);
        check!(Msg::UsageStatActiveDays);
        check!(Msg::UsageStatMostActive);
        check!(Msg::UsageStatLongestStreak);
        check!(Msg::UsageStatCurrentStreak);
        check!(Msg::UsageHeatLess);
        check!(Msg::UsageHeatMore);
        check!(Msg::UsageModelsTitle);
        check!(Msg::UsageNoData);
        check!(Msg::UsageFooterHint);
        check!(Msg::UsageFetchFailed { error: "timeout" });
        check!(Msg::UsagePlanTitle);
        check!(Msg::UsagePlanActive);
        check!(Msg::UsagePlanExpired);
        check!(Msg::UsagePlanClaimedExpires {
            claimed: "2026-06-01",
            expires: "2026-07-01"
        });
        check!(Msg::UsagePlanRemaining {
            remaining: 5,
            total: 30
        });
        check!(Msg::UsageCopied);
    }

    #[test]
    fn network_connect_hint_present_both_langs() {
        let _g = test_lock();
        let en = t_with(Locale::En, Msg::NetworkConnectHint);
        let zh = t_with(Locale::ZhCn, Msg::NetworkConnectHint);
        assert!(!en.trim().is_empty(), "en hint must be non-empty");
        assert!(!zh.trim().is_empty(), "zh hint must be non-empty");
        // Mentions the actionable knobs so the hint is useful.
        assert!(
            en.contains("/proxy") && en.contains("HTTPS_PROXY"),
            "en: {en}"
        );
        assert!(
            zh.contains("/proxy") && zh.contains("HTTPS_PROXY"),
            "zh: {zh}"
        );
    }

    #[test]
    fn plugin_install_toast_reports_the_reload_it_already_did() {
        let _g = test_lock();
        // `reload_plugins` runs immediately before this toast is rendered, so
        // the skills are already live. The old text said "Run /reload-plugins
        // to apply" — a slash command that does not exist, asking for work
        // that had already happened — while discarding all three counts.
        for locale in [Locale::En, Locale::ZhCn] {
            let msg = t_with(
                locale,
                Msg::PluginInstallDone {
                    plugin: "some-plugin",
                    marketplace: "atomcode-plugins-official",
                    loaded: 5,
                    skipped: 0,
                    show_details_hint: false,
                },
            );
            assert!(msg.contains("some-plugin"), "{locale:?}: {msg}");
            assert!(
                msg.contains('5'),
                "must report the count: {locale:?}: {msg}"
            );
            assert!(
                !msg.contains("/reload-plugins"),
                "must not name a command that does not exist: {locale:?}: {msg}"
            );
        }
    }

    #[test]
    fn plugin_install_toast_surfaces_skipped_skills_and_the_details_hint() {
        let _g = test_lock();
        // A rejected SKILL.md is the case a user most needs to notice; the
        // counts were being computed and then thrown away.
        for locale in [Locale::En, Locale::ZhCn] {
            let msg = t_with(
                locale,
                Msg::PluginUpdateDone {
                    plugin: "some-plugin",
                    marketplace: "atomcode-plugins-official",
                    loaded: 2,
                    skipped: 3,
                    show_details_hint: true,
                },
            );
            assert!(msg.contains('3'), "skipped count: {locale:?}: {msg}");
            assert!(msg.contains("Ctrl+O"), "details hint: {locale:?}: {msg}");
        }
    }

    /// `{brand}` and `{oauth}` placeholders must render the settled names,
    /// not leak through verbatim. `set_brand` is idempotent (`OnceLock` keeps
    /// the first value), so this test's `set_brand("TestBrand", ...)` is a
    /// no-op if an earlier test already settled — which is fine: the assert
    /// then checks the upstream default `"AtomCode"`, still proving the
    /// placeholder is replaced.
    #[test]
    fn placeholders_are_replaced_with_settled_names() {
        let _g = test_lock();
        // Settle a test-local brand. RwLock (last write wins) so this IS
        // observable, not a no-op like the old OnceLock. We restore the
        // upstream default at the end so this test does not pollute the
        // process-level cache for subsequent tests (test ordering independence).
        set_brand("TestBrand", "TestOAuth");

        let en = t_with(Locale::En, Msg::WelcomeBannerLine1);
        let zh = t_with(Locale::ZhCn, Msg::WelcomeBannerLine1);
        // Placeholder must NOT survive into the rendered string.
        assert!(!en.contains("{brand}"), "en leaked placeholder: {en}");
        assert!(!zh.contains("{brand}"), "zh leaked placeholder: {zh}");
        // The settled brand must appear (RwLock last-write-wins guarantees this).
        assert!(
            en.contains("TestBrand"),
            "en did not use settled brand: {en}"
        );
        assert!(
            zh.contains("TestBrand"),
            "zh did not use settled brand: {zh}"
        );

        // Restore upstream default so no other test sees "TestBrand".
        set_brand("AtomCode", "AtomGit OAuth");
    }

    /// When no `set_brand` has run, the fallback must be the upstream default
    /// `"AtomCode"` / `"AtomGit OAuth"`, so a fresh process renders the
    /// upstream brand, not a bare `{brand}` token.
    #[test]
    fn unset_brand_falls_back_to_upstream_default() {
        // Do NOT call set_brand here; rely on the OnceLock being unset OR
        // holding a prior value. Either way the placeholder is replaced —
        // the point is that `{brand}` never leaks.
        let _g = test_lock();
        let en = t_with(Locale::En, Msg::OnboardingPanelTitle);
        assert!(
            !en.contains("{brand}"),
            "default fallback leaked placeholder: {en}"
        );
        // OnboardingPanelTitle is just the brand name, so the rendered value
        // is whatever was settled (or "AtomCode" default) — never the raw token.
        assert!(!en.is_empty(), "OnboardingPanelTitle rendered empty");
    }

    /// `{oauth}` placeholder in login command descriptions is replaced with
    /// the settled OAuth provider name (or upstream default `"AtomGit OAuth"`).
    #[test]
    fn oauth_placeholder_is_replaced() {
        let _g = test_lock();
        let en = t_with(Locale::En, Msg::CmdDescLogin);
        let zh = t_with(Locale::ZhCn, Msg::CmdDescLogin);
        assert!(!en.contains("{oauth}"), "en leaked oauth placeholder: {en}");
        assert!(!zh.contains("{oauth}"), "zh leaked oauth placeholder: {zh}");
    }

    /// The authoritative `set_brand` call (from the full `Config` load) MUST
    /// override an earlier pre-scan value. This is the --config / --seed-config
    /// correctness contract: the pre-scan reads only the default config path,
    /// so a custom config's brand must win when the authoritative load settles.
    #[test]
    fn authoritative_set_brand_overrides_pre_scan() {
        let _g = test_lock();
        // Simulate the pre-scan (default config path sees "AtomCode").
        set_brand("AtomCode", "AtomGit OAuth");
        // Simulate the authoritative load from a custom config with "LongCode".
        set_brand("LongCode", "OA OAuth");
        let en = t_with(Locale::En, Msg::OnboardingPanelTitle);
        assert_eq!(
            en, "LongCode",
            "authoritative brand did not override pre-scan: {en}"
        );
        // Restore upstream default so no other test sees "LongCode".
        set_brand("AtomCode", "AtomGit OAuth");
    }

    /// A first-run scenario (no config file yet) must still render the upstream
    /// default brand, not a bare `{brand}` token or an empty string. This is
    /// the --seed-config path: Config::default() applies env overrides, so an
    /// env-set brand surfaces even without a config file.
    #[test]
    fn first_run_no_config_renders_env_brand() {
        let _g = test_lock();
        // Simulate Config::default() path: no config, env override applied.
        set_brand("EnvBrand", "EnvOAuth");
        let en = t_with(Locale::En, Msg::WelcomeBannerLine1);
        assert!(
            en.contains("EnvBrand"),
            "env brand not rendered on first-run: {en}"
        );
        assert!(
            !en.contains("{brand}"),
            "first-run leaked placeholder: {en}"
        );
        // Restore upstream default.
        set_brand("AtomCode", "AtomGit OAuth");
    }
}
