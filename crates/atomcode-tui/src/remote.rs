//! 手机或浏览器请这块屏幕跑一条命令。
//!
//! 共享出去之后,那一端看的是**同一段对话**。它偶尔想让这台机器干一件小事——
//! 「现在什么状态」「今天花了多少」「当前改了哪些文件」——而那些答案只有这块
//! 屏幕这一侧有。于是有这条反向的路:远端说一行,屏幕跑,跑完的话回去。
//!
//! **手势归屏幕,线路归启动器。** 远端在哪、话怎么回去,是 daemon 的事,而这块
//! 屏幕不认识 daemon(`docs/adr/0022` §3)。所以跑什么、准不准跑、跑完怎么画,
//! 在这里;线路是一条缝。没填就没有这条路——headless 与 ACP 正是如此。
//!
//! **为什么是白名单,而不是「远端等同于本人」。** 手机上一个误触和键盘上一次
//! 敲击不是一回事:那一端没有这块屏幕的上下文(它看不见编辑区里攒的东西、看不见
//! 正开着的面板),而 `/cd`、`/model`、`/clear` 这类命令会把这一侧的地面换掉。
//! 所以只放行**读**的那几条,外加 `/goal` ——它是那一端真正要的唯一一件有状态的事
//! (在手机上起一个自主目标,然后把手机放下)。上一代前端定的也是这几条。

/// 远端问过来的一行是什么。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Asked {
    /// 跑这一行(已去掉前导 `/`,原样是命令派发要的形状)。
    Run(String),
    /// 认得,但这块屏幕上没有对应的东西——不必报错,也不必画什么。
    Nothing,
    /// 不放行,并把这句话回给远端。
    Refused,
}

/// 远端放行的是哪几条:读的那几条,外加 `/goal`。
///
/// 只看第一个词,参数原样带过去:`/diff HEAD~1` 与 `/diff` 是同一条命令。
pub fn asked(line: &str) -> Asked {
    let line = line.trim().trim_start_matches('/').trim();
    if line.is_empty() {
        return Asked::Refused;
    }
    let (name, rest) = line
        .split_once(char::is_whitespace)
        .map(|(name, rest)| (name, rest.trim()))
        .unwrap_or((line, ""));
    // 上一代前端里,手机端的编辑区有一个「按下去就进目标模式」的预备态,按下时
    // 发这一行过来把桌面那侧的同一个状态也拨过去。这块屏幕没有那个状态——目标
    // 从 `/goal <条件>` 开始,没有中间态——所以这一行在这里什么也不是。答它
    // 「不认识这条命令」会是错的:远端没做错任何事。
    if name.eq_ignore_ascii_case("goal") && rest == "__app_arm" {
        return Asked::Nothing;
    }
    if name.eq_ignore_ascii_case("goal") || READ_ONLY.iter().any(|it| name.eq_ignore_ascii_case(it))
    {
        return Asked::Run(line.to_string());
    }
    Asked::Refused
}

/// 读一眼就完的那几条。
///
/// 加一条进来之前先问:它会不会把这一侧的地面换掉(工作目录、模型、会话、
/// 正开着的面板)。会,就不属于这里。
const READ_ONLY: &[&str] = &["status", "cost", "whoami", "diff"];

/// 拒的时候回给远端的话。
pub fn refusal() -> String {
    format!(
        "{}\n  {}",
        crate::i18n::t(crate::i18n::Msg::RemoteDesktopOnly),
        READ_ONLY
            .iter()
            .map(|name| format!("/{name}"))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

/// 远端问过来的命令,以及跑完的话回哪儿去。启动器填。
#[async_trait::async_trait]
pub trait Remote: Send + Sync + 'static {
    /// 等下一条。`None` = 不会再有了。
    async fn next(&self) -> Option<String>;
    /// 跑完说的话,回给问的那一端。
    fn said(&self, text: String);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 远端能跑的是读的那几条,外加 `/goal`。
    ///
    /// 反面才是理由:放行任何一条之后,手机上一个误触就能换掉这一侧的工作目录
    /// 或模型,而那一端看不见这块屏幕上正开着什么。所以这条判据真正钉的是
    /// **拒的那一半**——`/cd`、`/model`、`/clear` 必须拒。
    #[test]
    fn the_far_end_may_read_and_may_set_a_goal_and_nothing_else() {
        assert_eq!(asked("/status"), Asked::Run("status".into()));
        assert_eq!(
            asked("status"),
            Asked::Run("status".into()),
            "带不带斜杠都行"
        );
        assert_eq!(
            asked("/diff HEAD~1"),
            Asked::Run("diff HEAD~1".into()),
            "参数原样带过去"
        );
        assert_eq!(
            asked("/goal 让测试全过"),
            Asked::Run("goal 让测试全过".into())
        );
        assert_eq!(asked("/goal clear"), Asked::Run("goal clear".into()));
        for moves_the_ground in [
            "/cd /tmp",
            "/model opus",
            "/clear",
            "/resume",
            "/quit",
            "/mcp",
        ] {
            assert_eq!(
                asked(moves_the_ground),
                Asked::Refused,
                "{moves_the_ground:?}"
            );
        }
        assert_eq!(asked("   "), Asked::Refused);
        // 上一代前端编辑区的预备态,这块屏幕没有:不报错,也不画。
        assert_eq!(asked("/goal __app_arm"), Asked::Nothing);
    }
}
