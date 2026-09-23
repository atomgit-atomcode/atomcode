//! 恢复面板:别的会话,画出来挑一个接着聊。
//!
//! 和 `/config`、`/provider`、`/plugin`、`/toolbox`、`/rewind` 从同一个地方升起来,是
//! 同一件事的又一块。骨架照 `/provider`:一条规则线、表头、一个边框搜索框(输入即
//! 筛)、下面是会话列表(亮色标题压一行灰色元数据),底下一行提示。
//!
//! 住在这儿的是**画**。会话**是什么**是数据([`crate::resume::ResumeView`],由
//! `/resume` 命令那一趟往返填进 [`crate::moment::Moment::resume`]),恢复哪个从
//! [`crate::resume::Step`] 出去——这个模块不认宿主控制契约。

use crate::frame::{Line, Style};
use crate::i18n::{t, Msg};
use crate::module::{Height, View};
use crate::modules::chrome::{self, box_edge, pad_to, panel_edge, search_line};
use crate::moment::{Moment, Viewport};
use crate::resume::{Panel, ResumeView};
use crate::theme::{self, Role};
use crate::width;

pub const ID: &str = "resume";
/// 表头里那个名字,和别的面板一样是这一块的英文标识。
const NAME: &str = "resume";
/// 列表最多画这么多条,和别的面板占同一块地方。
const MOST: usize = 12;

#[derive(Default)]
pub struct State;

pub struct Resume;

impl View for Resume {
    type State = State;

    fn id() -> &'static str {
        ID
    }

    fn absorb(_state: &mut State, _fact: &atomcode_harness::session::SessionEvent) {}

    fn render(_state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        let Some(panel) = vp.moment.resume_panel.as_ref() else {
            return Vec::new();
        };
        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let view = &vp.moment.resume;
        let caps = vp.moment.caps;
        let preview = preview_of(vp.moment, view, panel);
        layout(view, panel, preview, vp.rect.h as usize)
            .into_iter()
            .map(|row| draw(view, panel, preview, row, w, caps, &vp.moment.lead))
            .collect()
    }

    /// 高度按能画下的条数算,不按屏幕给了多少——理由同别的面板(见
    /// `crate::modules::rewind`)。
    fn height(_state: &State, moment: &Moment, width: u16) -> Height {
        let Some(panel) = moment.resume_panel.as_ref() else {
            return Height::Hug(0);
        };
        if width == 0 {
            return Height::Hug(0);
        }
        Height::Hug(
            layout(
                &moment.resume,
                panel,
                preview_of(moment, &moment.resume, panel),
                usize::MAX,
            )
            .len()
            .min(u16::MAX as usize) as u16,
        )
    }
}

/// 面板的一行,画出来之前。整张布局只有这一份:[`draw`]、[`geometry`] 和 `height`
/// 走的是同一个。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Rule,
    Header,
    BoxTop,
    Search,
    BoxBottom,
    Blank,
    /// 列表的第几行,是 [`ResumeView::listed`] 里的下标(筛出行里的位置,不是原始下
    /// 标)——光标、命中测试、[`crate::resume::key`] 都按这个位置走。
    Session(usize),
    /// 一个也没有,或筛掉了。
    Nothing,
    /// 上下各有多少条不在视野里。
    Scroll {
        above: usize,
        below: usize,
    },
    /// 选中那个会话最后聊的一句,第几行。
    Preview(usize),
    /// 还没问到。
    PreviewWaiting,
    Legend,
}

/// 选中那个会话的预览,照这个模块要的样子:`None` 没有,`Some(None)` 正在问,
/// `Some(Some(lines))` 是答案。只有当它问的正是**现在选中的**那个会话时才算数。
fn preview_of<'a>(
    moment: &'a Moment,
    view: &ResumeView,
    panel: &Panel,
) -> Option<Option<&'a Vec<String>>> {
    let (asked, lines) = moment.resume_preview.as_ref()?;
    let listed = view.listed(panel);
    let at = listed.get(panel.cursor)?;
    let selected = view.sessions().get(*at)?;
    (&selected.id == asked).then_some(lines.as_ref())
}

