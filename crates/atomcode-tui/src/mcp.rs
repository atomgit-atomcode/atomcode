//! MCP 面板:模型与按键。
//!
//! 和 `/toolbox`、`/resume` 是同一件事的一块——人要在里面待一会儿、来回钻——所以
//! 同一副骨架:从底下升起来占住输入框的位置。
//!
//! 与 `/toolbox` 不同的是它是**两级**的:列表看全局,`Enter` 钻进去看一个服务器的
//! 配置与诊断。`Esc` 在两级上的意思不同(详情→列表,列表→关掉),所以 `key()` 按级分派。
//!
//! 住在这儿的是**数据和按键**。有哪些服务器、动作怎么落下去,由 [`Mcp`] 端口出去
//! (`docs/adr/0022` §3):这个模块不知道 host 命令长什么样,也不知道一趟要过几层。

use crate::caps::Glyph;
use crate::i18n::{t, Msg};
use crate::surface::{Key, KeyPress, Mods};

/// 一个服务器此刻是什么状态。这一层不认 `atomcode_host_api`,另立一份
/// (理由同 [`crate::tools::State`],`docs/adr/0022` §3)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpState {
    Connecting,
    Connected,
    /// HTTP + OAuth,且没存下可用凭据:要人去认证一次。
    NeedsAuthentication,
    /// 配置里写死了停用。人选的,列出来是为了让「启用」够得着。
    Disabled,
    /// 这个项目还没被信任。
    Untrusted,
    Failed(String),
    Disconnected,
}

impl McpState {
    /// 行首那个记号,**按含义**要、不按字符要:字面的 `⚠` 在纯 ASCII 终端上是豆腐块,
    /// 屏蔽层知道该换成什么(`crate::caps`)。
    ///
    /// 三个"要人做点什么"的状态(连接中 / 待认证 / 未信任)共用 `Pending`:记号只分
    /// 「好、坏、等着、关着」四类,具体是什么由紧跟其后的状态词说。
    pub fn glyph(&self) -> Glyph {
        match self {
            Self::Connected => Glyph::Ok,
            Self::Connecting | Self::NeedsAuthentication | Self::Untrusted => Glyph::Pending,
            Self::Disabled | Self::Disconnected => Glyph::Hollow,
            Self::Failed(_) => Glyph::Fail,
        }
    }

    pub fn about(&self) -> String {
        match self {
            Self::Connecting => t(Msg::McpConnecting),
            Self::Connected => t(Msg::McpConnected),
            Self::NeedsAuthentication => t(Msg::McpNeedsAuthentication),
            Self::Disabled => t(Msg::McpDisabled),
            Self::Untrusted => t(Msg::McpUntrusted),
            Self::Failed(message) => t(Msg::McpFailed {
                message: message.as_str(),
            }),
            Self::Disconnected => t(Msg::McpDisconnected),
        }
        .into_owned()
    }
}

/// 怎么连。**只带地址本身**:headers 与 auth 材料是凭据住的地方,屏幕不该拿到
/// (设计 §4.2;wire 层为同一个理由另立了 `McpTransport`)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transport {
    Stdio { command: String, args: Vec<String> },
    Http { url: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Auth {
    None,
    OAuth { authenticated: bool },
}

/// 列表里的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpRow {
    pub name: String,
    pub state: McpState,
    /// 来源:`"global"` / `"project"` / `"driver"`。分组就按它,不再另立枚举——
    /// 屏幕只是把同值的排在一起,不需要知道它们意味着什么。
    pub source: String,
    pub tool_count: usize,
    pub config_path: Option<String>,
}

/// 详情页要看的东西。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpDetail {
    pub name: String,
    pub state: McpState,
    pub source: String,
    pub transport: Transport,
    pub auth: Auth,
    pub tool_count: usize,
    pub config_path: Option<String>,
}

/// 一次动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Trust,
    Untrust,
    Login,
    Logout,
    Enable,
    Disable,
}

