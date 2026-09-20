//! 工具面板:模型此刻能调哪些工具,哪些被人关掉了,哪些这棵树压根没有。
//!
//! 和 `/config`、`/provider`、`/plugin` 是同一件事的第四块——人要在里面待一会儿、
//! 反复开关——所以同一副骨架:从底下升起来占住输入框的位置。
//!
//! 住在这儿的是**数据和按键**。工具是什么、开关怎么落下去,由
//! [`Tools`] 端口出去(`docs/adr/0022` §3):这个模块不知道有 MCP 这回事,也不知道
//! 一次开关要过几层。判据与语义见 `docs/tool-catalog-policy.md`。
//!
//! 一件要紧的事:**配置排除掉的工具不给开关**。它压根没进过目录,命令放不回来,
//! 屏幕上给一个按下去什么都不会发生的开关,比不给还糟。

use crate::surface::{Key, KeyPress, Mods};

/// 一个工具此刻是什么状态。与 `atomcode_host_api::ToolState` 同形,分开一份是因为
/// 这一层不认那个 crate(`docs/adr/0022` §3)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// 模型能调。
    On,
    /// 人在这次会话里关掉了,能放回来。
    Off,
    /// 这棵树的配置就没有它。只有改配置才动得了。
    Excluded,
}

impl State {
    /// 行首那个记号,**按含义**要,不按字符要:字面的 `●` 在 ASCII 终端上是个
    /// 豆腐块,而屏蔽层知道该换成什么(`crate::caps`)。
    pub fn glyph(self) -> crate::caps::Glyph {
        match self {
            Self::On => crate::caps::Glyph::Bullet,
            Self::Off => crate::caps::Glyph::Hollow,
            Self::Excluded => crate::caps::Glyph::Fail,
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            Self::On => "模型能调",
            Self::Off => "本次会话关掉的",
            Self::Excluded => "配置排除的",
        }
    }
}

/// 一个工具,照面板需要的样子。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolRow {
    pub name: String,
    /// 哪一行给的。空着表示注册它的人没说。
    pub owner: String,
    pub state: State,
}

/// 目录此刻的样子。面板开的时候问一次,每次开关之后再问一次——不按帧问,
/// 因为那是一次跨进程往返,而 `render` 必须是纯的。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolsView {
    tools: Vec<ToolRow>,
}

impl ToolsView {
    pub fn new(mut tools: Vec<ToolRow>) -> Self {
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Self { tools }
    }

    pub fn tools(&self) -> &[ToolRow] {
        &self.tools
    }

    pub fn on_count(&self) -> usize {
        self.tools.iter().filter(|t| t.state == State::On).count()
    }

    pub fn off_count(&self) -> usize {
        self.tools.iter().filter(|t| t.state == State::Off).count()
    }

    /// 过滤之后列出来的那些,按名字和给它的那一行都能搜到。
    pub fn listed(&self, panel: &Panel) -> Vec<&ToolRow> {
        let q = panel.query.trim().to_lowercase();
        self.tools
            .iter()
            .filter(|t| {
                q.is_empty()
                    || t.name.to_lowercase().contains(&q)
                    || t.owner.to_lowercase().contains(&q)
            })
            .collect()
    }
}

/// 有活在外面跑着:一次开关是一趟往返,回来之前屏上要有话说。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Busy {
    pub what: String,
}

/// 工具面板,开着的时候。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Panel {
    /// 光标停在 *listed* 的第几行。
    pub cursor: usize,
    pub query: String,
    /// 上一次操作之后要说的一句话,比如「配置排除的,改配置才能放回来」。
    pub note: Option<String>,
    pub busy: Option<Busy>,
}

impl Panel {
    pub fn new() -> Self {
        Self::default()
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
    /// 把 `pattern` 关掉或者放回来,然后拿回新的目录。
    Switch { pattern: String, on: bool },
}

/// 一次按键。纯函数:面板改自己,要外面做的事从返回值出去。
pub fn key(view: &ToolsView, panel: &mut Panel, press: KeyPress) -> Step {
    // 有活在跑:只认 Esc。别的键按下去会派出第二次开关,而第一次还没回来。
    if panel.busy.is_some() {
        return match (press.key, press.mods) {
            (Key::Esc, _) => {
                panel.busy = None;
                Step::Close
            }
            _ => Step::Stay,
        };
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
        (Key::Enter, _) | (Key::Char(' '), Mods::NONE) => {
            panel.note = None;
            let Some(row) = view.listed(panel).get(panel.cursor).cloned().cloned() else {
                return Step::Stay;
            };
            match row.state {
                // 没有开关可给:配置里没有它,命令也放不回来。说出来,而不是给一个
                // 按下去什么都不发生的键。
                State::Excluded => {
                    panel.note = Some(format!(
                        "`{}` 是这棵树的配置排除掉的 —— 改配置才能放回来",
                        row.name
                    ));
                    Step::Stay
                }
                State::On => {
                    panel.busy = Some(Busy {
                        what: format!("正在关掉 {}…", row.name),
                    });
                    Step::Switch {
                        pattern: row.name,
                        on: false,
                    }
                }
                State::Off => {
                    panel.busy = Some(Busy {
                        what: format!("正在放回 {}…", row.name),
                    });
                    Step::Switch {
                        pattern: row.name,
                        on: true,
                    }
                }
            }
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

/// 粘贴进过滤框:一个工具名常常是复制来的。
pub fn paste(panel: &mut Panel, text: &str) -> bool {
    if panel.busy.is_some() {
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

/// 目录从哪儿来,开关往哪儿去。
///
/// 实现住在装配它的那一侧(cli),因为「此刻能调什么」是运行中那棵树的事,而屏幕
/// 不许伸手进 agent 的 App(`docs/adr/0022` §3)。
#[async_trait::async_trait]
pub trait Tools: Send + Sync {
    /// 目录此刻的样子。
    async fn list(&self) -> Result<ToolsView, String>;

    /// 关掉或放回,答的是**之后**的目录——一趟往返,屏上画的是发生过的事,
    /// 不是自己以为发生了的事。
    async fn switch(&self, pattern: &str, on: bool) -> Result<ToolsView, String>;
}