fn layout(
    view: &ResumeView,
    panel: &Panel,
    preview: Option<Option<&Vec<String>>>,
    h: usize,
) -> Vec<Row> {
    let mut rows = vec![
        Row::Rule,
        Row::Header,
        Row::BoxTop,
        Row::Search,
        Row::BoxBottom,
    ];
    let listed = view.listed(panel);
    if listed.is_empty() {
        rows.push(Row::Nothing);
    } else {
        // 上面 chrome、下面空行加提示各占了地方之后,列表还剩多少行。`h == usize::MAX`
        // 是 `height` 在问「要多少行」,答案是整张列表,仍夹在一块面板能占的上限里。
        let cap = h.saturating_sub(rows.len() + 2).min(MOST);
        let (from, to) = if listed.len() > cap {
            // 那条「还有多少没画」自己也要占一行,从同一份预算里扣。
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
            rows.push(Row::Session(at));
        }
    }
    // 选中那个会话最后聊了什么。列表下面、图例上面:它说的是「这一行是不是我要
    // 找的那个会话」,所以贴着列表;而它是读的,不是操作的,所以不进列表本身。
    match preview {
        Some(None) => rows.push(Row::PreviewWaiting),
        Some(Some(lines)) if !lines.is_empty() => {
            rows.push(Row::Blank);
            for at in 0..lines.len() {
                rows.push(Row::Preview(at));
            }
        }
        _ => {}
    }
    rows.push(Row::Blank);
    rows.push(Row::Legend);
    rows
}

/// 一份长为 `len` 的列表里,让 `cursor` 留在视野中的那 `room` 行的切片。
fn window(len: usize, cursor: usize, room: usize) -> (usize, usize) {
    if len <= room {
        return (0, len);
    }
    let half = room / 2;
    let from = cursor.saturating_sub(half).min(len - room);
    (from, from + room)
}

fn draw(
    view: &ResumeView,
    panel: &Panel,
    preview: Option<Option<&Vec<String>>>,
    row: Row,
    w: usize,
    caps: crate::caps::Caps,
    // `current`:此刻开着的那个会话,用来在列表里认出它自己。
    current: &str,
) -> Line {
    match row {
        Row::Rule => panel_edge(w, caps),
        Row::Header => Line::from_spans(chrome::header_parts(NAME, &[], usize::MAX).0).truncate(w),
        Row::BoxTop => box_edge(w, caps, true),
        Row::Search => search_line(&panel.query, Some(panel.query.len()), w, caps),
        Row::BoxBottom => box_edge(w, caps, false),
        Row::Blank => Line::empty(),
        Row::Nothing => Line::styled(
            width::take_width(&t(Msg::ResumeNoOthers), w),
            theme::fg(Role::Muted),
        ),
        Row::Scroll { above, below } => Line::styled(
            width::take_width(&format!("  ↑{above} ↓{below}"), w),
            theme::fg(Role::Muted),
        ),
        Row::Session(at) => session_line(view, panel, at, w, caps, current),
        Row::PreviewWaiting => Line::styled(
            format!("  {}", t(Msg::ResumePreviewWaiting)),
            theme::fg(Role::Muted),
        )
        .truncate(w),
        Row::Preview(at) => {
            let said = preview
                .flatten()
                .and_then(|lines| lines.get(at))
                .cloned()
                .unwrap_or_default();
            Line::styled(format!("  {said}"), theme::fg(Role::Muted)).truncate(w)
        }
        Row::Legend => Line::styled(
            format!("  {}", t(Msg::ResumePickerHint)),
            theme::fg(Role::Muted),
        )
        .truncate(w),
    }
}

