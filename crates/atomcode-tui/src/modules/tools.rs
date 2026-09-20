//! 工具面板:模型此刻能调什么,列出来,开关它。
//!
//! 和 `/config`、`/provider`、`/plugin` 从同一个地方升起来,是同一件事的第四块。
//!
//! 住在这儿的是**画**。工具**是什么**是数据([`crate::tools::ToolsView`]),开关从
//! [`crate::tools::Tools`] 出去——这个模块不知道有 MCP 这回事,也不知道一次开关要
//! 过几层(`docs/adr/0022` §3)。
//!
//! 什么都不折叠:面板的状态在 [`crate::moment::Moment`] 里,那是「这块屏幕此刻在
//! 干什么」该待的地方。

use crate::frame::{Line, Style};
use crate::module::{Height, View};
use crate::modules::chrome::{self, box_edge, pad_to, panel_edge, search_line};
use crate::moment::{Moment, Viewport};
use crate::theme::{self, Role};
use crate::tools::{Panel, State, ToolsView};
use crate::width;

pub const ID: &str = "tools";

/// 面板的名字,表头里和命中测试里是同一个串。
const NAME: &str = "tools";

/// 不管有多少工具,列表最多占这么多行。和另外三块面板同一个数,因为占的是同一块地方。
const MOST: usize = 12;

#[derive(Default)]
pub struct State_;

pub struct Tools;

impl View for Tools {
    type State = State_;

    fn id() -> &'static str {
        ID
    }

    fn absorb(_state: &mut State_, _fact: &atomcode_harness::session::SessionEvent) {}

    fn render(_state: &State_, vp: &Viewport<'_>) -> Vec<Line> {
        let Some(panel) = vp.moment.tools_panel.as_ref() else {
            return Vec::new();
        };
        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let view = &vp.moment.tools;
        let caps = vp.moment.caps;
        layout(view, panel, vp.rect.h as usize)
            .into_iter()
            .map(|row| draw(view, panel, row, w, caps))
            .collect()
    }

    /// 按**没过滤**的清单数,理由同另外三块:面板的下沿是钉住的那条边,跟着过滤走
    /// 的高度会在人打字时把搜索框顺着屏幕往下拽。
    fn height(_state: &State_, moment: &Moment, width: u16) -> Height {
        let Some(panel) = moment.tools_panel.as_ref() else {
            return Height::Hug(0);
        };
        if width == 0 {
            return Height::Hug(0);
        }
        let mut open = panel.clone();
        open.query.clear();
        Height::Hug(
            layout(&moment.tools, &open, usize::MAX)
                .len()
                .min(u16::MAX as usize) as u16,
        )
    }
}

/// 面板的一行,画出来之前。整张布局只有这一份:[`draw`]、[`geometry`] 和 `height`
/// 走的是同一个。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Row {
    Rule,
    Header,
    BoxTop,
    Search,
    BoxBottom,
    Blank,
    Listed(usize),
    Nothing,
    Scroll { above: usize, below: usize },
    Note,
    Busy,
    Legend,
}

fn layout(view: &ToolsView, panel: &Panel, h: usize) -> Vec<Row> {
    let mut rows = vec![Row::Rule, Row::Header];
    if panel.busy.is_some() {
        rows.push(Row::Blank);
        rows.push(Row::Busy);
        rows.push(Row::Blank);
        rows.push(Row::Legend);
        return rows;
    }
    rows.push(Row::BoxTop);
    rows.push(Row::Search);
    rows.push(Row::BoxBottom);
    let listed = view.listed(panel);
    if listed.is_empty() {
        rows.push(Row::Nothing);
    } else {
        let reserved = rows.len() + 2 + usize::from(panel.note.is_some());
        let cap = h.saturating_sub(reserved).min(MOST);
        let (from, to) = if listed.len() > cap {
            let room = cap.saturating_sub(1).max(1);
            let (from, to) = window(listed.len(), panel.cursor, room);
            rows.push(Row::Scroll {
                above: from,
                below: listed.len() - to,
            });
            (from, to)
        } else {
            (0, listed.len())
        };
        for at in from..to {
            rows.push(Row::Listed(at));
        }
    }
    if panel.note.is_some() {
        rows.push(Row::Note);
    }
    rows.push(Row::Blank);
    rows.push(Row::Legend);
    rows
}

/// `len` 行里让 `cursor` 留在视野里的那一段。夹住而不是居中,同另外三块。
fn window(len: usize, cursor: usize, room: usize) -> (usize, usize) {
    if len <= room {
        return (0, len);
    }
    let half = room / 2;
    let from = cursor.saturating_sub(half).min(len - room);
    (from, from + room)
}

