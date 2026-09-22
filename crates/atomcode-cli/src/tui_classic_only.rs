//! 只有经典界面还有的命令：在这块屏幕上敲它们，说清楚它们还住在哪。
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
//! **什么时候删**：这组命令在这块屏幕上真做出来时，逐条从 [`NAMES`] 里拿掉；拿空了，
//! 这个文件连同它在 `tui_front::mount` 里的那一行一起删。F1 删 tuix 之前它必须已经空了。

use atomcode_i18n::screen::{t as tr, Msg as SMsg};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::command::{Command, CommandSet, Outcome};
use atomcode_tui::plugin::CommandsSvc;
use serde_json::Value;

/// 行的名字。
pub const ROW: &str = "tui-classic-only";

/// 还只在经典界面里的命令。
pub const NAMES: &[&str] = &["webui", "sync", "app", "desktop"];

pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// 挂上那组提示命令。
pub struct ClassicOnlyRow;

#[async_trait]
impl Plugin for ClassicOnlyRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands"]
    }
    fn description(&self) -> &'static str {
        "commands only the classic screen has yet: typed here, they say where they still are"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let commands = ctx.require::<CommandsSvc>().map_err(|e| e.to_string())?;
        commands.add(Arc::new(ClassicOnly))?;
        Ok(())
    }
}

struct ClassicOnly;

#[async_trait]
impl CommandSet for ClassicOnly {
    fn id(&self) -> &'static str {
        ROW
    }

    fn commands(&self) -> Vec<Command> {
        Vec::new()
    }

    fn hidden(&self) -> Vec<Command> {
        NAMES
            .iter()
            .map(|name| Command::said(name, tr(SMsg::CmdAboutClassicOnly)))
            .collect()
    }

    async fn run(&self, name: &str, _args: &str, _ctx: &Context) -> Outcome {
        match NAMES.iter().find(|known| **known == name) {
            Some(known) => {
                Outcome::Said(tr(SMsg::ClassicOnlyForNow { command: known }).into_owned())
            }
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
        assert!(ClassicOnly.commands().is_empty());
        let hidden: Vec<String> = ClassicOnly
            .hidden()
            .into_iter()
            .map(|c| c.name.into_owned())
            .collect();
        assert_eq!(hidden, NAMES);
    }
}