impl Action {
    pub fn about(self) -> String {
        match self {
            Self::Trust => t(Msg::McpActionTrust),
            Self::Untrust => t(Msg::McpActionUntrust),
            Self::Login => t(Msg::McpActionLogin),
            Self::Logout => t(Msg::McpActionLogout),
            Self::Enable => t(Msg::McpActionEnable),
            Self::Disable => t(Msg::McpActionDisable),
        }
        .into_owned()
    }
}

/// 面包屑所处的层级。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    List,
    Detail,
}

impl Default for Level {
    fn default() -> Self {
        Self::List
    }
}

/// 目录,以及打开着的那一份详情。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpView {
    rows: Vec<McpRow>,
    detail: Option<McpDetail>,
}

impl McpView {
    /// 行按 (来源, 名字) 排——分组与行内顺序都稳定,不随 host 的返回顺序变。
    pub fn new(mut rows: Vec<McpRow>) -> Self {
        rows.sort_by(|a, b| {
            (a.source.as_str(), a.name.as_str()).cmp(&(b.source.as_str(), b.name.as_str()))
        });
        Self { rows, detail: None }
    }

    pub fn with_detail(mut self, detail: McpDetail) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn rows(&self) -> &[McpRow] {
        &self.rows
    }

    pub fn detail(&self) -> Option<&McpDetail> {
        self.detail.as_ref()
    }

    /// 这一台的那份详情,不是这一台就当没有。
    ///
    /// `for_` 是面板正在看的那一台([`Panel::detail_for`])。过滤而不是断言,
    /// 因为「回包还没到」本来就是常态:调用方拿到的 `None` 有两重意思——正在等,
    /// 或者手里那份属于别人——而两者该有同一种表现:画「正在取详情…」,不画
    /// 别人的页面,也不拿别人的动作去打。
    pub fn detail_for(&self, for_: Option<&str>) -> Option<&McpDetail> {
        self.detail
            .as_ref()
            .filter(|d| for_ == Some(d.name.as_str()))
    }

    /// 按来源分组,来源之间按名字排。空目录给空表。
    pub fn grouped(&self) -> Vec<(&str, Vec<&McpRow>)> {
        let mut out: Vec<(&str, Vec<&McpRow>)> = Vec::new();
        for row in &self.rows {
            match out.last_mut() {
                Some((source, group)) if *source == row.source.as_str() => group.push(row),
                _ => out.push((row.source.as_str(), vec![row])),
            }
        }
        out
    }

    /// 过滤之后列出来的那些:名字与来源都能搜到。
    pub fn listed(&self, panel: &Panel) -> Vec<&McpRow> {
        let q = panel.query.trim().to_lowercase();
        self.rows
            .iter()
            .filter(|r| {
                q.is_empty()
                    || r.name.to_lowercase().contains(&q)
                    || r.source.to_lowercase().contains(&q)
            })
            .collect()
    }
}

/// 有活在外面跑着:一次动作是一趟往返,而认证可能等上几分钟。回来之前屏上要有话说。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Busy {
    pub what: String,
}

/// 面板开着的时候。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Panel {
    pub level: Level,
    /// 光标停在 *listed* 的第几行。
    pub cursor: usize,
    pub query: String,
    /// 上一次操作之后要说的一句话,比如「配置里有注释,写不进去」。
    pub note: Option<String>,
    pub busy: Option<Busy>,
    /// 详情页在看**哪一台**——正在等的那一台也算。
    ///
    /// 详情有名字,而视图里压着的那一份不一定属于当前这一台:人从 A 退回来、
    /// 立刻对 B 按下 Enter 时,B 的回包还没到,视图里仍是 A 的那一份。照名字核
    /// 一遍,那一份就既画不出来也按不动——否则 B 的页面上按下的动作会打到 A。
    pub detail_for: Option<String>,
    /// 进详情之前光标停在列表的哪一行,退回来时站回原处。
    pub list_at: usize,
    /// 上一次点击落在**哪一级的哪一行**。
    ///
    /// 鼠标按两次才算动手(见 `Host::mcp_click`),而「同一个格」不能只比行号:
    /// 进出详情会让同一格底下换一套东西——列表第 8 行是第 3 台服务器,详情第 8 行
    /// 是第 1 个动作。只比行号的话,双击里落空的那一下就会变成一次危险动作。
    pub clicked: Option<(Level, usize)>,
}

