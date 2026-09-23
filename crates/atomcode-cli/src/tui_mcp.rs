//! MCP 面板端口:`atomcode --tui` 这一侧的另一半。
//!
//! `atomcode_tui::mcp` 画列表与详情、走光标、收按键;这里知道「有哪些服务器、
//! 每台怎么连、动作落下去会怎样」是运行中那棵树的事,必须走宿主控制契约问一趟
//! (`docs/adr/0021` §2、`docs/adr/0022` §3)。屏幕那边不认识 `HostCommand`,
//! 这是故意的。
//!
//! **为什么不是屏幕直接读配置**:配置住在运行中那棵树里,而屏幕是另一个 App
//! (`docs/adr/0022`)。`tui/tests/guards.rs` 守着这条线。
//!
//! 三条命令:`list` 走 `McpManage`,`detail` 走 `McpDetail`,`act` 走 `McpAct`。
//! `act` 的回答是刷新后的**目录**而不是这一台——契约里 `McpAct` 的注释写明了
//! (信任是整项目级的,列表比单台更有用)。字段映射与失败语义见
//! `docs/mcp-panel-design.md` §4.2、§6。

use atomcode_i18n::screen::{t as tr, Msg as SMsg};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_host_api::{
    HostCommand, HostError, HostReply, McpAction, McpAuth, McpServerDetail, McpServerState,
    McpTransport,
};
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::mcp::{Action, Auth, Mcp, McpDetail, McpState, McpView, Transport};
use atomcode_tui::module::{Modules, Mounted};
use atomcode_tui::plugin::{AgentClientSvc, McpSvc, ModulesSvc};
use serde_json::Value;

/// 行的名字,插件和点它的那一层共用一个串。
pub const ROW: &str = "tui-panel-mcp";

/// 把 MCP 面板挂到屏幕上、并把端口填进去的那一行。
///
/// **启动器的行,不是屏幕的**,和 `tui-panel-tools` 一样:`atomcode-tui` 带的是
/// 面板的*画法*,它不知道有宿主控制契约这回事。
pub struct McpRow;

