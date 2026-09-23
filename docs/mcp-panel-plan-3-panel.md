# /mcp 面板 —— 面板层实施计划（3/3）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `/mcp` 从一列文本做成一个**能看、能钻、能改**的面板：按来源分组的列表 → `Enter` 钻进详情 → 在详情里认证/登出/启用/停用/信任。

**Architecture:** 照 `/toolbox` 的三层骨架，一模一样：

| 层 | 文件 | 住着什么 |
| --- | --- | --- |
| 模型 + 按键 | `crates/atomcode-tui/src/mcp.rs`（新建） | `McpPanel` / `Level` / `Step` / `key()`（**纯函数**，碰外面的事从 `Step` 出去）/ `Mcp` 端口 trait |
| 绘制 | `crates/atomcode-tui/src/modules/mcp.rs`（新建） | `impl View` + `Row`/`layout`/`geometry`，只画 |
| 端口实现 | `crates/atomcode-cli/src/tui_mcp.rs`（新建） | `impl Mcp for McpPort`，把面板的话翻译成 host 命令 |

`Moment` 上挂两个字段（面板与数据），`host.rs` 给一组 `toggle_mcp`/`close_mcp`/`show_mcp`/`mcp_busy`/`mcp_note` 并接上按键路由。

**Tech Stack:** Rust、`cargo nextest`、`atomcode-tui` 自己的 i18n 表（`atomcode-i18n/src/screen/`）。

**前置：** worktree `/Users/lichao/project/gitcode/ai/atomcode/.worktrees/mcp-panel`、分支 `feat/mcp-panel`，且**计划 2（读路径）与 2b（写路径）都已落地**——面板把它们的命令当端口用。

**测试命令（这一层要跑两个 crate）：**

| crate | 命令 |
| --- | --- |
| `atomcode-tui` | `cargo nextest run -p atomcode-tui` |
| `atomcode-i18n` | `cargo nextest run -p atomcode-i18n` |
| `atomcode`（cli，目录叫 `atomcode-cli`） | `cargo nextest run -p atomcode` |

**不要在 worktree 里把 `CARGO_TARGET_DIR` 指到主检出的 `target/`**——两棵树包名/版本/相对路径全同、内容不同，会脏产物串用，症状是报"源码里明明存在的符号找不到"而 `cargo tree` 看着完全正确。（本分支实测过。）

**设计依据：** `docs/mcp-panel-design.md` §4.2（字段映射）、§5.1（列表层）、§5.2（详情层与动作随状态）、§5.3（按键）、§5.4（动作之后怎么刷新）、§6（失败语义）、§8（明确不做）。

---

## File Structure

| 文件 | 动作 | 负责什么 |
| --- | --- | --- |
| `crates/atomcode-tui/src/mcp.rs` | 新建 | 模型与按键（纯），`Mcp` 端口 trait |
| `crates/atomcode-tui/src/lib.rs` | 修改 | `pub mod mcp;`（照 `pub mod tools;` 的位置） |
| `crates/atomcode-tui/src/modules/mcp.rs` | 新建 | `impl View`，绘制列表层与详情层 |
| `crates/atomcode-tui/src/modules/mod.rs` | 修改 | `pub mod mcp;` |
| `crates/atomcode-tui/src/plugin.rs` | 修改 | `McpSvc` 这个 seam（照 `ToolCatalogSvc`），端口靠它挂进来 |
| `crates/atomcode-tui/src/moment.rs` | 修改 | `mcp_panel: Option<crate::mcp::Panel>` 与 `mcp: crate::mcp::McpView` |
| `crates/atomcode-tui/src/host.rs` | 修改 | `toggle_mcp` / `close_mcp` / `show_mcp` / `mcp_busy` / `mcp_note` + 按键路由 |
| `crates/atomcode-tui/src/commands.rs` | 修改 | `/mcp` 无参 → 打开面板（有参仍走原来的开关命令） |
| `crates/atomcode-cli/src/tui_mcp.rs` | 新建 | `impl Mcp for McpPort` |
| `crates/atomcode-cli/src/main.rs` | 修改 | 装配这个端口（照 `tui_tools` 的挂法） |
| `crates/atomcode-i18n/src/screen/{messages,en,zh_cn}.rs` | 修改 | 面板标题、按键图例、状态词、动作名、提示 |

**为什么模型层自己再定义一份类型**：`crates/atomcode-tui/src/tools.rs` 开头的注释写明"这一层不认 `atomcode_host_api`（`docs/adr/0022` §3）"，所以它有自己的 `State`/`ToolRow`。面板照它办——`crate::mcp` 用自己的 `McpState`/`McpRow`/`McpDetail`，由端口实现那一层做映射。**不要**为了省事把 host-api 的类型直接引进来（`modules/settings.rs` 那么做过，但那是统计页，不是面板）。

