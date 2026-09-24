//! 后台面板:放到后台的会话,画出来。
//!
//! 与 `/resume` 从同一个地方升起来。顶上一行暗色的说明(怎么打开、怎么回去、怎么
//! 退出),下面三组——`Needs input`(黄 `✱`)、`Working`、`Completed`(绿 `·`)——每行
//! 两列:会话名按组内最宽的对齐,右边一行摘要,超宽截断;选中那一行整行底色、名字
//! 加粗。底下一个输入框(写任务新开一个,或就地回复选中的那个),最底一行按键提示。
//!
//! 住在这儿的是**画**:会话**是什么**在 [`crate::bg::BgView`],按键在
//! [`crate::bg::key`]——这个模块不认宿主控制契约。

use crate::bg::{BgView, Group, Panel};
use crate::frame::{Line, Span, Style};
use crate::i18n::{t, Msg};
use crate::module::{Height, View};
use crate::modules::chrome::{box_edge, caret_spans, pad_to, panel_edge};
use crate::moment::{Moment, Viewport};
use crate::theme::{self, Role};
use crate::width;

pub const ID: &str = "bg";
/// 列表最多画这么多行(组标题也算),和别的面板占同一块地方。
const MOST: usize = 16;

#[derive(Default)]
pub struct State;

pub struct Bg;

impl View for Bg {
    type State = State;

    fn id() -> &'static str {
        ID
    }

    fn absorb(_state: &mut State, _fact: &atomcode_harness::session::SessionEvent) {}

    fn render(_state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        let Some(panel) = vp.moment.bg_panel.as_ref() else {
            return Vec::new();
        };
        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let view = &vp.moment.bg;
        layout(view, panel)
            .into_iter()
            .map(|row| draw(view, panel, row, w, vp.moment.caps))
            .collect()
    }

    fn height(_state: &State, moment: &Moment, width: u16) -> Height {
        let Some(panel) = moment.bg_panel.as_ref() else {
            return Height::Hug(0);
        };
        if width == 0 {
            return Height::Hug(0);
        }
        Height::Hug(layout(&moment.bg, panel).len().min(u16::MAX as usize) as u16)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Rule,
    Top,
    Blank,
    Heading(Group),
    /// 画的顺序里的第几个会话。
    Session(usize),
    Nothing,
    /// 上下各有多少条不在视野里。
    Scroll {
        above: usize,
        below: usize,
    },
    BoxTop,
    Input,
    BoxBottom,
    Legend,
}

fn layout(view: &BgView, panel: &Panel) -> Vec<Row> {
    let mut rows = vec![Row::Rule, Row::Top, Row::Blank];
    if view.is_empty() {
        rows.push(Row::Nothing);
    } else {
        let mut listed = Vec::new();
        for group in Group::ALL {
            let members: Vec<usize> = view
                .sessions()
                .iter()
                .enumerate()
                .filter(|(_, s)| s.group == group)
                .map(|(at, _)| at)
                .collect();
            if members.is_empty() {
                continue;
            }
            listed.push(Row::Heading(group));
            listed.extend(members.into_iter().map(Row::Session));
        }
        if listed.len() > MOST {
            // 让光标那一行留在视野里。
            let here = listed
                .iter()
                .position(|row| *row == Row::Session(panel.cursor))
                .unwrap_or(0);
            let room = MOST - 1;
            let from = here.saturating_sub(room / 2).min(listed.len() - room);
            let to = from + room;
            rows.push(Row::Scroll {
                above: from,
                below: listed.len() - to,
            });
            rows.extend_from_slice(&listed[from..to]);
        } else {
            rows.extend(listed);
        }
    }
    rows.extend([
        Row::Blank,
        Row::BoxTop,
        Row::Input,
        Row::BoxBottom,
        Row::Legend,
    ]);
    rows
}

fn heading(group: Group) -> std::borrow::Cow<'static, str> {
    match group {
        Group::NeedsInput => t(Msg::BgGroupNeedsInput),
        Group::Working => t(Msg::BgGroupWorking),
        Group::Completed => t(Msg::BgGroupCompleted),
    }
}

fn draw(view: &BgView, panel: &Panel, row: Row, w: usize, caps: crate::caps::Caps) -> Line {
    match row {
        Row::Rule => panel_edge(w, caps),
        Row::Top => Line::styled(
            format!(
                "  {}",
                if panel.moved.is_some() {
                    t(Msg::BgPanelMoved)
                } else {
                    t(Msg::BgPanelLooking)
                }
            ),
            theme::fg(Role::Muted),
        )
        .truncate(w),
        Row::Blank => Line::empty(),
        Row::Heading(group) => Line::styled(
            format!("  {}", heading(group)),
            theme::fg(Role::Secondary).bold(),
        )
        .truncate(w),
        Row::Nothing => Line::styled(
            width::take_width(&format!("  {}", t(Msg::BgPanelEmpty)), w),
            theme::fg(Role::Muted),
        ),
        Row::Scroll { above, below } => Line::styled(
            width::take_width(&format!("  ↑{above} ↓{below}"), w),
            theme::fg(Role::Muted),
        ),
        Row::Session(at) => session_line(view, panel, at, w),
        Row::BoxTop => box_edge(w, caps, true),
        Row::BoxBottom => box_edge(w, caps, false),
        Row::Input => input_line(view, panel, w, caps),
        Row::Legend => Line::styled(
            format!(
                "  {}",
                if panel.keys {
                    t(Msg::BgKeys)
                } else {
                    t(Msg::BgLegend)
                }
            ),
            theme::fg(Role::Muted),
        )
        .truncate(w),
    }
}

