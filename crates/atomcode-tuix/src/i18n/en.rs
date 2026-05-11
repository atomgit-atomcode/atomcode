use std::borrow::Cow;
use super::messages::Msg;

pub(super) fn en(msg: Msg<'_>) -> Cow<'static, str> {
    match msg {
        Msg::WelcomeBannerLine1 =>
            "Welcome to AtomCode. Pick an option to get started:".into(),
        Msg::WelcomeBannerLine2 =>
            "(↑↓ to navigate, Enter to confirm, Esc to skip)".into(),
        Msg::WelcomeOptionCodingPlan => "Set up CodingPlan".into(),
        Msg::WelcomeOptionCodingPlanHint => "Free tokens · recommended".into(),
        Msg::WelcomeOptionConfigureManually => "Configure manually".into(),
        Msg::WelcomeOptionConfigureManuallyHint => "API key".into(),
        Msg::WelcomeOptionSkip => "Skip for now".into(),
        Msg::WelcomeOptionSkipHint => "explore first".into(),

        Msg::ErrUnsupportedLocale { input } =>
            format!("unsupported locale: {input}").into(),
    }
}