fn draw(view: &ToolsView, panel: &Panel, row: Row, w: usize, caps: crate::caps::Caps) -> Line {
    match row {
        Row::Rule => panel_edge(w, caps),
        Row::Header => {
            // 一块没有页签的面板,表头上说的是这份目录此刻的账:开着几个、关掉几个。
            let label = format!("{} 个能调 · {} 个关掉的", view.on_count(), view.off_count());
            let labels = [label.as_str()];
            Line::from_spans(chrome::header_parts(NAME, &labels, usize::MAX).0).truncate(w)
        }
        Row::BoxTop => box_edge(w, caps, true),
        Row::Search => search_line(&panel.query, Some(panel.query.len()), w, caps),
        Row::BoxBottom => box_edge(w, caps, false),
        Row::Blank => Line::empty(),
        Row::Nothing => Line::styled(
            width::take_width(
                if view.tools().is_empty() {
                    "  这棵树一个工具都没挂"
                } else {
                    "  没有匹配的工具"
                },
                w,
            ),
            theme::fg(Role::Muted),
        ),
        Row::Scroll { above, below } => Line::styled(
            width::take_width(&format!("  ↑{above} ↓{below}"), w),
            theme::fg(Role::Muted),
        ),
        Row::Listed(at) => listed_line(view, panel, at, w, caps),
        Row::Note => Line::styled(
            width::take_width(&format!("  {}", panel.note.clone().unwrap_or_default()), w),
            theme::fg(Role::Warning),
        ),
        Row::Busy => Line::styled(
            width::take_width(
                &format!(
                    "  {}",
                    panel
                        .busy
                        .as_ref()
                        .map(|b| b.what.clone())
                        .unwrap_or_default()
                ),
                w,
            ),
            theme::fg(Role::Warning),
        ),
        Row::Legend => Line::styled(
            format!("  {}", crate::widget::keys(&legend(panel), caps)),
            theme::fg(Role::Muted),
        )
        .truncate(w),
    }
}

fn listed_line(
    view: &ToolsView,
    panel: &Panel,
    at: usize,
    w: usize,
    caps: crate::caps::Caps,
) -> Line {
    let listed = view.listed(panel);
    let Some(tool) = listed.get(at).copied() else {
        return Line::empty();
    };
    let here = at == panel.cursor;
    let base = if here {
        theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg))
    } else {
        Style::new()
    };
    let pointer = if here {
        format!("{} ", caps.g(crate::caps::Glyph::Pointer))
    } else {
        "  ".to_string()
    };
    // 关掉的和排除的都画暗:它们此刻不是模型的工具,一眼扫过去该看得出来。
    let name_style = match tool.state {
        State::On => base,
        _ => theme::fg(Role::Muted).under(base),
    };
    let mut about = Vec::new();
    if !tool.owner.is_empty() {
        about.push(tool.owner.clone());
    }
    if tool.state != State::On {
        about.push(tool.state.about().to_string());
    }
    let head = format!("{pointer}{} {}", caps.g(tool.state.glyph()), tool.name);
    let mut line = Line::styled(width::take_width(&head, w), name_style);
    if !about.is_empty() {
        let room = w.saturating_sub(width::str_width(&head));
        if room > 2 {
            let tail = format!("  {}", about.join(" · "));
            line.push(crate::frame::Span::styled(
                width::take_width(&tail, room),
                theme::fg(Role::Muted).under(base),
            ));
        }
    }
    pad_to(line, w, base)
}

fn legend(panel: &Panel) -> Vec<(&'static str, &'static str)> {
    if panel.busy.is_some() {
        return vec![("esc", "不等了")];
    }
    vec![
        ("↑↓", "选择"),
        ("⏎", "开 / 关"),
        ("打字", "筛"),
        ("esc", "收起"),
    ]
}

/// 屏幕上的行 ↔ 列表里的第几个,给点击用。
pub struct Geometry {
    rows: Vec<Option<usize>>,
    header: Option<usize>,
}

impl Geometry {
    pub fn row_at(&self, row: usize) -> Option<usize> {
        self.rows.get(row).copied().flatten()
    }

    pub fn is_header(&self, row: usize) -> bool {
        self.header == Some(row)
    }
}

pub fn geometry(moment: &Moment, vp: &Viewport<'_>) -> Geometry {
    let Some(panel) = moment.tools_panel.as_ref() else {
        return Geometry {
            rows: Vec::new(),
            header: None,
        };
    };
    let rows = layout(&moment.tools, panel, vp.rect.h as usize);
    Geometry {
        header: rows.iter().position(|row| *row == Row::Header),
        rows: rows
            .into_iter()
            .map(|row| match row {
                Row::Listed(at) => Some(at),
                _ => None,
            })
            .collect(),
    }
}