impl Panel {
    pub fn new() -> Self {
        Self::default()
    }

    /// 这一个服务器此刻能做的动作,顺序即屏幕上的顺序(设计 §5.2)。
    ///
    /// 动作是**随状态现算**的,不是固定六项:给一个不可用的动作,人按下去只会
    /// 得到一次失败——不如不给。
    pub fn actions(&self, detail: &McpDetail) -> Vec<Action> {
        let mut out = Vec::new();
        match detail.state {
            McpState::Disabled => {
                out.push(Action::Enable);
            }
            McpState::Untrusted => {
                out.push(Action::Trust);
                out.push(Action::Disable);
            }
            McpState::NeedsAuthentication => {
                out.push(Action::Login);
                out.push(Action::Disable);
            }
            _ => {
                if let Auth::OAuth {
                    authenticated: true,
                } = detail.auth
                {
                    out.push(Action::Logout);
                }
                out.push(Action::Disable);
            }
        }
        // `Untrust` 是**整项目级**的动作(设计 §5.2):这一台来自项目、而项目还是信任的
        // (这正是它跑着的原因),它就该够得着。§5.2 把这一个动作挂在列表的项目组标题上,
        // 面板还没有可选的组标题,所以先挂在每一台项目服务器上——语义一样,位置不同。
        let project_is_trusted = detail.source == "project" && detail.state != McpState::Untrusted;
        if project_is_trusted {
            // 「停用」在每一支里都排最后,所以取消信任插在它前面。
            let before = match out.last() {
                Some(Action::Disable) => out.len() - 1,
                _ => out.len(),
            };
            out.insert(before, Action::Untrust);
        }
        out
    }

    /// 指到第几行,夹在列出来的范围里。变了才返回 true。
    pub fn point_at(&mut self, row: usize, rows: usize) -> bool {
        let want = row.min(rows.saturating_sub(1));
        if self.cursor == want {
            return false;
        }
        self.cursor = want;
        self.note = None;
        true
    }
}

/// 一次按键要面板的主人去做的事。凡是碰得到外面世界的都在这儿,不在 [`key`] 里。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// 面板自己动了,外面没有。
    Stay,
    /// 收起来。
    Close,
    /// 去取这一个服务器的详情。
    OpenDetail { server: String },
    /// 对这一个服务器做这一件事。
    Act { server: String, action: Action },
}

/// 一次按键。纯函数:面板改自己,要外面做的事从返回值出去。
pub fn key(view: &McpView, panel: &mut Panel, press: KeyPress) -> Step {
    // 有活在跑:只认 Esc。别的键按下去会派出第二次动作,而第一次还没回来。
    if panel.busy.is_some() {
        return match (press.key, press.mods) {
            (Key::Esc, _) => {
                panel.busy = None;
                Step::Close
            }
            _ => Step::Stay,
        };
    }

    if panel.level == Level::Detail {
        return detail_key(view, panel, press);
    }

    let listed = view.listed(panel).len();
    match (press.key, press.mods) {
        (Key::Esc, _) => Step::Close,
        (Key::Up, _) | (Key::Char('p'), Mods::CTRL) => {
            panel.note = None;
            panel.cursor = panel.cursor.saturating_sub(1);
            Step::Stay
        }
        (Key::Down, _) | (Key::Char('n'), Mods::CTRL) => {
            panel.note = None;
            if panel.cursor + 1 < listed {
                panel.cursor += 1;
            }
            Step::Stay
        }
        (Key::Enter, _) => {
            panel.note = None;
            let Some(row) = view.listed(panel).get(panel.cursor).copied() else {
                return Step::Stay;
            };
            let server = row.name.clone();
            panel.level = Level::Detail;
            panel.detail_for = Some(server.clone());
            panel.list_at = panel.cursor;
            panel.cursor = 0;
            Step::OpenDetail { server }
        }
        (Key::Backspace, _) => {
            panel.note = None;
            panel.query.pop();
            panel.cursor = 0;
            Step::Stay
        }
        (Key::Char('u'), Mods::CTRL) => {
            panel.note = None;
            panel.query.clear();
            panel.cursor = 0;
            Step::Stay
        }
        (Key::Char(c), Mods::NONE | Mods::SHIFT) => {
            panel.note = None;
            panel.query.push(c);
            panel.cursor = 0;
            Step::Stay
        }
        _ => Step::Stay,
    }
}