---

## Task 1: 先加文案（否则后面一步都编译不过）

模型层要引用一批还不存在的 `Msg`,所以文案必须**在最前面**加。`atomcode-tui` 的 i18n 是薄薄一层转发（`crates/atomcode-tui/src/i18n/mod.rs` 只有 `pub use atomcode_i18n::screen::*;`），表本身在 `atomcode-i18n/src/screen/`。

**加一条的步骤（该 crate 自己的文档注释写明的，是硬要求）：** 在 `messages.rs` 加变体，在 `en.rs` 加一臂，在 `zh_cn.rs` 加一臂。**那个 `match` 是穷尽的——缺一条翻译就编译不过**，所以这一步没有"忘了加"的可能。措辞风格见 `docs/i18n-style.md`。

**Files:**
- Modify: `crates/atomcode-i18n/src/screen/messages.rs`
- Modify: `crates/atomcode-i18n/src/screen/en.rs`
- Modify: `crates/atomcode-i18n/src/screen/zh_cn.rs`
- Test: 靠穷尽 match 本身当判据（缺一臂即编译失败）

- [ ] **Step 1: 加变体**

`messages.rs` 里，`McpDisabled` 那一组附近加（**注意不要和已有的 `Mcp*` 重名**——已有的是 `McpConnecting`/`McpConnected`/`McpUntrusted`/`McpNeedsAuthentication`/`McpFailed`/`McpDisconnected`/`McpDisabled`/`McpUnknownState`/`McpNoneConfigured` 等）：

```rust
    // 面板
    McpPanelTitle,
    McpPanelServers { n: usize },
    McpPanelEmpty,
    McpDetailPending,

    // 按来源分组（host 给的是 "global"/"project"/"driver"）
    McpGroupGlobal,
    McpGroupProject,
    McpGroupDriver,

    // 详情页那四行标签
    McpLabelState,
    McpLabelAuth,
    McpLabelEndpoint,
    McpLabelSource,
    McpLabelTools { n: usize },

    // 认证
    McpAuthNone,
    McpAuthAuthenticated,
    McpAuthNotAuthenticated,

    // 六个动作
    McpActionTrust,
    McpActionUntrust,
    McpActionLogin,
    McpActionLogout,
    McpActionEnable,
    McpActionDisable,

    // 按键图例
    McpLegendList,
    McpLegendDetail,
    McpLegendBusy,
```

- [ ] **Step 2: 两个语言各加一臂**

`en.rs` / `zh_cn.rs` 各加 24 臂，照邻居的形状（`Msg::McpTallyFailed { n }` 那种 `format!` 用法）。建议措辞：

| 变体 | zh | en |
| --- | --- | --- |
| `McpPanelTitle` | `管理 MCP 服务器` | `Manage MCP servers` |
| `McpPanelServers{n}` | `{n} 个服务器` | `{n} servers` |
| `McpPanelEmpty` | `没有配置任何 MCP 服务器` | `No MCP servers configured` |
| `McpDetailPending` | `正在取详情…` | `Fetching details…` |
| `McpGroupGlobal` | `全局` | `Global` |
| `McpGroupProject` | `项目` | `Project` |
| `McpGroupDriver` | `外部传入` | `Supplied by the client` |
| `McpLabelState` | `状态` | `Status` |
| `McpLabelAuth` | `认证` | `Auth` |
| `McpLabelEndpoint` | `地址` | `Endpoint` |
| `McpLabelSource` | `来源` | `Config location` |
| `McpLabelTools{n}` | `工具 {n} 个` | `{n} tools` |
| `McpAuthNone` | `不需要` | `Not required` |
| `McpAuthAuthenticated` | `已认证` | `authenticated` |
| `McpAuthNotAuthenticated` | `未认证` | `not authenticated` |
| `McpActionTrust` | `信任这个项目` | `Trust this project` |
| `McpActionUntrust` | `取消信任` | `Untrust` |
| `McpActionLogin` | `认证` | `Authenticate` |
| `McpActionLogout` | `登出` | `Sign out` |
| `McpActionEnable` | `启用` | `Enable` |
| `McpActionDisable` | `停用` | `Disable` |
| `McpLegendList` | `↑/↓ 移动 · Enter 详情 · Esc 关闭` | `↑/↓ move · Enter details · Esc close` |
| `McpLegendDetail` | `↑/↓ 移动 · Enter 执行 · Esc 返回` | `↑/↓ move · Enter run · Esc back` |
| `McpLegendBusy` | `Esc 取消` | `Esc cancel` |

