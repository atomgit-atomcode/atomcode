//! 会话叫什么名字,这里只剩**一条规矩**。
//!
//! 起名字本身归 harness 的 `session-title` 那条缝(`session-title-first-prompt`
//! 读第一句话,`session-title-model` 问工具模型),由 `session-title-on-first-prompt`
//! 在第一句话落地之后触发,答案作为一条 `Titled` 事实提交 —— 所以会话目录、
//! `/resume` 列表和屏上的标题读到的是同一个名字。哪一个命名器挂上,由
//! `[ui] ai_session_naming` 决定(`on_harness::names_sessions_with_a_model`)。
//!
//! **这里原来还有第二个命名器**:运行时在回合末自己再起一次模型请求,答案走
//! `SessionNameSuggested` 事件。默认屏幕从来没接过那条事件,于是那个开关的全部
//! 效果是每个会话白烧一次模型请求;开关打开之后屏幕上一个字都不会变。一件事
//! 一个 owner,那一份已经删掉。
//!
//! 留下的是**「这个名字该不该被盖掉」**——人自己改过的名字谁也不许动。经典
//! 界面(`--classic`)与 daemon 的会话目录都读它。

/// 这个会话还能不能被自动命名。
///
/// 人自己 `/rename` 过就不行了,已经自动命名过一次也不行 —— 第二次自动命名
/// 会把人刚刚读熟的那个名字换掉,而他没有做任何事。
pub fn should_accept_ai_name(user_renamed: bool, ai_named: bool) -> bool {
    !user_renamed && !ai_named
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_the_person_gave_is_never_replaced() {
        assert!(should_accept_ai_name(false, false));
        assert!(!should_accept_ai_name(true, false), "人改过的名字不许动");
        assert!(!should_accept_ai_name(false, true), "自动命名只做一次");
        assert!(!should_accept_ai_name(true, true));
    }
}