/// 详情层:光标在**动作**之间走,`Esc` 回列表。
/// 退回列表:光标站回进详情前那一行,并忘掉刚才在看哪一台。
///
/// 忘掉是必须的:留着它,下一台的回包到达之前,上一台的详情会被当成「这一台的」
/// 挂上去——名字核对正是照它做的。
fn back_to_list(panel: &mut Panel) {
    panel.level = Level::List;
    panel.detail_for = None;
    panel.cursor = panel.list_at;
}

fn detail_key(view: &McpView, panel: &mut Panel, press: KeyPress) -> Step {
    // 只认**这一台**的详情。`None` 有两重意思——回包还没到,或者手里那份属于
    // 上一台——而两者该有同一种表现:画「正在取详情…」,不画别人的页面,更不拿
    // 别人的动作去打(停用、登出、取消信任都是破坏性的)。
    let Some(detail) = view.detail_for(panel.detail_for.as_deref()) else {
        // 这时 Esc 仍要能退回去,否则一次慢往返(或一次错配)会把面板卡在空白的
        // 详情页上。
        return match press.key {
            Key::Esc => {
                back_to_list(panel);
                Step::Stay
            }
            _ => Step::Stay,
        };
    };
    let actions = panel.actions(detail);
    match (press.key, press.mods) {
        (Key::Esc, _) => {
            panel.note = None;
            back_to_list(panel);
            Step::Stay
        }
        (Key::Up, _) | (Key::Char('p'), Mods::CTRL) => {
            panel.note = None;
            panel.cursor = panel.cursor.saturating_sub(1);
            Step::Stay
        }
        (Key::Down, _) | (Key::Char('n'), Mods::CTRL) => {
            panel.note = None;
            if panel.cursor + 1 < actions.len() {
                panel.cursor += 1;
            }
            Step::Stay
        }
        (Key::Enter, _) | (Key::Char(' '), Mods::NONE) => {
            panel.note = None;
            let Some(action) = actions.get(panel.cursor).copied() else {
                return Step::Stay;
            };
            let server = detail.name.clone();
            panel.busy = Some(Busy {
                what: action.about(),
            });
            Step::Act { server, action }
        }
        _ => Step::Stay,
    }
}

/// 粘贴进过滤框:一个服务器名常常是复制来的。
pub fn paste(panel: &mut Panel, text: &str) -> bool {
    if panel.busy.is_some() || panel.level == Level::Detail {
        return false;
    }
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    if text.is_empty() {
        return false;
    }
    panel.query.push_str(&text);
    panel.cursor = 0;
    panel.note = None;
    true
}

/// 服务器从哪儿来,动作往哪儿去。
///
/// 实现住在装配它的那一侧(cli),因为「有哪些服务器、能不能改」是运行中那棵树的
/// 事,而屏幕不许伸手进 agent 的 App(`docs/adr/0022` §3)。
#[async_trait::async_trait]
pub trait Mcp: Send + Sync {
    /// 目录此刻的样子,停用项也在里面。
    async fn list(&self) -> Result<McpView, String>;

    /// 一个服务器的详情。
    async fn detail(&self, server: &str) -> Result<McpDetail, String>;

