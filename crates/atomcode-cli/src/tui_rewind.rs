//! 回退端口:`atomcode --tui` 这一侧的另一半。
//!
//! `atomcode_tui::rewind` 画回合、走光标、收按键;这里知道「这次会话走过哪些回合」
//! 是运行中那棵树的事(工作区那一半还在磁盘上的检查点里),要走宿主控制契约问一趟
//! (`docs/adr/0021` §2、`docs/adr/0022` §3)。屏幕那边不认识 `HostCommand`,这是
//! 故意的。
//!
//! **为什么一次回退要带 `based_on`**:屏幕看到的最后一条事实就是它据以下判断的那
//! 条。中间要是又落了一个回合,这一下回退指的已经不是人看见的那个位置——宿主按
//! `Stale` 拒掉,而不是回到一个谁都没打算回的地方(`docs/adr/0021` §9)。

use atomcode_i18n::screen::{t as tr, Msg as SMsg};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_host_api::{HostCommand, HostReply};
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::module::{Modules, Mounted};
use atomcode_tui::plugin::{AgentClientSvc, ModulesSvc, RewindSvc};
use atomcode_tui::rewind::{Change, CodeOff, Done, Point, Rewind, RewindView, Scope};
use serde_json::Value;

/// 行的名字,插件和点它的那一层共用一个串。
pub const ROW: &str = "tui-panel-rewind";

/// 把回退面板挂到屏幕上、并把端口填进去的那一行。
///
/// **启动器的行,不是屏幕的**,和 `tui-panel-tools` 一样:`atomcode-tui` 带的是面板
/// 的*画法*,它不知道有宿主控制契约这回事。
pub struct RewindRow;

#[async_trait]
impl Plugin for RewindRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-rewind"]
    }
    fn description(&self) -> &'static str {
        "the rewind panel: the turns this session can be taken back to, and the taking back"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
        let view = Arc::new(Mounted::<atomcode_tui::modules::rewind::Rewind>::new());
        let id = <atomcode_tui::modules::rewind::Rewind as atomcode_tui::module::View>::id();
        mods.add_view(view)?;
        let m: Arc<Modules> = mods.clone();
        let _ = ctx.effect(move || m.remove_view(id));
        let _ = ctx
            .provide::<RewindSvc>(Arc::new(RewindPort { ctx: ctx.clone() }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// 一趟宿主往返。
///
/// 不持 `HostControl` 而是每次问 `ctx` 要:连接会换(换会话、重连),持着的那个会指
/// 向已经没人听的一端。
struct RewindPort {
    ctx: Context,
}

impl RewindPort {
    fn client(&self) -> Result<Arc<atomcode_tui::plugin::AgentClient>, String> {
        self.ctx
            .service::<AgentClientSvc>()
            .ok_or_else(|| tr(SMsg::ScreenNotConnectedRewind).into_owned())
    }

    fn link(&self) -> Result<(Arc<dyn atomcode_host_api::HostControl>, String, u64), String> {
        let client = self.client()?;
        let control = client
            .control()
            .ok_or_else(|| tr(SMsg::HostHasNoControl).into_owned())?;
        Ok((control, client.root(), client.root_high()))
    }
}

#[async_trait]
impl Rewind for RewindPort {
    async fn points(&self) -> Result<RewindView, String> {
        let (control, session, _) = self.link()?;
        match control.call(HostCommand::RewindPoints { session }).await {
            Ok(HostReply::RewindPoints {
                points,
                code_unavailable,
            }) => Ok(RewindView::new(
                points.into_iter().map(point).collect(),
                code_unavailable.map(why_not),
            )),
            Ok(other) => Err(tr(SMsg::HostSaidSomethingElse {
                reply: &format!("{other:?}"),
            })
            .into_owned()),
            Err(error) => Err(said(error)),
        }
    }

    async fn rewind(&self, turn: u64, scope: Scope) -> Result<Done, String> {
        let (control, session, based_on) = self.link()?;
        let scope = match scope {
            Scope::Conversation => atomcode_kernel::session::RewindScope::Conversation,
            // 契约里的第三档(只回工作区)面板不给,所以这里也没有它可映射——
            // `/rewind N 代码` 仍然走得到它,那条路在 `tui::commands`。
            Scope::Both => atomcode_kernel::session::RewindScope::Both,
        };
        match control
            .call(HostCommand::Rewind {
                session,
                turn,
                scope,
                based_on,
            })
            .await
        {
            Ok(HostReply::Undone {
                prompt,
                restored_files,
            }) => Ok(Done {
                prompt,
                files: restored_files.len(),
            }),
            Ok(other) => Err(tr(SMsg::HostSaidSomethingElse {
                reply: &format!("{other:?}"),
            })
            .into_owned()),
            Err(error) => Err(said(error)),
        }
    }
}

/// 契约的话译成屏幕的话。两边各一份类型,理由同 `docs/adr/0021` §2。
fn point(point: atomcode_host_api::RewindPoint) -> Point {
    Point {
        turn: point.turn,
        prompt: point.prompt,
        changes: point
            .changes
            .into_iter()
            .map(|file| Change {
                path: file.path,
                additions: file.added,
                deletions: file.removed,
            })
            .collect(),
        code: point.code,
    }
}

/// 工作区为什么回不去:契约的分类译成屏幕的分类。**话不在这儿说**——屏幕按人选的
/// 语言说它自己的那句,这一层只管哪一种(`docs/adr/0021` §2)。
fn why_not(why: atomcode_host_api::CodeUnavailable) -> CodeOff {
    use atomcode_host_api::CodeUnavailable as Why;
    match why {
        Why::NotEnabled => CodeOff::NotEnabled,
        Why::NoSession => CodeOff::NoSession,
        Why::Failed { message } => CodeOff::Failed(message),
        // 契约是 `non_exhaustive` 的:一个这个构建还不认识的原因,照样是「回不去」,
        // 而不是「回得去」。少给一次回退是误差,多给一次是把人的工作区搅了。
        other => CodeOff::Failed(format!("{other:?}")),
    }
}

/// 宿主拒绝的话,说给人听。屏幕那边有一份同样的映射,这里不借它:那是 tui 的
/// `pub(crate)`,而一个为了共用它而开放的函数,就是为一句话开一条公开面。
fn said(error: atomcode_host_api::HostError) -> String {
    use atomcode_host_api::HostError;
    match error {
        HostError::Busy { reason } => tr(SMsg::HostBusy { reason: &reason }).into_owned(),
        HostError::Unavailable => tr(SMsg::HostUnavailable).into_owned(),
        HostError::NotFound => tr(SMsg::HostNotFoundShort).into_owned(),
        HostError::Failed { message } => message,
        other => format!("{other:?}"),
    }
}

/// 这一行的那一层,给装配用。
pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}
