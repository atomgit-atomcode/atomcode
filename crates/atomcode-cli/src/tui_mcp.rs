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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::mcp::{
    load_mcp_config_including_disabled, login_mcp_oauth_until, McpOAuthLoginOptions,
    McpOAuthLoginStop, McpServerConfig,
};
use atomcode_harness::seams::UiSvc;
use atomcode_host_api::{
    HostCommand, HostControl, HostError, HostReply, McpAction, McpAuth, McpServerDetail,
    McpServerState, McpTransport,
};
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::mcp::{Action, Auth, Mcp, McpDetail, McpState, McpView, Transport};
use atomcode_tui::module::{Modules, Mounted};
use atomcode_tui::plugin::{AgentClientSvc, McpSvc, ModulesSvc, RepaintSvc};
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
            .provide::<McpSvc>(Arc::new(McpPort {
                ctx: ctx.clone(),
                login: Arc::new(sign_in_by_browser),
                signing_in: std::sync::Mutex::new(None),
            }))
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
    /// How a server is signed in to. The browser flow in production; a test
    /// hands in one that does not open a browser.
    login: Login,
    /// The stop flag of the sign-in running now. Its own flag per sign-in, so
    /// a cancel stops that one and the next starts clean. Raised by
    /// [`Mcp::cancel`], and when the port goes away — a sign-in still waiting on
    /// a browser then gives up instead of holding a thread for a tab nobody
    /// will finish.
    signing_in: std::sync::Mutex<Option<Arc<AtomicBool>>>,
}

impl McpPort {
    fn raise_stop(&self) {
        if let Some(flag) = self.signing_in.lock().expect("sign-in poisoned").as_ref() {
            flag.store(true, Ordering::Release);
        }
    }
}

impl Drop for McpPort {
    fn drop(&mut self) {
        self.raise_stop();
    }
}

/// Sign in to one server: `announce` gets the authorization URL. Blocking —
/// it waits on a browser — so it is only ever called off the async runtime.
type Login = Arc<
    dyn Fn(&McpServerConfig, &McpOAuthLoginStop, &dyn Fn(&str)) -> Result<(), String> + Send + Sync,
>;

/// How long a sign-in gives the browser. Long enough for a second factor,
/// short enough that an abandoned tab does not hold a thread all session.
const SIGN_IN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

fn sign_in_by_browser(
    config: &McpServerConfig,
    stop: &McpOAuthLoginStop,
    announce: &dyn Fn(&str),
) -> Result<(), String> {
    login_mcp_oauth_until(
        config,
        McpOAuthLoginOptions::for_server(config),
        stop,
        announce,
    )
    .map(|_token| ())
    .map_err(|error| format!("{error:#}"))
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

    /// Say a line on the screen, and paint it now: a sign-in is waiting on the
    /// person, and the URL is what they are waiting for.
    fn sayer(&self) -> Arc<dyn Fn(String) + Send + Sync> {
        let ui = self.ctx.service::<UiSvc>();
        let repaint = self.ctx.service::<RepaintSvc>();
        Arc::new(move |line: String| {
            if let Some(ui) = ui.as_ref() {
                ui.say(&line);
            }
            if let Some(repaint) = repaint.as_ref() {
                repaint.now();
            }
        })
    }
}

#[async_trait]
impl Mcp for McpPort {
    fn cancel(&self) {
        self.raise_stop();
    }

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
        let Some(action) = wire_action(action) else {
            let cancel = Arc::new(AtomicBool::new(false));
            *self.signing_in.lock().expect("sign-in poisoned") = Some(Arc::clone(&cancel));
            let stop = McpOAuthLoginStop {
                cancel,
                timeout: SIGN_IN_TIMEOUT,
            };
            return sign_in(
                control.as_ref(),
                session,
                server,
                self.login.clone(),
                stop,
                self.sayer(),
            )
            .await;
        };
        listed(
            control
                .call(HostCommand::McpAct {
                    session,
                    server: server.to_string(),
                    action,
                })
                .await,
        )
    }
}