    /// 做一件事,答的是**之后**的目录——屏上画的是发生过的事,不是自己以为
    /// 发生了的事(与 `/toolbox` 的 `switch` 同一个道理)。
    async fn act(&self, server: &str, action: Action) -> Result<McpView, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key: Key) -> KeyPress {
        KeyPress::plain(key)
    }

    /// 一台服务器:连着,来源一样,只有名字是这一次要的。
    fn row(name: &str) -> McpRow {
        McpRow {
            name: name.to_string(),
            state: McpState::Connected,
            source: "global".to_string(),
            tool_count: 0,
            config_path: None,
        }
    }

    /// 一份目录,按参数里的顺序。
    fn view_of(names: &[&str]) -> McpView {
        McpView::new(names.iter().copied().map(row).collect())
    }

    /// 一台**用户级**服务器的详情页,只有状态与认证是这一次要的。
    fn detail(state: McpState, auth: Auth) -> McpDetail {
        detail_from("global", state, auth)
    }

    /// 同一张详情页,来源也由这一次要的说。
    fn detail_from(source: &str, state: McpState, auth: Auth) -> McpDetail {
        McpDetail {
            name: "figma".to_string(),
            state,
            source: source.to_string(),
            transport: Transport::Http {
                url: "https://mcp.figma.com/mcp".into(),
            },
            auth,
            tool_count: 0,
            config_path: None,
        }
    }

    #[test]
    fn enter_drills_into_the_selected_server_and_esc_comes_back() {
        let view = view_of(&["alpha", "beta", "gamma"]);
        let mut panel = Panel::new();
        panel.cursor = 1;

        // Enter opens the one under the cursor — not the first, not the last.
        assert_eq!(
            key(&view, &mut panel, press(Key::Enter)),
            Step::OpenDetail {
                server: "beta".into()
            }
        );
        assert_eq!(panel.level, Level::Detail);

        // Esc goes back to the list, it does not close the panel.
        assert_eq!(key(&view, &mut panel, press(Key::Esc)), Step::Stay);
        assert_eq!(panel.level, Level::List);

        // A second Esc, now on the list, is what closes it.
        assert_eq!(key(&view, &mut panel, press(Key::Esc)), Step::Close);
    }

    #[test]
    fn the_detail_actions_are_the_ones_this_server_can_take() {
        let mut panel = Panel::new();
        panel.level = Level::Detail;

        // Connected and authenticated: turn off, or sign out.
        let connected = detail(
            McpState::Connected,
            Auth::OAuth {
                authenticated: true,
            },
        );
        assert_eq!(
            panel.actions(&connected),
            vec![Action::Logout, Action::Disable]
        );

        // Waiting for a person: authenticate it, or switch it off.
        let waiting = detail(
            McpState::NeedsAuthentication,
            Auth::OAuth {
                authenticated: false,
            },
        );
        assert_eq!(
            panel.actions(&waiting),
            vec![Action::Login, Action::Disable]
        );

        // Already off: the only way is back on.
        let off = detail(McpState::Disabled, Auth::None);
        assert_eq!(panel.actions(&off), vec![Action::Enable]);
    }

    /// `Untrust` 是整项目级的动作(设计 §5.2):项目还信任着的时候要够得着,并且排在
    /// 这一台自己的动作之后、「停用」之前;用户级的那一台与项目信任无关,不给。
    #[test]
    fn a_trusted_project_server_can_be_untrusted_and_a_user_level_one_cannot() {
        let mut panel = Panel::new();
        panel.level = Level::Detail;

        // 项目信任着——这一台正连着,所以「取消信任」够得着。
        let project = detail_from(
            "project",
            McpState::Connected,
            Auth::OAuth {
                authenticated: true,
            },
        );
        let actions = panel.actions(&project);
        assert!(actions.contains(&Action::Untrust), "{actions:?}");
        assert_eq!(
            actions,
            vec![Action::Logout, Action::Untrust, Action::Disable]
        );

        // 项目还没被信任:这里该给的是「信任」,不是「取消信任」。
        let untrusted = detail_from("project", McpState::Untrusted, Auth::None);
        assert_eq!(
            panel.actions(&untrusted),
            vec![Action::Trust, Action::Disable]
        );

        // 用户级的那一台:项目信任与它无关,一个都不给。
        let global = detail(McpState::Connected, Auth::None);
        assert_eq!(panel.actions(&global), vec![Action::Disable]);
    }
}
