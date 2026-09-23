//! 住在别处的命令：在这块屏幕上敲它们，说清楚它们在哪。
//!
//! 2026-09-22 翻了默认（`docs/plans/2026-09-19-remaining-gaps.md` 决策 10 那一节），
//! 用户选的是「现在翻，`--classic` 兜底」：`/webui`、`/sync`、`/app`、`/desktop` 这一组
//! 共享当前终端会话的功能，要跟 daemon 迁到契约（F3）同一批做，在那之前只在 tuix 里有。
//! 这一行不实现它们，只让敲惯了的人得到一句「去哪儿用」，而不是「没有这条命令」。
//!
//! **为什么是启动器的行、不是屏幕的**：`--classic` 是这个二进制的 flag，屏幕不该认识它
//! （`docs/adr/0022` §3）；换一个不带经典界面的构建，这一行整个不挂就是了。
//!
//! **为什么是隐藏命令**：列出来，欢迎块和命令菜单就会去推荐一条只能回答「去别处」的
//! 命令。敲得到、但不推荐。
//!
//! 两类，说的话不一样：[`CLASSIC_ONLY`] 是这块屏幕还没做、暂时只在 tuix 里的；
//! [`IN_THE_CLI`] 是本来就归命令行的（`/upgrade` 自下载替换二进制，界面里再做一遍
//! 只是重复它）。
//!
//! **什么时候删**：`CLASSIC_ONLY` 里的命令在这块屏幕上真做出来时逐条拿掉，
//! F1 删 tuix 之前它必须已经空了；`IN_THE_CLI` 不会空——它说的是一条长期的边界。

use atomcode_i18n::screen::{t as tr, Msg as SMsg};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::command::{Command, CommandSet, Outcome};
use atomcode_tui::plugin::CommandsSvc;
use serde_json::Value;

/// 行的名字。
pub const ROW: &str = "tui-elsewhere";

/// 还只在经典界面里的命令。
///
/// `/webui`、`/sync`、`/desktop` 已于 2026-09-23 在这块屏幕上做出来(`tui_share`),
/// 从这里拿掉了;剩 `/app`(扫码给手机)那一条。
pub const CLASSIC_ONLY: &[&str] = &["app"];

/// 归命令行的命令，以及在命令行里怎么运行它。
pub const IN_THE_CLI: &[(&str, &str)] = &[("upgrade", "atomcode upgrade")];

pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// 挂上那两组提示命令。
pub struct ElsewhereRow;

#[async_trait]
impl Plugin for ElsewhereRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands"]
    }
    fn description(&self) -> &'static str {
        "commands that live elsewhere: typed here, they say where"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let commands = ctx.require::<CommandsSvc>().map_err(|e| e.to_string())?;
        commands.add(Arc::new(Elsewhere))?;
        Ok(())
    }
}

struct Elsewhere;

#[async_trait]
impl CommandSet for Elsewhere {
    fn id(&self) -> &'static str {
        ROW
    }

    fn commands(&self) -> Vec<Command> {
        Vec::new()
    }

    fn hidden(&self) -> Vec<Command> {
        CLASSIC_ONLY
            .iter()
            .map(|name| Command::said(name, tr(SMsg::CmdAboutClassicOnly)))
            .chain(
                IN_THE_CLI
                    .iter()
                    .map(|(name, _)| Command::said(name, tr(SMsg::CmdAboutInTheCli))),
            )
            .collect()
    }

    async fn run(&self, name: &str, _args: &str, _ctx: &Context) -> Outcome {
        if let Some(known) = CLASSIC_ONLY.iter().find(|known| **known == name) {
            return Outcome::Said(tr(SMsg::ClassicOnlyForNow { command: known }).into_owned());
        }
        match IN_THE_CLI.iter().find(|(known, _)| *known == name) {
            Some((known, run)) => Outcome::Said(
                tr(SMsg::LivesInTheCli {
                    command: known,
                    run,
                })
                .into_owned(),
            ),
            None => Outcome::Quiet,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_of_them_is_offered_only_answered() {
        // 列出来，欢迎块和命令菜单就会推荐一条只能回答「去别处」的命令。
        assert!(Elsewhere.commands().is_empty());
        let hidden: Vec<String> = Elsewhere
            .hidden()
            .into_iter()
            .map(|c| c.name.into_owned())
            .collect();
        let expected: Vec<String> = CLASSIC_ONLY
            .iter()
            .map(|n| n.to_string())
            .chain(IN_THE_CLI.iter().map(|(n, _)| n.to_string()))
            .collect();
        assert_eq!(hidden, expected);
    }
}