- [ ] **Step 3: 编译即验收**

```bash
cargo nextest run -p atomcode-i18n
```

Expected: 全绿。**若有哪一臂漏了，这个 crate 编译不过**——不用另写测试来钉这件事。

- [ ] **Step 4: 提交**

```bash
git add crates/atomcode-i18n/src/screen/messages.rs crates/atomcode-i18n/src/screen/en.rs crates/atomcode-i18n/src/screen/zh_cn.rs
git commit -F - <<'EOF'
feat(i18n): /mcp 面板要的 24 条文案

面板标题、四个分组/标签、三种认证说法、六个动作、三行按键图例。

放在最前面是因为模型层引用的 Msg 得先存在,否则后面一步都编译不过。
screen 表的 match 是穷尽的,缺一条翻译就编译不过——这一步不需要另写测试。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 2: 模型层——两级、纯按键

面板与 `/toolbox` 最大的不同：它是**两级**的（列表 ↔ 详情）。所以多一个 `Level`，而 `key()` 要按级分派。

**Files:**
- Create: `crates/atomcode-tui/src/mcp.rs`
- Modify: `crates/atomcode-tui/src/lib.rs`（`pub mod mcp;`）
- Test: 同文件的 `mod tests`

- [ ] **Step 1: 写失败的测试**

```rust
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
        let connected = detail(McpState::Connected, Auth::OAuth { authenticated: true });
        assert_eq!(
            panel.actions(&connected),
            vec![Action::Logout, Action::Disable]
        );

        // Waiting for a person: authenticate it, or switch it off.
        let needs_auth = detail(McpState::NeedsAuthentication, Auth::OAuth { authenticated: false });
        assert_eq!(
            panel.actions(&needs_auth),
            vec![Action::Login, Action::Disable]
        );

        // Already off: the only way is back on.
        let off = detail(McpState::Disabled, Auth::None);
        assert_eq!(panel.actions(&off), vec![Action::Enable]);
    }