/// 一行会话:记号、按组内最宽对齐的名字、一行摘要。
fn session_line(view: &BgView, panel: &Panel, at: usize, w: usize) -> Line {
    let Some(session) = view.at(at) else {
        return Line::empty();
    };
    let here = at == panel.cursor;
    let base = if here {
        theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg))
    } else {
        Style::new()
    };
    let (mark, mark_style) = match session.group {
        Group::NeedsInput => ("✱", theme::fg(Role::Warning)),
        Group::Working => ("◦", theme::fg(Role::Accent)),
        Group::Completed => ("·", theme::fg(Role::Success)),
    };
    // 名字这一列按**这一组**最宽的对齐,但不许吃掉超过一半的宽度——否则一个很长的
    // 名字会把摘要挤得一个字也不剩。
    let widest = view
        .sessions()
        .iter()
        .filter(|s| s.group == session.group)
        .map(|s| width::str_width(&s.title))
        .max()
        .unwrap_or(0)
        .min(w / 2);
    let title = width::take_width(&session.title, widest);
    let pad = widest.saturating_sub(width::str_width(&title));
    let title_style = if here {
        base.under(theme::fg(Role::PanelFg)).bold()
    } else {
        base.under(theme::fg(Role::PanelFg))
    };
    let replying = panel.replying.as_deref() == Some(session.id.as_str());
    let last = session
        .last
        .clone()
        .unwrap_or_else(|| t(Msg::BgNothingSaid).into_owned());
    let spans = vec![
        Span::styled("  ", base),
        Span::styled(mark, base.under(mark_style)),
        Span::styled(" ", base),
        Span::styled(format!("{title}{}", " ".repeat(pad)), title_style),
        Span::styled("   ", base),
        Span::styled(
            last,
            base.under(theme::fg(if replying {
                Role::Warning
            } else {
                Role::Muted
            })),
        ),
    ];
    pad_to(Line::from_spans(spans).truncate(w), w, base)
}

/// 输入框里那一行:写着的字(带光标),或者占位的那句。
fn input_line(view: &BgView, panel: &Panel, w: usize, caps: crate::caps::Caps) -> Line {
    use crate::caps::Glyph;
    let lead = Span::styled(
        format!("{} ", caps.g(Glyph::Vertical)),
        theme::fg(Role::Border),
    );
    let mut spans = vec![lead];
    if let Some(title) = panel
        .replying
        .as_deref()
        .and_then(|id| view.position(id))
        .and_then(|at| view.at(at))
        .map(|s| s.title.clone())
    {
        spans.push(Span::styled(
            format!("{} ", t(Msg::BgReplyTo { title: &title })),
            theme::fg(Role::Warning),
        ));
    }
    if panel.input.is_empty() && panel.replying.is_none() {
        spans.push(Span::styled(" ", Style::new().reverse()));
        spans.push(Span::styled(
            t(Msg::BgPlaceholder).into_owned(),
            theme::fg(Role::Muted),
        ));
    } else {
        spans.extend(caret_spans(
            &panel.input,
            panel.input.len(),
            theme::fg(Role::PanelFg),
            w.saturating_sub(2),
        ));
    }
    Line::from_spans(spans).truncate(w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bg::Session;

    fn session(id: &str, title: &str, group: Group, last: &str) -> Session {
        Session {
            id: id.into(),
            title: title.into(),
            group,
            last: Some(last.into()),
        }
    }

    fn text(view: &BgView, panel: &Panel, w: usize) -> Vec<String> {
        layout(view, panel)
            .into_iter()
            .map(|row| draw(view, panel, row, w, crate::caps::Caps::default()).plain())
            .collect()
    }

    /// 三组按固定顺序出现,空组不画标题;名字按组内最宽的对齐。
    #[test]
    fn groups_come_in_order_and_titles_line_up() {
        let view = BgView::new(vec![
            session("a", "短", Group::Completed, "改完了"),
            session("b", "Rewind 计算问题", Group::Completed, "三处都修了"),
            session("c", "跑测试", Group::NeedsInput, "可以改 src/lib.rs 吗?"),
        ]);
        let panel = Panel::new(None);
        let lines = text(&view, &panel, 80);
        let find = |needle: &str| lines.iter().position(|l| l.contains(needle));
        let needs = find(&t(Msg::BgGroupNeedsInput)).expect("needs-input heading");
        let done = find(&t(Msg::BgGroupCompleted)).expect("completed heading");
        assert!(needs < done, "{lines:#?}");
        assert!(
            find(&t(Msg::BgGroupWorking)).is_none(),
            "空组不画标题:{lines:#?}"
        );
        let column = |needle: &str| {
            let line = &lines[find(needle).unwrap()];
            width::str_width(line.split(needle).next().unwrap_or(""))
        };
        assert_eq!(column("改完了"), column("三处都修了"), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains('✱')), "{lines:#?}");
    }

    #[test]
    fn the_box_says_what_it_is_for_until_something_is_typed() {
        let view = BgView::new(Vec::new());
        let mut panel = Panel::new(None);
        let shown = text(&view, &panel, 80).join("\n");
        assert!(shown.contains(&*t(Msg::BgPlaceholder)), "{shown}");
        assert!(shown.contains(&*t(Msg::BgPanelEmpty)), "{shown}");
        panel.input = "跑一遍测试".into();
        let shown = text(&view, &panel, 80).join("\n");
        assert!(!shown.contains(&*t(Msg::BgPlaceholder)), "{shown}");
        assert!(shown.contains("跑一遍测试"), "{shown}");
    }
}
