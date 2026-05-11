use std::borrow::Cow;
use super::messages::Msg;

pub(super) fn zh_cn(msg: Msg<'_>) -> Cow<'static, str> {
    match msg {
        Msg::WelcomeBannerLine1 => {
            "欢迎使用 AtomCode，请选择一项开始：".into()
        }
        Msg::ErrUnsupportedLocale { input } => {
            format!("不支持的语言：{input}").into()
        }
    }
}