#[async_trait]
impl Plugin for McpRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-mcp"]
    }
    fn description(&self) -> &'static str {
        "the mcp panel: the servers configured, what each is doing, and the person's switch over them"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
        let view = Arc::new(Mounted::<atomcode_tui::modules::mcp::Mcp>::new());
        let id = <atomcode_tui::modules::mcp::Mcp as atomcode_tui::module::View>::id();
        mods.add_view(view)?;
        let m: Arc<Modules> = mods.clone();
        let _ = ctx.effect(move || m.remove_view(id));
        let _ = ctx
            .provide::<McpSvc>(Arc::new(McpPort { ctx: ctx.clone() }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// 一趟宿主往返。
///
/// 不持 `HostControl` 而是每次问 `ctx` 要:连接会换(换会话、重连),持着的那个会
/// 指向已经没人听的一端。
struct McpPort {
    ctx: Context,
}

impl McpPort {
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
}

#[async_trait]
impl Mcp for McpPort {
    async fn list(&self) -> Result<McpView, String> {
        let (control, session) = self.link()?;
        listed(control.call(HostCommand::McpManage { session }).await)
    }

    async fn detail(&self, server: &str) -> Result<McpDetail, String> {
        let (control, session) = self.link()?;
        detailed(
            control
                .call(HostCommand::McpDetail {
                    session,
                    server: server.to_string(),
                })
                .await,
        )
    }

    async fn act(&self, server: &str, action: Action) -> Result<McpView, String> {
        let (control, session) = self.link()?;
        listed(
            control
                .call(HostCommand::McpAct {
                    session,
                    server: server.to_string(),
                    action: wire_action(action),
                })
                .await,
        )
    }
}

/// `McpManage` 与 `McpAct` 的答复:刷新后的目录(契约里 `McpAct` 就是这么定的)。
fn listed(reply: Result<HostReply, HostError>) -> Result<McpView, String> {
    match reply {
        Ok(HostReply::McpRows { rows }) => Ok(view(rows)),
        Ok(other) => Err(unexpected(&other)),
        // 宿主拒绝的话在屏幕侧只有一份译法:`tui_tools` 那份(`tui_tools.rs:114`),
        // 本 crate 别的端口也共用它。这里不另写一份,免得两处措辞各自漂移。
        Err(error) => Err(crate::tui_tools::said(error)),
    }
}

/// `McpDetail` 的答复:一台服务器的详情页。
fn detailed(reply: Result<HostReply, HostError>) -> Result<McpDetail, String> {
    match reply {
        Ok(HostReply::McpDetail { detail }) => Ok(detail_of(detail)),
        Ok(other) => Err(unexpected(&other)),
        Err(error) => Err(crate::tui_tools::said(error)),
    }
}

/// 宿主答了别的。措辞与 `tui_tools` 里那条逐字一致(都是
/// `Msg::HostSaidSomethingElse`),不另造一句话。
fn unexpected(reply: &HostReply) -> String {
    tr(SMsg::HostSaidSomethingElse {
        reply: &format!("{reply:?}"),
    })
    .into_owned()
}

/// 屏幕的动作译成契约的动作。六个一一对应——两边取同一个名字、同一件事
/// (`McpAction` 的 `Enable`/`Disable` 注释钉过方向:名字是人这一侧的)。
fn wire_action(action: Action) -> McpAction {
    match action {
        Action::Trust => McpAction::Trust,
        Action::Untrust => McpAction::Untrust,
        Action::Login => McpAction::Login,
        Action::Logout => McpAction::Logout,
        Action::Enable => McpAction::Enable,
        Action::Disable => McpAction::Disable,
    }
}

/// 契约的行译成屏幕的行。两边各一份类型,理由同 `docs/adr/0021` §2。
fn view(rows: Vec<atomcode_host_api::McpRow>) -> McpView {
    McpView::new(rows.into_iter().map(row).collect())
}

fn row(row: atomcode_host_api::McpRow) -> atomcode_tui::mcp::McpRow {
    atomcode_tui::mcp::McpRow {
        name: row.name,
        state: state(row.state),
        source: row.source,
        tool_count: row.tool_count,
        config_path: row.config_path,
    }
}

/// 契约的详情译成屏幕的详情。
fn detail_of(detail: McpServerDetail) -> McpDetail {
    McpDetail {
        name: detail.name,
        state: state(detail.state),
        source: detail.source,
        transport: transport(detail.transport),
        auth: auth(detail.auth),
        tool_count: detail.tool_count,
        config_path: detail.config_path,
    }
}

/// 一个服务器此刻是什么状态。`McpServerState` 是 `non_exhaustive` 的,而屏幕只有
/// 它自己那份状态(设计 §4.3 也只加了两个变体):一个这个构建还不认识的状态按
/// 「已断开」算——它不断言好、不断言坏,也不凭空说要人去做什么。
fn state(state: McpServerState) -> McpState {
    match state {
        McpServerState::Connecting => McpState::Connecting,
        McpServerState::Connected => McpState::Connected,
        McpServerState::Untrusted => McpState::Untrusted,
        McpServerState::NeedsAuthentication => McpState::NeedsAuthentication,
        McpServerState::Disabled => McpState::Disabled,
        McpServerState::Failed { message } => McpState::Failed(message),
        McpServerState::Disconnected => McpState::Disconnected,
        _ => McpState::Disconnected,
    }
}

/// 怎么连。契约的 `McpTransport` 本来就没带 headers 与凭据(那个类型就是为这个
/// 另立的,见它的注释),所以照搬:**命令与参数**,或**地址**,多一个字段都不带。
fn transport(transport: McpTransport) -> Transport {
    match transport {
        McpTransport::Stdio { command, args, .. } => Transport::Stdio { command, args },
        McpTransport::Http { url, .. } => Transport::Http { url },
        // 认不出的传输把宿主自己的话放在地址那行,而不是编一个命令或地址出来:
        // 屏幕的类型没有「未知的传输方式」这一行,编出来的才是假话。
        other => Transport::Http {
            url: format!("{other:?}"),
        },
    }
}

/// 要不要认证、认证了没有。两边各三种可能,一一对应。
fn auth(auth: McpAuth) -> Auth {
    match auth {
        McpAuth::None => Auth::None,
        McpAuth::OAuth { authenticated } => Auth::OAuth { authenticated },
        // 认不出的认证方式按「有认证、凭据没存下」算:说「不需要认证」会把人骗去
        // 连一台连不上的服务器,这样算只多给一个「认证」动作,不隐瞒任何事。
        _ => Auth::OAuth {
            authenticated: false,
        },
    }
}

/// 这一行的那一层,给装配用。
pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 契约的一行,照 `atomcode-host-api` 自己的测试里那种形状。
    fn wire_row(name: &str, state: McpServerState, source: &str) -> atomcode_host_api::McpRow {
        atomcode_host_api::McpRow {
            name: name.into(),
            state,
            source: source.into(),
            tool_count: 8,
            config_path: Some("/w/.mcp.json".into()),
        }
    }

    /// 契约的一台详情。
    fn wire_detail(transport: McpTransport, auth: McpAuth) -> McpServerDetail {
        McpServerDetail {
            name: "figma".into(),
            state: McpServerState::Connected,
            source: "project".into(),
            transport,
            auth,
            tool_count: 8,
            config_path: Some("/w/.mcp.json".into()),
        }
    }

    /// 屏幕的详情,拿来当期望值;整值相等才钉得住「多带了什么」。
    fn screen_detail(transport: Transport, auth: Auth) -> McpDetail {
        McpDetail {
            name: "figma".into(),
            state: McpState::Connected,
            source: "project".into(),
            transport,
            auth,
            tool_count: 8,
            config_path: Some("/w/.mcp.json".into()),
        }
    }

    /// 屏幕的一行,拿来当期望值。
    fn screen_row(name: &str, state: McpState, source: &str) -> atomcode_tui::mcp::McpRow {
        atomcode_tui::mcp::McpRow {
            name: name.into(),
            state,
            source: source.into(),
            tool_count: 8,
            config_path: Some("/w/.mcp.json".into()),
        }
    }

    /// 状态是**逐变体**译的:契约那天加一个变体,这里就该跟着想清楚它画成什么,
    /// 而不是被一个兜底分支静悄悄吞掉。七种都在这儿。
    #[test]
    fn every_state_the_contract_can_report_translates_to_the_matching_screen_state() {
        let cases = [
            (McpServerState::Connecting, McpState::Connecting),
            (McpServerState::Connected, McpState::Connected),
            (McpServerState::Untrusted, McpState::Untrusted),
            (
                McpServerState::NeedsAuthentication,
                McpState::NeedsAuthentication,
            ),
            (McpServerState::Disabled, McpState::Disabled),
            (
                McpServerState::Failed {
                    message: "boom".into(),
                },
                McpState::Failed("boom".into()),
            ),
            (McpServerState::Disconnected, McpState::Disconnected),
        ];
        for (wire, screen) in cases {
            assert_eq!(state(wire.clone()), screen, "{wire:?}");
        }
    }

    /// 六个动作一一对应。写反一个方向,人就按着「停用」把服务器启用了
    /// (`McpAction` 的注释专门钉过这一点)。
    #[test]
    fn each_screen_action_translates_to_the_wire_action_of_the_same_name() {
        use atomcode_tui::mcp::Action as A;
        let cases = [
            (A::Trust, McpAction::Trust),
            (A::Untrust, McpAction::Untrust),
            (A::Login, McpAction::Login),
            (A::Logout, McpAction::Logout),
            (A::Enable, McpAction::Enable),
            (A::Disable, McpAction::Disable),
        ];
        for (screen, wire) in cases {
            assert_eq!(wire_action(screen), wire, "{screen:?}");
        }
    }

    /// 一行的映射不吞字段:来源原样带过去(屏幕按它分组),工具数与配置路径都要到。
    #[test]
    fn a_listed_server_keeps_its_name_source_count_and_config_path() {
        let listed = view(vec![wire_row(
            "figma",
            McpServerState::NeedsAuthentication,
            "project",
        )]);
        let rows = listed.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            screen_row("figma", McpState::NeedsAuthentication, "project")
        );
    }

    /// stdio 服务器只带命令与参数过去。这里的相等是**整值**相等,所以契约变体里
    /// 那个 `timeout_ms`(以及将来可能加的字段)没有一条到屏幕的路——多带一个就断。
    ///
    /// headers 与环境变量在契约的 `McpTransport` 上本来就没有(它正是为此另立的),
    /// 屏幕上更没有地方放它们:这是两边类型的形状本身保证的。
    #[test]
    fn a_stdio_server_translates_to_its_command_and_arguments_and_nothing_else() {
        let detail = detail_of(wire_detail(
            McpTransport::Stdio {
                command: "npx".into(),
                args: vec![
                    "-y".into(),
                    "@modelcontextprotocol/server-filesystem".into(),
                ],
                timeout_ms: Some(30_000),
            },
            McpAuth::None,
        ));
        assert_eq!(
            detail,
            screen_detail(
                Transport::Stdio {
                    command: "npx".into(),
                    args: vec![
                        "-y".into(),
                        "@modelcontextprotocol/server-filesystem".into()
                    ],
                },
                Auth::None,
            )
        );
    }

    /// HTTP 服务器只带地址过去,别的什么都不带。
    #[test]
    fn an_http_server_translates_to_its_url_and_nothing_else() {
        let detail = detail_of(wire_detail(
            McpTransport::Http {
                url: "https://mcp.example/sse".into(),
                timeout_ms: Some(30_000),
            },
            McpAuth::OAuth {
                authenticated: true,
            },
        ));
        assert_eq!(
            detail,
            screen_detail(
                Transport::Http {
                    url: "https://mcp.example/sse".into(),
                },
                Auth::OAuth {
                    authenticated: true
                },
            )
        );
    }

    /// 凭据没有落脚的地方:屏幕这一份类型里没有一个字段装得下 headers 或环境
    /// 变量。契约的 `McpTransport` 也本来就不带它们(那个类型正是为此另立的),
    /// 这道判据拦的是「将来谁给屏幕那边加一个 headers/env 字段」那一下。
    ///
    /// 它钉的是**这一侧交出去的值的形状**,不是「host 没发秘密」——后者是契约
    /// 类型的形状保证的,不在这条判据的射程里。
    #[test]
    fn a_details_fields_leave_no_place_for_headers_or_env() {
        let detail = detail_of(wire_detail(
            McpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "server".into()],
                timeout_ms: None,
            },
            McpAuth::OAuth {
                authenticated: true,
            },
        ));
        let shown = format!("{detail:?}").to_lowercase();
        for word in [
            "header",
            "env",
            "token",
            "secret",
            "password",
            "authorization",
        ] {
            assert!(
                !shown.contains(word),
                "{word} 出现在交出去的详情里: {shown}"
            );
        }
    }

    /// 认证状态三种:不需要、已认证、需要认证。第三种是面板上最要紧的一种
    /// (它才让「认证」那个动作出现),不能和第一种混掉。
    #[test]
    fn auth_translates_as_none_or_whether_a_token_is_stored() {
        assert_eq!(auth(McpAuth::None), Auth::None);
        assert_eq!(
            auth(McpAuth::OAuth {
                authenticated: true
            }),
            Auth::OAuth {
                authenticated: true
            }
        );
        assert_eq!(
            auth(McpAuth::OAuth {
                authenticated: false
            }),
            Auth::OAuth {
                authenticated: false
            }
        );
    }

    /// 答对的那一趟:`McpRows` 就是屏幕的目录。
    #[test]
    fn a_rows_reply_becomes_the_screen_directory() {
        let reply = Ok(HostReply::McpRows {
            rows: vec![wire_row("fs", McpServerState::Connected, "global")],
        });
        let directory = listed(reply).expect("McpRows 就是一趟列表的答复");
        assert_eq!(directory.rows().len(), 1);
        assert_eq!(directory.rows()[0].name, "fs");
    }

    /// `McpDetail` 的那一趟。
    #[test]
    fn a_detail_reply_becomes_the_detail_page() {
        let reply = Ok(HostReply::McpDetail {
            detail: wire_detail(
                McpTransport::Http {
                    url: "https://mcp.example/sse".into(),
                    timeout_ms: None,
                },
                McpAuth::OAuth {
                    authenticated: false,
                },
            ),
        });
        let detail = detailed(reply).expect("McpDetail 就是一趟详情的答复");
        assert_eq!(detail.name, "figma");
        assert_eq!(detail.state, McpState::Connected);
    }

    /// 答了别的命令的话:按 `HostSaidSomethingElse` 说,并把宿主的话原样带上,
    /// 不装作这一趟成功,也不静默。
    #[test]
    fn a_reply_meant_for_another_command_is_refused_in_the_hosts_own_words() {
        let wrong = HostReply::ToolCatalog { tools: Vec::new() };

        let said = listed(Ok(wrong.clone())).expect_err("一份工具目录不是一趟列表的答复");
        assert_eq!(said, unexpected(&wrong));
        assert!(said.contains("ToolCatalog"), "{said}");

        let said = detailed(Ok(wrong.clone())).expect_err("一份工具目录不是一趟详情的答复");
        assert_eq!(said, unexpected(&wrong));
    }

    /// 宿主拒绝的话走的是**同一份**译法(`tui_tools::said`),不是这一侧另写的
    /// 一份:同一句拒绝在屏幕的两块地方必须说得一样。
    #[test]
    fn a_refusal_reads_exactly_as_tui_tools_says_it() {
        assert_eq!(
            listed(Err(HostError::NotFound)),
            Err(crate::tui_tools::said(HostError::NotFound))
        );
        assert_eq!(
            detailed(Err(HostError::Busy {
                reason: "a turn is running".into(),
            })),
            Err(crate::tui_tools::said(HostError::Busy {
                reason: "a turn is running".into(),
            }))
        );
    }
}
