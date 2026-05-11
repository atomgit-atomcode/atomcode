use std::borrow::Cow;
use super::messages::Msg;

pub(super) fn en(msg: Msg<'_>) -> Cow<'static, str> {
    match msg {
        Msg::WelcomeBannerLine1 => {
            "Welcome to AtomCode. Pick an option to get started:".into()
        }
        Msg::ErrUnsupportedLocale { input } => {
            format!("unsupported locale: {input}").into()
        }
    }
}