/// Sign in to `server` on this side, then have the host reconnect with the token.
///
/// Not a host command: signing in opens a browser and writes a token, and
/// touches nothing the running session owns — the same bargain `/openrouter`
/// strikes (`tui_openrouter.rs`). Two things follow from running it here:
///
/// - **the URL is said on the screen** through `say`, where a person can copy
///   it. This process's stdout *is* the screen, so printing it would land on
///   top of the frame at the moment it is needed;
/// - **the browser wait is on a thread of its own**, not the async runtime's:
///   it can take minutes, and a plain thread is not waited for when the
///   program exits.
///
/// The config is read from where the session works *now* (`Context` answers
/// it), not from where it started — `/cd` moves it.
async fn sign_in(
    control: &dyn HostControl,
    session: String,
    server: &str,
    login: Login,
    stop: McpOAuthLoginStop,
    say: Arc<dyn Fn(String) + Send + Sync>,
) -> Result<McpView, String> {
    let working_dir = match control
        .call(HostCommand::Context {
            session: session.clone(),
        })
        .await
    {
        Ok(HostReply::Context { working_dir, .. }) => std::path::PathBuf::from(working_dir),
        Ok(other) => return Err(unexpected(&other)),
        Err(error) => return Err(crate::tui_tools::said(error)),
    };
    let config = load_mcp_config_including_disabled(&working_dir)
        .map_err(|error| format!("{error:#}"))?
        .into_iter()
        .find(|config| config.name == server)
        .ok_or_else(|| tr(SMsg::McpServerNotConfigured { server }).into_owned())?;

    let (done, answer) = tokio::sync::oneshot::channel();
    let name = server.to_string();
    let cancelled = Arc::clone(&stop.cancel);
    std::thread::spawn(move || {
        let announce = |url: &str| say(tr(SMsg::McpLoginUrl { server: &name, url }).into_owned());
        let _ = done.send(login(&config, &stop, &announce));
    });
    answer
        .await
        .map_err(|_| tr(SMsg::McpSignInLost).into_owned())?
        .map_err(|error| {
            // Stopped because the person asked: say that in their words, not
            // the library's account of an interrupted wait.
            if cancelled.load(Ordering::Acquire) {
                tr(SMsg::McpSignInCancelled).into_owned()
            } else {
                error
            }
        })?;

    // The token is on disk; the session connects with it only when rebuilt.
    match control
        .call(HostCommand::Reload {
            session: session.clone(),
        })
        .await
    {
        Ok(_) => {}
        // A turn is running. The sign-in is not lost — say what is left to do.
        Err(HostError::Busy { .. }) => {
            return Err(tr(SMsg::McpSignedInReloadLater { server }).into_owned())
        }
        Err(error) => return Err(crate::tui_tools::said(error)),
    }
    listed(control.call(HostCommand::McpManage { session }).await)
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

/// 屏幕的动作译成契约的动作。两边取同一个名字、同一件事(`McpAction` 的
/// `Enable`/`Disable` 注释钉过方向:名字是人这一侧的)。
///
/// `None` 是「认证」:它不是宿主命令,在这一侧跑([`sign_in`])。
fn wire_action(action: Action) -> Option<McpAction> {
    match action {
        Action::Trust => Some(McpAction::Trust),
        Action::Untrust => Some(McpAction::Untrust),
        Action::Login => None,
        Action::Logout => Some(McpAction::Logout),
        Action::Enable => Some(McpAction::Enable),
        Action::Disable => Some(McpAction::Disable),
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

    /// 五个走宿主的动作一一对应,「认证」不走宿主。写反一个方向,人就按着「停用」
    /// 把服务器启用了(`McpAction` 的注释专门钉过这一点)。
    #[test]
    fn each_screen_action_translates_to_the_wire_action_of_the_same_name() {
        use atomcode_tui::mcp::Action as A;
        let cases = [
            (A::Trust, Some(McpAction::Trust)),
            (A::Untrust, Some(McpAction::Untrust)),
            // Signed in on this side, never sent as a host command.
            (A::Login, None),
            (A::Logout, Some(McpAction::Logout)),
            (A::Enable, Some(McpAction::Enable)),
            (A::Disable, Some(McpAction::Disable)),
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

    /// A host that records what it was asked, and answers `Context` with a
    /// working directory and `Reload` as told.
    struct SignInHost {
        working_dir: std::path::PathBuf,
        reload: Result<HostReply, HostError>,
        asked: std::sync::Mutex<Vec<&'static str>>,
    }

    #[async_trait]
    impl HostControl for SignInHost {
        async fn call(&self, command: HostCommand) -> Result<HostReply, HostError> {
            let (name, reply) = match command {
                HostCommand::Context { .. } => (
                    "Context",
                    Ok(HostReply::Context {
                        window: 0,
                        used: 0,
                        model: "m".into(),
                        working_dir: self.working_dir.to_string_lossy().into_owned(),
                    }),
                ),
                HostCommand::Reload { .. } => ("Reload", self.reload.clone()),
                HostCommand::McpManage { .. } => {
                    ("McpManage", Ok(HostReply::McpRows { rows: Vec::new() }))
                }
                HostCommand::McpAct { .. } => ("McpAct", Ok(HostReply::Done)),
                _ => ("other", Ok(HostReply::Done)),
            };
            self.asked.lock().unwrap().push(name);
            reply
        }
        fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<atomcode_host_api::HostEvent> {
            tokio::sync::mpsc::unbounded_channel().1
        }
    }

    /// A project whose `.mcp.json` has one OAuth server, `remote`.
    fn project_with_an_oauth_server() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".mcp.json"),
            r#"{"mcpServers":{"remote":{"url":"https://example.invalid/mcp","auth":{"type":"oauth"}}}}"#,
        )
        .unwrap();
        dir
    }

    fn stop() -> McpOAuthLoginStop {
        McpOAuthLoginStop {
            cancel: Arc::new(AtomicBool::new(false)),
            timeout: std::time::Duration::from_secs(5),
        }
    }

    const URL: &str = "https://auth.example.invalid/authorize?state=s1";

    /// Signing in from the panel says the URL on the screen and reconnects.
    ///
    /// The URL used to be `println!`ed by the library, which behind
    /// `atomcode --tui` wrote it over the frame; here it has to reach the
    /// screen's own `say`, whole. The browser wait has to be off the async
    /// runtime — it can take minutes — and the token only reaches the session
    /// through the `Reload` that follows it.
    #[tokio::test]
    async fn signing_in_says_the_url_on_the_screen_and_reconnects() {
        let project = project_with_an_oauth_server();
        let host = SignInHost {
            working_dir: project.path().to_path_buf(),
            reload: Ok(HostReply::Done),
            asked: Default::default(),
        };
        let off_the_runtime = Arc::new(AtomicBool::new(false));
        let login: Login = {
            let off_the_runtime = Arc::clone(&off_the_runtime);
            Arc::new(move |config, _stop, announce| {
                assert_eq!(config.name, "remote", "the server asked about is signed in");
                off_the_runtime.store(
                    tokio::runtime::Handle::try_current().is_err(),
                    Ordering::Release,
                );
                announce(URL);
                Ok(())
            })
        };
        let said = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let say: Arc<dyn Fn(String) + Send + Sync> = {
            let said = Arc::clone(&said);
            Arc::new(move |line| said.lock().unwrap().push(line))
        };

        sign_in(&host, "s".into(), "remote", login, stop(), say)
            .await
            .expect("signed in");

        let said = said.lock().unwrap();
        assert!(
            said.iter().any(|line| line.contains(URL)),
            "the URL is said on the screen, whole: {said:?}"
        );
        assert!(
            off_the_runtime.load(Ordering::Acquire),
            "the browser wait ran on the async runtime, where it holds a worker"
        );
        assert_eq!(
            *host.asked.lock().unwrap(),
            vec!["Context", "Reload", "McpManage"],
            "where the session works, then the reconnect, then the list after it"
        );
    }

    /// A sign-in that finishes while a turn runs keeps the token and says what
    /// is left to do — not a bare "busy" that reads as if it had failed.
    #[tokio::test]
    async fn a_sign_in_during_a_turn_keeps_the_token_and_says_to_reload() {
        let project = project_with_an_oauth_server();
        let host = SignInHost {
            working_dir: project.path().to_path_buf(),
            reload: Err(HostError::Busy {
                reason: "a turn is running".into(),
            }),
            asked: Default::default(),
        };
        let signed = Arc::new(AtomicBool::new(false));
        let login: Login = {
            let signed = Arc::clone(&signed);
            Arc::new(move |_config, _stop, _announce| {
                signed.store(true, Ordering::Release);
                Ok(())
            })
        };
        let error = sign_in(&host, "s".into(), "remote", login, stop(), Arc::new(|_| {}))
            .await
            .expect_err("the reconnect has to wait");
        assert!(signed.load(Ordering::Acquire), "the sign-in itself ran");
        assert!(
            error.contains("/mcp reload"),
            "it says how to finish: {error}"
        );
    }

    /// A server the config does not define is refused before any browser opens.
    #[tokio::test]
    async fn signing_in_to_an_unknown_server_opens_nothing() {
        let project = project_with_an_oauth_server();
        let host = SignInHost {
            working_dir: project.path().to_path_buf(),
            reload: Ok(HostReply::Done),
            asked: Default::default(),
        };
        let login: Login = Arc::new(|_config, _stop, _announce| {
            panic!("no sign-in for a server that is not configured")
        });
        let error = sign_in(&host, "s".into(), "nope", login, stop(), Arc::new(|_| {}))
            .await
            .expect_err("unknown server");
        assert!(error.contains("nope"), "{error}");
        assert_eq!(*host.asked.lock().unwrap(), vec!["Context"], "no reconnect");
    }

    /// A sign-in the person cancelled says it was cancelled, in their words.
    ///
    /// The login gives up when its stop flag is raised (`Esc` on the panel);
    /// what comes back is the library's account of an interrupted wait, which is
    /// not what the person did. And nothing is reconnected: there is no token.
    #[tokio::test]
    async fn a_cancelled_sign_in_says_so_and_reconnects_nothing() {
        let project = project_with_an_oauth_server();
        let host = SignInHost {
            working_dir: project.path().to_path_buf(),
            reload: Ok(HostReply::Done),
            asked: Default::default(),
        };
        let login: Login = Arc::new(|_config, stop, _announce| {
            // What the person does while the browser is open.
            stop.cancel.store(true, Ordering::Release);
            Err("MCP OAuth login was cancelled before the browser came back".into())
        });
        let error = sign_in(&host, "s".into(), "remote", login, stop(), Arc::new(|_| {}))
            .await
            .expect_err("cancelled");
        assert_eq!(error, tr(SMsg::McpSignInCancelled));
        assert_eq!(*host.asked.lock().unwrap(), vec!["Context"], "no reconnect");
    }
}
