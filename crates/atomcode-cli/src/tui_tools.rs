//! 工具端口:`atomcode --tui` 这一侧的另一半。
//!
//! `atomcode_tui::tools` 画列表、走光标、收按键;这里知道「此刻能调什么」是运行中
//! 那棵树的事,要走宿主控制契约问一趟(`docs/adr/0021` §2、`docs/adr/0022` §3)。
//! 屏幕那边不认识 `HostCommand`,这是故意的。
//!
//! **为什么不是屏幕直接读目录**:目录住在 agent 的 App 里,而屏幕是另一个 App
//! (`docs/adr/0022`)。`tui/tests/guards.rs` 守着这条线。
//!
//! 语义、开关活多久、MCP 怎么按台算,见 `docs/tool-catalog-policy.md`。

use atomcode_i18n::screen::{t as tr, Msg as SMsg};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_host_api::{CatalogTool, HostCommand, HostReply, ToolState};
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::module::{Modules, Mounted};
use atomcode_tui::plugin::{AgentClientSvc, ModulesSvc, ToolCatalogSvc};
use atomcode_tui::tools::{State, ToolRow, Tools, ToolsView};
use serde_json::Value;

/// 行的名字,插件和点它的那一层共用一个串。
pub const ROW: &str = "tui-panel-tools";

/// 把工具面板挂到屏幕上、并把端口填进去的那一行。
///
/// **启动器的行,不是屏幕的**,和 `tui-panel-providers`、`tui-panel-plugins` 一样:
/// `atomcode-tui` 带的是面板的*画法*,它不知道有宿主控制契约这回事。
pub struct ToolsRow;

#[async_trait]
impl Plugin for ToolsRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-tools"]
    }
    fn description(&self) -> &'static str {
        "the tools panel: what the model can call, and the person's switch over it"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
        let view = Arc::new(Mounted::<atomcode_tui::modules::tools::Tools>::new());
        let id = <atomcode_tui::modules::tools::Tools as atomcode_tui::module::View>::id();
        mods.add_view(view)?;
        let m: Arc<Modules> = mods.clone();
        let _ = ctx.effect(move || m.remove_view(id));
        let _ = ctx
            .provide::<ToolCatalogSvc>(Arc::new(ToolsPort { ctx: ctx.clone() }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// 一趟宿主往返。
///
/// 不持 `HostControl` 而是每次问 `ctx` 要:连接会换(换会话、重连),持着的那个会
/// 指向已经没人听的一端。
struct ToolsPort {
    ctx: Context,
}

impl ToolsPort {
    fn link(&self) -> Result<(Arc<dyn atomcode_host_api::HostControl>, String), String> {
        let client = self
            .ctx
            .service::<AgentClientSvc>()
            .ok_or_else(|| tr(SMsg::ScreenNotConnectedAgent).into_owned())?;
        let control = client
            .control()
            .ok_or_else(|| tr(SMsg::HostHasNoControl).into_owned())?;
        Ok((control, client.root()))
    }

    async fn ask(&self, command: HostCommand) -> Result<ToolsView, String> {
        let (control, _) = self.link()?;
        match control.call(command).await {
            Ok(HostReply::ToolCatalog { tools }) => Ok(view(tools)),
            Ok(other) => Err(tr(SMsg::HostSaidSomethingElse {
                reply: &format!("{other:?}"),
            })
            .into_owned()),
            Err(error) => Err(said(error)),
        }
    }
}

#[async_trait]
impl Tools for ToolsPort {
    async fn list(&self) -> Result<ToolsView, String> {
        let (_, session) = self.link()?;
        self.ask(HostCommand::ToolCatalog { session }).await
    }

    async fn switch(&self, pattern: &str, on: bool) -> Result<ToolsView, String> {
        let (_, session) = self.link()?;
        self.ask(HostCommand::SwitchTool {
            session,
            pattern: pattern.to_string(),
            on,
        })
        .await
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

/// 契约的话译成屏幕的话。两边各一份类型,理由同 `docs/adr/0021` §2。
fn view(tools: Vec<CatalogTool>) -> ToolsView {
    ToolsView::new(
        tools
            .into_iter()
            .map(|tool| ToolRow {
                name: tool.name,
                owner: tool.owner,
                state: match tool.state {
                    ToolState::On => State::On,
                    ToolState::OffInSession => State::Off,
                    // 契约是 `non_exhaustive` 的:一个这个构建还不认识的状态,按
                    // 「不是模型的工具」算,而不是按「能调」算。少画一个工具是误差,
                    // 多画一个是假话。
                    _ => State::Excluded,
                },
            })
            .collect(),
    )
}

/// 这一行的那一层,给装配用。
pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}