```

- [ ] **Step 2: 跑它，确认失败**

```bash
cd /Users/lichao/project/gitcode/ai/atomcode/.worktrees/mcp-panel
cargo nextest run -p atomcode-tui enter_drills_into_the_selected_server_and_esc_comes_back
```

Expected: 编译失败，`cannot find type 'Panel' in this scope`（`lib.rs` 里还没有 `pub mod mcp;`）。

- [ ] **Step 3: 建文件并实现**

先 `crates/atomcode-tui/src/lib.rs` 里照 `pub mod tools;` 的位置加 `pub mod mcp;`。

`crates/atomcode-tui/src/mcp.rs`：

```rust
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
            let Some(row) = view.listed(panel).get(panel.cursor) else {
                return Step::Stay;
            };
            let server = row.name.clone();
            panel.level = Level::Detail;
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
fn detail_key(view: &McpView, panel: &mut Panel, press: KeyPress) -> Step {
    let Some(detail) = view.detail() else {
        // 详情还没到(刚按下 Enter,往返还没回来)。这时 Esc 仍要能退回去,
        // 否则一次慢往返会把面板卡在空白的详情页上。
        return match press.key {
            Key::Esc => {
                panel.level = Level::List;
                Step::Stay
            }
            _ => Step::Stay,
        };
    };
    let actions = panel.actions(detail);
    match (press.key, press.mods) {
        (Key::Esc, _) => {
            panel.note = None;
            panel.level = Level::List;
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
```

`level` 在 `Enter` 时**先切到详情再发请求**（`Step::OpenDetail`）：屏上立刻换页，数据到了再填。这也是 `detail_key` 里那个「详情还没到」分支存在的理由。

- [ ] **Step 4: 跑测试，确认通过**

```bash
cargo nextest run -p atomcode-tui
```

Expected: 全绿。此时 `modules/mcp.rs` 还没建——这一步只验模型层。

- [ ] **Step 5: 提交**

```bash
git add crates/atomcode-tui/src/mcp.rs crates/atomcode-tui/src/lib.rs
git commit -F - <<'EOF'
feat(tui): /mcp 面板的模型层——两级,按键是纯的

列表 ↔ 详情两级;Esc 在两级上意思不同(详情→列表,列表→关掉),所以 key()
按级分派。动作随状态现算,不是固定六项:给一个不可用的动作,人按下去只会
得到一次失败,不如不给。

模型层自带 McpState/McpRow/McpDetail,不引 host-api 的类型——照 crate::tools
的先例(docs/adr/0022 §3:屏幕不伸手进 agent 的 App),映射留给端口那一层。

记号按含义要、不按字符要:三个"要人做点什么"的状态共用 Pending,
记号只分「好/坏/等着/关着」四类,具体是什么由状态词说。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 3: 绘制层——`modules/mcp.rs`

与 `modules/tools.rs` 同一副骨架（`View` impl + `Row`/`layout`/`window`/`draw`/`geometry`），差别只在：详情层要画一张「标签 / 值」表加一个编号动作表。

**Files:**
- Create: `crates/atomcode-tui/src/modules/mcp.rs`
- Modify: `crates/atomcode-tui/src/modules/mod.rs`（`pub mod mcp;`）
- Test: 同文件 `mod tests`

- [ ] **Step 1: 写失败的测试**

```rust
    #[test]
    fn the_list_groups_by_source_and_the_detail_numbers_its_actions() {
        let view = McpView::new(vec![
            row("context7", "global", McpState::Connected, 8),
            row("figma", "project", McpState::NeedsAuthentication, 0),
        ]);

        let lines = render_list(&view, &Panel::new(), 60);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(text.iter().any(|l| l.contains("全局")), "来源分组: {text:?}");
        assert!(text.iter().any(|l| l.contains("项目")), "来源分组: {text:?}");

        let detail = detail_of(
            "figma",
            McpState::NeedsAuthentication,
            Auth::OAuth { authenticated: false },
        );
        let mut panel = Panel::new();
        panel.level = Level::Detail;
        let lines = render_detail(&view.with_detail(detail), &panel, 60);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(text.iter().any(|l| l.contains("1.")), "动作带编号: {text:?}");
        assert!(text.iter().any(|l| l.contains("认证")), "动作名: {text:?}");
    }
```

> `render_list` / `render_detail` / `line_text` / `row` / `detail_of` 是测试侧的小助手，写在同一 `mod tests` 里：前两个把被测的 `layout` + `draw` 跑成一个 `Vec<Line>`，后三个造数据。

- [ ] **Step 2: 跑它，确认失败**

```bash
cargo nextest run -p atomcode-tui the_list_groups_by_source_and_the_detail_numbers_its_actions
```

Expected: 编译失败（`modules/mcp.rs` 还不存在）。

- [ ] **Step 3: 实现**

照 `crates/atomcode-tui/src/modules/tools.rs` 的结构写：`pub const ID: &str = "mcp";`、`pub struct State_;`、`pub struct Mcp;`、`impl View for Mcp`（`render` 按 `Moment` 上是列表还是详情决定画哪一层）、`enum Row`（`Header(&str)` / `Server(&McpRow)` / `Label(&str, String)` / `Action(usize, Action)` / `Note(&str)`）、`layout`、`window`、`draw`、`geometry`。

三个必须照抄的细节：

1. **`render` 必须纯**：`View::render` 的文档写明"同一 `(state, viewport)` 在任何机器、任何时刻都要给出同样的行"。这里不需要时间，但**不要**在绘制里读 `Instant::now()` 之类的东西——那会让整个测试回路变绿而不可信（`docs/adr/0008`）。
2. **宽度经 `crate::width`**：中日韩宽字符按两格算，`chars().count()` 会错位（`modules/tools.rs` 用的就是 `crate::width`）。
3. **来源那三个串要译**：`McpRow.source` 是 `"global"`/`"project"`/`"driver"`（host 的说法），画标题时映射成 `Msg::McpGroupGlobal`/`McpGroupProject`/`McpGroupDriver`；**认不出的串原样画**，别吞（协议是 `non_exhaustive` 的那套思路：不知道的照实说）。

- [ ] **Step 4: 跑测试，确认通过**

```bash
cargo nextest run -p atomcode-tui
```

Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
git add crates/atomcode-tui/src/modules/mcp.rs crates/atomcode-tui/src/modules/mod.rs
git commit -F - <<'EOF'
feat(tui): /mcp 面板的绘制层

照 modules/tools.rs 的骨架:View impl + Row/layout/window/draw/geometry。
列表层按来源分组,详情层画"标签 / 值"表 + 编号动作表。

宽度一律走 crate::width:中日韩宽字符按两格算,直接数 chars 会错位。
来源那三个串译成人的话,认不出的原样画——不知道的照实说,别吞。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 4: 接线——`Moment` 两个字段 + `host.rs` 一组方法

`host.rs:2916` 的 `tools_key` 就是模板：它把面板的 `Step` 翻译成"面板变了没有 + 要不要往另一端发点活"。

**Files:**
- Modify: `crates/atomcode-tui/src/moment.rs`
- Modify: `crates/atomcode-tui/src/host.rs`
- Test: `host.rs` 的 `mod tests`

- [ ] **Step 1: 写失败的测试**

```rust
    #[test]
    fn the_mcp_panel_is_mutually_exclusive_with_the_others() {
        let host = host_for_test();
        assert!(!host.mcp_open());

        host.toggle_settings();
        assert!(host.toggle_mcp(), "opening it is a change");
        assert!(host.mcp_open(), "it is up");
        // 面板是一件事的不同块：升起一块，别的就该落下去。
        assert!(!host.settings_open(), "the settings panel stepped aside");

        // 列表层一次 Esc 就是关掉。
        let closing = host.mcp_key(key_press(Key::Esc));
        assert!(closing.0);
        assert!(!host.mcp_open());
    }
```

> `host_for_test()` / `key_press(Key)` / `settings_open()` 按该文件既有测试的写法取；`host.rs` 的 `mod tests` 里已有装配好的 host，照它来。

- [ ] **Step 2: 跑它，确认失败**

```bash
cargo nextest run -p atomcode-tui the_mcp_panel_is_mutually_exclusive_with_the_others
```

Expected: 编译失败，`no method named 'mcp_open' found`。

- [ ] **Step 3: 实现**

`moment.rs`，照 `tools_panel`（约 :564）与 `tools` 加两个字段：

```rust
    pub mcp_panel: Option<crate::mcp::Panel>,
    pub mcp: crate::mcp::McpView,
```

`host.rs`，照 `tools_*` 那一组（约 :2836-2973）加：

```rust
    pub fn mcp_open(&self) -> bool {
        self.moment
            .read()
            .expect("moment poisoned")
            .mcp_panel
            .is_some()
    }

    /// Pull the mcp panel up, or put it away. True when it changed.
    pub fn toggle_mcp(&self) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        match m.mcp_panel.take() {
            Some(_) => true,
            None => {
                if !self.modules.has_view(crate::modules::mcp::ID) {
                    return false;
                }
                // 面板家族是互斥的：升起一块，别的落下去。
                m.settings_panel = None;
                m.plugins_panel = None;
                if m.providers_panel.take().is_some() {
                    self.providers_secret
                        .lock()
                        .expect("provider secret poisoned")
                        .clear();
                }
                m.rewind_panel = None;
                m.tools_panel = None;
                m.mcp_panel = Some(crate::mcp::Panel::new());
                true
            }
        }
    }

    pub fn close_mcp(&self) -> bool {
        self.moment
            .write()
            .expect("moment poisoned")
            .mcp_panel
            .take()
            .is_some()
    }

    /// Put what the port answered into the moment. True when it changed.
    pub fn show_mcp(&self, view: crate::mcp::McpView) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        if m.mcp == view {
            return false;
        }
        m.mcp = view;
        true
    }

    /// Say that an action is on its way there and back, or that it landed.
    pub fn mcp_busy(&self, busy: Option<crate::mcp::Busy>) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let Some(panel) = m.mcp_panel.as_mut() else {
            return false;
        };
        if panel.busy == busy {
            return false;
        }
        panel.busy = busy;
        true
    }

    /// Say what the last key came to, when it came to something worth reading —
    /// the comment-guard's own words land here (设计 §6).
    pub fn mcp_note(&self, note: Option<String>) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let Some(panel) = m.mcp_panel.as_mut() else {
            return false;
        };
        if panel.note == note {
            return false;
        }
        panel.note = note;
        true
    }

    /// Run one key against the mcp panel: the panel it writes back, and the work
    /// to send over the seam when the key asked for some.
    pub fn mcp_key(&self, press: crate::surface::KeyPress) -> (bool, Option<crate::mcp::Step>) {
        let mut m = self.moment.write().expect("moment poisoned");
        let view = m.mcp.clone();
        let Some(panel) = m.mcp_panel.as_mut() else {
            return (false, None);
        };
        let before = panel.clone();
        let step = crate::mcp::key(&view, panel, press);
        let changed = *panel != before;
        match step {
            crate::mcp::Step::Stay => (changed, None),
            crate::mcp::Step::Close => {
                m.mcp_panel = None;
                (true, None)
            }
            step => (true, Some(step)),
        }
    }

    /// Put a paste into the mcp panel's search box.
    pub fn mcp_paste(&self, text: &str) -> bool {
        let mut m = self.moment.write().expect("moment poisoned");
        let Some(panel) = m.mcp_panel.as_mut() else {
            return false;
        };
        crate::mcp::paste(panel, text)
    }
```

**`mcp_key` 的返回值就是那条缝**：`Step::Stay`/`Close` 由面板自己收掉，`OpenDetail`/`Act` 原样交出去（`step => (true, Some(step))`）——与 `tools_key` 对 `Switch` 的处理同一个形状。屏幕不认识 `HostCommand`，所以拿到 `Step` 的那一层（输入循环）才知道要发什么。

- [ ] **Step 4: 跑测试，确认通过**

```bash
cargo nextest run -p atomcode-tui
```

Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
git add crates/atomcode-tui/src/moment.rs crates/atomcode-tui/src/host.rs
git commit -F - <<'EOF'
feat(tui): mcp 面板接上 Moment 与按键

照 tools_* 那一组:toggle/close/show/busy/note + mcp_key。面板家族互斥,
toggle_mcp 会把别的先收掉(与 toggle_tools 一致)。

mcp_key 的返回值就是那条缝:Stay/Close 面板自己收,OpenDetail/Act 原样交
出去。屏幕不认识 HostCommand,知道要发什么的是输入循环那一层。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 5: CLI 侧端口 `tui_mcp.rs`

模板是 `crates/atomcode-cli/src/tui_tools.rs`（整份读它再动手：插件行 + 一趟往返 + 契约到屏幕类型的翻译 + `said` 的错误映射）。

**Files:**
- Create: `crates/atomcode-cli/src/tui_mcp.rs`
- Modify: `crates/atomcode-tui/src/plugin.rs`（加 `McpSvc` seam，照 `ToolCatalogSvc`）
- Modify: `crates/atomcode-cli/src/main.rs`（挂这一行，照 `tui_tools` 的挂法）
- Test: `crates/atomcode-cli/tests/` 里已有的面板端口测试（若有）或新建一条

- [ ] **Step 1: 写失败的测试**

判据：`McpPort::list()` 在有 MCP 服务器时报出那些服务器，`detail()` 报出一个服务器的传输与认证，`act(Disable)` 之后 `list()` 里那台变成停用。用 `crates/atomcode-cli/tests/host.rs` 里已经建好的 `connected_mcp`（读路径那条判据的脚手架）装配，别重造。

- [ ] **Step 2: 跑它，确认失败**

```bash
cargo nextest run -p atomcode mcp_port_lists_details_and_acts
```

Expected: 编译失败（`tui_mcp.rs` 还不存在）。

- [ ] **Step 3: 实现**

`crates/atomcode-tui/src/plugin.rs` 加**一条宏**，不是结构体——**更正（初稿写错了）**：初稿写的是 `pub struct McpSvc(pub Arc<dyn crate::mcp::Mcp>);`，那样满足不了它自己的两处调用点（`provide::<McpSvc>(Arc::new(McpPort{..}))` 与 Task 6 的 `ctx.service::<McpSvc>()`）。这个文件里所有 seam 都是宏形状（`plugin.rs:98` 起）：

```rust
plexus_service!(McpSvc => dyn crate::mcp::Mcp, "tui-mcp", Seam, "The MCP servers a person can look at, drill into and change");
```

名字 `"tui-mcp"` 与 `tui_mcp.rs` 里那一行的 `provides()` 必须一致，否则端口挂上了也没人认得出。

**这个文件归计划 3 的 Task 4**（它在本层范围内），不在 Task 5 的写者手里。

`crates/atomcode-cli/src/tui_mcp.rs`：插件行 `McpRow`（`name()` = `"tui-panel-mcp"`，`inject()` = `["tui-modules"]`，`provides()` = `["tui-mcp"]`，`apply()` 里 `mods.add_view(Mounted::<atomcode_tui::modules::mcp::Mcp>::new())` 并 `provide::<McpSvc>(Arc::new(McpPort { ctx: ctx.clone() }))`），以及：

```rust
struct McpPort {
    ctx: Context,
}

impl McpPort {
    /// 不持 `HostControl` 而是每次问 `ctx` 要：连接会换（换会话、重连），
    /// 持着的那个会指向已经没人听的一端（`tui_tools.rs:60-66` 同一个理由）。
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
        match control.call(HostCommand::McpManage { session }).await {
            Ok(HostReply::McpRows { rows }) => Ok(view(rows)),
            Ok(other) => Err(unexpected(&other)),
            Err(error) => Err(said(error)),
        }
    }

    async fn detail(&self, server: &str) -> Result<McpDetail, String> {
        let (control, session) = self.link()?;
        match control
            .call(HostCommand::McpDetail {
                session,
                server: server.to_string(),
            })
            .await
        {
            Ok(HostReply::McpDetail { detail }) => Ok(detail_of(detail)),
            Ok(other) => Err(unexpected(&other)),
            Err(error) => Err(said(error)),
        }
    }

    async fn act(&self, server: &str, action: Action) -> Result<McpView, String> {
        let (control, session) = self.link()?;
        // 动手之前先让屏上说话：认证要走浏览器，可能几分钟。
        match control
            .call(HostCommand::McpAct {
                session,
                server: server.to_string(),
                action: wire_action(action),
            })
            .await
        {
            // 回答是刷新后的**列表**，不是这一台的详情——host 侧就是这么定的
            // （信任是整项目级的，列表比单台更有用）。
            Ok(HostReply::McpRows { rows }) => Ok(view(rows)),
            Ok(other) => Err(unexpected(&other)),
            Err(error) => Err(said(error)),
        }
    }
}
```

三个自由函数：`wire_action(Action) -> atomcode_host_api::McpAction`（一一对应）、`view(Vec<McpRow>) -> McpView`、`detail_of(McpServerDetail) -> McpDetail`（**传输只取命令与地址**，`McpTransport` 本来就没带 headers，照搬即可）。

`unexpected` 与错误映射**不要另写一份**：`crates/atomcode-cli/src/tui_tools.rs:114` 的 `said` 已经是 `pub(crate)`，直接用；`HostSaidSomethingElse` 那个串也照它的写法。

- [ ] **Step 4: 跑测试，确认通过**

```bash
cargo nextest run -p atomcode
```

Expected: 全绿（基线 404，读路径之后 406，本任务再加）。

- [ ] **Step 5: 提交**

```bash
git add crates/atomcode-cli/src/tui_mcp.rs crates/atomcode-cli/src/main.rs crates/atomcode-tui/src/plugin.rs
git commit -F - <<'EOF'
feat(tui): mcp 面板的 CLI 侧端口

照 tui_tools.rs:插件行 + 每次问 ctx 要连接(连接会换,持着的会指向没人听的一端)
+ 契约类型到屏幕类型的翻译。错误映射直接用 tui_tools 那份 said,不另写。

list/detail/act 分别走 McpManage/McpDetail/McpAct。act 的回答是刷新后的列表
而不是单台详情——host 侧就是这么定的。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 6: `/mcp` 无参打开面板

**这一步会改掉既有行为**：今天 `/mcp` 无参打印一列文本，改完之后它升起面板。设计 §5.1 与截图都是这个意思（面板取代文本列表）。

**Files:**
- Modify: `crates/atomcode-tui/src/commands.rs`（`/mcp` 的无参分支）
- Modify: `crates/atomcode-tui/src/commands.rs` 的测试（既有那条断言文本输出的用例）
- Modify: 输入循环里分发 `tools_key` 的那一处（照它加 `mcp_key` 与端口调用）

- [ ] **Step 1: 改既有测试**

`mcp_names_the_server_waiting_for_auth_and_the_one_switched_off`（读路径时新加的那条）断言的是**文本**输出。它现在要改成断言**面板升起来了**：同样的 `McpServers` 回答之后，`host.mcp_open()` 为真，且 `Moment` 里的 `mcp` 视图里那两个状态正确。那两个状态的**文案**断言搬到 Task 3 的绘制测试里去（那里才是它们现在被画出来的地方）。

- [ ] **Step 2: 跑它，确认失败**

```bash
cargo nextest run -p atomcode-tui mcp_names_the_server_waiting_for_auth_and_the_one_switched_off
```

Expected: 失败（无参 `/mcp` 还在打印文本，面板没升起）。

- [ ] **Step 3: 实现**

`commands.rs` 的 `/mcp` 分支里，把 `""`（无参）那一支从"拼字符串回报"改成"升起面板并取一次目录"：

```rust
                    "" => {
                        if !host.toggle_mcp() {
                            return Outcome::Refused(t(Msg::McpPanelUnavailable).into_owned());
                        }
                        // 目录由端口那一趟带进来；屏上先画空表，别等。
                        match ask_mcp_list(host).await {
                            Ok(view) => {
                                host.show_mcp(view);
                                Outcome::Said(t(Msg::McpPanelTitle).into_owned())
                            }
                            Err(message) => {
                                host.mcp_note(Some(message.clone()));
                                Outcome::Refused(message)
                            }
                        }
                    }
```

`ask_mcp_list` 是从 `McpSvc` 取端口、调 `list()` 的小助手（照同一文件里 `/toolbox` 取目录那一段写）。

**`Msg::McpPanelUnavailable`** 是这一处新要的一条文案（「这个构建没挂 MCP 面板」）：在 `messages.rs` 加变体、`en.rs` 与 `zh_cn.rs` 各加一臂，**就在这一步的提交里加**——不回头看 Task 1，那一步已经提交过了。穷尽 match 会提醒你别只加一半。

有参的那几支（`login`/`logout`/`trust`/`untrust`/`tools`/`reload`）**一个都不动**：面板是给无参调用的人的，命令行开关照旧。

- [ ] **Step 4: 跑测试，确认通过**

```bash
cargo nextest run -p atomcode-tui
```

Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
git add crates/atomcode-tui/src/commands.rs crates/atomcode-i18n/src/screen/messages.rs crates/atomcode-i18n/src/screen/en.rs crates/atomcode-i18n/src/screen/zh_cn.rs
git commit -F - <<'EOF'
feat(tui): /mcp 无参升起面板,不再是打印一列文本

设计与截图都是这个意思:面板取代文本列表。有参的那几支(login/logout/
trust/untrust/tools/reload)一个都不动——面板是给无参调用的人的。

既有那条断言文本输出的判据改成断言面板升起;那两个状态的文案断言搬到
绘制层的测试里,那儿才是它们现在被画出来的地方。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Task 7: 交付门

- [ ] **Step 1: 三个 crate 各跑一遍**

```bash
cargo nextest run -p atomcode-tui
cargo nextest run -p atomcode-i18n
cargo nextest run -p atomcode
```

Expected: 全绿（`atomcode-coding` 的 2 个既有失败与本层无关，不必跑它）。

- [ ] **Step 2: 格式门**

```bash
cargo fmt --all
cargo fmt --all -- --check
```

Expected: 第二条退出 0。**若同一棵工作树里还有别人未提交的改动**，`--all` 会连带格式化它们的半成品——先确认工作树里只有你的文件，否则用 `cargo fmt -p <crate>`。

- [ ] **Step 3: 手动过一遍（这一层没有自动化能替代）**

```bash
cargo run -p atomcode -- --tui
```

然后：`/mcp` → 面板升起、按来源分组 → `Enter` 钻进去 → 动作随状态出现 → `Esc` 回列表 → 再 `Esc` 关掉。终端太窄时确认标题与图例没被截成半句。

**这一条是手动的，不要拿测试通过冒充它**：面板的观感（分组是否清楚、光标是否可见、窄屏是否错位）只有人眼能判。

- [ ] **Step 4: 确认没动命令层的代码**

```bash
git diff <计划 2b 的最后一个提交> -- crates/atomcode-host-api/src crates/atomcode-coding/src crates/atomcode-cli/src/host.rs
```

人工核对：**这一层一行都没碰命令层**，只加了端口实现与装配。

---

## 覆盖对照（对着设计文档 §7 与 §5.1-§5.4）

| 来源 | 判据 | 落在哪 |
| --- | --- | --- |
| 计划新增 | `enter_drills_into_the_selected_server_and_esc_comes_back` | Task 2 Step 1（两级导航，`Esc` 在两级上意思不同） |
| 计划新增 | `the_detail_actions_are_the_ones_this_server_can_take` | Task 2 Step 1（动作随状态现算，设计 §5.2） |
| 计划新增 | `the_list_groups_by_source_and_the_detail_numbers_its_actions` | Task 3 Step 1（列表分组 + 详情编号动作） |
| 计划新增 | `the_mcp_panel_is_mutually_exclusive_with_the_others` | Task 4 Step 1（面板家族互斥） |
| 计划新增 | `mcp_port_lists_details_and_acts` | Task 5 Step 1（端口三层都能通） |
| 既有判据改写 | `mcp_names_the_server_waiting_for_auth_and_the_one_switched_off` | Task 6 Step 1（从"断言文本"改成"断言面板升起"） |
| §5.4 | 停用后回列表 / 认证期间可 `Esc` | Task 2 的 `Step` 与 `busy` 分支；观感由 Task 7 Step 3 手动过 |

## 本计划明确不做

- **不做会话内启停**（`/toolbox` 的 `SwitchTool`，语义不同——设计 §3 钉过）
- **不在面板里逐个开关工具**——那还是 `/toolbox` 的活，面板只报工具数
- **不动命令层的代码**（host-api / coding / cli 的 host.rs），只加端口实现与装配
- **不做鼠标**：`tools_wheel`/`tools_row_at` 那一套不复制过来（面板够小，键盘够用；要做是单独一件事）
- **不做认证进度推送**（设计 §8 已明确）：认证那一趟阻塞到结束，期间只显示 `busy`
```