/// 一行会话:亮色标题,后面压一段灰色元数据(`N 轮 · 时间 · 目录`)。光标所在的行
/// 铺一层选中底色、行首一个指针。
fn session_line(
    view: &ResumeView,
    panel: &Panel,
    at: usize,
    w: usize,
    caps: crate::caps::Caps,
    current: &str,
) -> Line {
    let listed = view.listed(panel);
    let Some(session) = listed.get(at).and_then(|&index| view.sessions().get(index)) else {
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
    // 待删的那一行说的是「再按一次」,不是它有几回合:此刻人要读的就是这一句。
    let meta = if panel.armed.as_deref() == Some(session.id.as_str()) {
        t(Msg::ResumeDeleteArmed).into_owned()
    } else if session.needs_newer_version {
        t(Msg::SessionNeedsNewerVersion { id: &session.id }).into_owned()
    } else {
        let when = crate::text::when(session.updated_at);
        match &session.working_dir {
            Some(dir) => t(Msg::SessionTurnsWhenWhere {
                turns: session.turns,
                when: &when,
                dir: &crate::text::collapse_home(dir),
            })
            .into_owned(),
            None => t(Msg::SessionTurnsWhen {
                turns: session.turns,
                when: &when,
            })
            .into_owned(),
        }
    };
    // 这一条就是正开着的那个会话。说出来,因为「恢复我正在用的这个」是一次
    // 无谓的重建:同一段对话会被重放一遍,而屏幕上本来就是它。
    let meta = if session.id == current {
        format!("{meta}  {}", t(Msg::RewindPanelCurrent))
    } else {
        meta
    };
    let armed = panel.armed.as_deref() == Some(session.id.as_str());
    let heading_style = if armed {
        base.under(theme::fg(Role::Error))
    } else if session.needs_newer_version {
        base.under(theme::fg(Role::Muted))
    } else {
        base.under(theme::fg(Role::PanelFg))
    };
    let spans = vec![
        crate::frame::Span::styled(pointer, base),
        crate::frame::Span::styled(session.heading().to_string(), heading_style),
        crate::frame::Span::styled(format!("  {meta}"), base.under(theme::fg(Role::Muted))),
    ];
    pad_to(Line::from_spans(spans).truncate(w), w, base)
}

/// 命中测试要的:每一屏行对应到列表里的哪个位置(筛出行里的下标)。
pub struct Geometry {
    rows: Vec<Option<usize>>,
}

impl Geometry {
    /// 第 `row` 屏行落在列表的哪个位置,不在会话行上就是 `None`。
    pub fn listed_at(&self, row: usize) -> Option<usize> {
        self.rows.get(row).copied().flatten()
    }
}

pub fn geometry(moment: &Moment, vp: &Viewport<'_>) -> Geometry {
    let Some(panel) = moment.resume_panel.as_ref() else {
        return Geometry { rows: Vec::new() };
    };
    let rows = layout(
        &moment.resume,
        panel,
        preview_of(moment, &moment.resume, panel),
        vp.rect.h as usize,
    );
    Geometry {
        rows: rows
            .into_iter()
            .map(|row| match row {
                Row::Session(at) => Some(at),
                _ => None,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resume::Session;

    fn session(id: &str, title: Option<&str>) -> Session {
        Session {
            id: id.into(),
            title: title.map(Into::into),
            working_dir: Some("/w/atomcode".into()),
            updated_at: 0,
            turns: 3,
            needs_newer_version: false,
        }
    }

    fn view() -> ResumeView {
        ResumeView::new(vec![
            session("aaa", Some("fix login")),
            session("bbb", Some("死代码扫描")),
        ])
    }

    fn text_with(
        view: &ResumeView,
        panel: &Panel,
        preview: Option<Option<&Vec<String>>>,
        w: usize,
    ) -> Vec<String> {
        text_as(view, panel, preview, w, "")
    }

    /// 同上,但说明「此刻开着的是哪个会话」。
    fn text_as(
        view: &ResumeView,
        panel: &Panel,
        preview: Option<Option<&Vec<String>>>,
        w: usize,
        current: &str,
    ) -> Vec<String> {
        layout(view, panel, preview, 40)
            .into_iter()
            .map(|row| {
                draw(
                    view,
                    panel,
                    preview,
                    row,
                    w,
                    crate::caps::Caps::default(),
                    current,
                )
                .plain()
            })
            .collect()
    }

    /// 选中一个会话,它最后聊的几句就在列表底下——列表说得出「什么时候、几回合」,
    /// 说不出「聊的是什么」,而后者才是人认出「就是这个」的依据。
    #[test]
    fn the_selected_session_shows_what_it_last_talked_about() {
        let (view, panel) = (view(), Panel::new());
        let lines = vec!["你: 把错误处理改一遍".to_string(), "它: 改完了".to_string()];
        let shown = text_with(&view, &panel, Some(Some(&lines)), 60).join("\n");
        assert!(shown.contains("把错误处理改一遍"), "{shown}");
        assert!(shown.contains("改完了"), "{shown}");
    }

    /// 正开着的那个会话,在列表里说出来。
    ///
    /// 「恢复我正在用的这个」是一次无谓的重建:同一段对话被重放一遍,而屏幕上
    /// 本来就是它。列表不说,人就只能靠标题去认——而标题正是最容易重名的那一项。
    #[test]
    fn the_session_already_open_says_so_in_the_list() {
        let (view, panel) = (view(), Panel::new());
        let shown = text_as(&view, &panel, None, 60, "bbb").join("\n");
        let marked: Vec<&str> = shown
            .lines()
            .filter(|line| line.contains("（当前）"))
            .collect();
        assert_eq!(marked.len(), 1, "只有一行是当前会话:{shown}");
        assert!(
            marked[0].contains("死代码扫描"),
            "而且是那一条:{:?}",
            marked[0]
        );

        // 不在列表里的会话:一行都不标,而不是退回去标第一条。
        let none = text_as(&view, &panel, None, 60, "somewhere-else").join("\n");
        assert!(!none.contains("（当前）"), "{none}");
    }

    /// 还没问到的时候说一句,而不是先空着再突然长出几行。
    #[test]
    fn a_preview_on_its_way_says_so() {
        let (view, panel) = (view(), Panel::new());
        let shown = text_with(&view, &panel, Some(None), 60).join("\n");
        assert!(shown.contains("正在读"), "{shown}");
    }

    /// 没有预览时,面板和从前一模一样:这一块是加出来的,不是把列表挤掉的。
    #[test]
    fn with_no_preview_the_panel_is_what_it_was() {
        let (view, panel) = (view(), Panel::new());
        let without = text_with(&view, &panel, None, 60);
        let lines = vec!["你: 一句".to_string()];
        let with = text_with(&view, &panel, Some(Some(&lines)), 60);
        assert!(with.len() > without.len(), "{with:?}");
        for row in &without {
            assert!(with.contains(row), "原来那些行都还在:{row}");
        }
    }

    fn text(view: &ResumeView, panel: &Panel, w: usize) -> Vec<String> {
        layout(view, panel, None, 40)
            .into_iter()
            .map(|row| draw(view, panel, None, row, w, crate::caps::Caps::default(), "").plain())
            .collect()
    }

    #[test]
    fn a_session_is_drawn_by_its_title_and_its_turn_count() {
        let view = view();
        let panel = Panel::new();
        let joined = text(&view, &panel, 80).join("\n");
        assert!(joined.contains("fix login"), "一个标题在屏上: {joined:?}");
        assert!(joined.contains("死代码扫描"), "另一个也在: {joined:?}");
        assert!(joined.contains('3'), "轮数是元数据的一半: {joined:?}");
    }

    #[test]
    fn typing_filters_the_list_on_screen() {
        let view = view();
        let mut panel = Panel::new();
        panel.query = "死".into();
        let joined = text(&view, &panel, 80).join("\n");
        assert!(joined.contains("死代码扫描"), "命中的留下: {joined:?}");
        assert!(!joined.contains("fix login"), "没命中的走了: {joined:?}");
    }

    #[test]
    fn an_empty_catalog_says_there_is_nothing_to_resume() {
        let view = ResumeView::new(Vec::new());
        let panel = Panel::new();
        let joined = text(&view, &panel, 80).join("\n");
        assert!(
            joined.contains(&*t(Msg::ResumeNoOthers)),
            "空目录说出来: {joined:?}"
        );
    }
}
