use std::borrow::Cow;
use super::messages::Msg;

pub(super) fn zh_cn(msg: Msg<'_>) -> Cow<'static, str> {
    match msg {
        Msg::WelcomeBannerLine1 =>
            "欢迎使用 AtomCode，请选择一项开始：".into(),
        Msg::WelcomeBannerLine2 =>
            "（↑↓ 切换，Enter 确认，Esc 跳过）".into(),
        Msg::WelcomeOptionCodingPlan => "配置 CodingPlan".into(),
        Msg::WelcomeOptionCodingPlanHint => "免费额度 · 推荐".into(),
        Msg::WelcomeOptionConfigureManually => "手动配置".into(),
        Msg::WelcomeOptionConfigureManuallyHint => "使用 API key".into(),
        Msg::WelcomeOptionSkip => "暂时跳过".into(),
        Msg::WelcomeOptionSkipHint => "稍后再说".into(),

        Msg::ErrUnsupportedLocale { input } =>
            format!("不支持的语言：{input}").into(),
    }
}
