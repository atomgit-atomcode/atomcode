//! 回退面板:这次会话走过的回合,画出来。
//!
//! 和 `/config`、`/provider`、`/plugin`、`/toolbox` 从同一个地方升起来,是同一件事
//! 的第五块。
//!
//! 住在这儿的是**画**。回合**是什么**是数据([`crate::rewind::RewindView`]),回退从
//! [`crate::rewind::Rewind`] 出去——这个模块不知道有宿主控制契约这回事,也不知道一次
//! 回退在日志里是追加一条事实(`docs/adr/0022` §3、`docs/adr/0024`)。
//!
//! 一条条目占两行:**人说过的那句话**,底下压一行**那一回合动过什么**。两行是有理由
//! 的——「回到哪儿」人靠自己说过的话认,「要不要回」靠它改过多少东西判。只画一行,
//! 两个问题里总有一个没法回答。
//!
//! 列表的最后一行是「当前」:停在那儿按回车什么都不会发生。一块刚升起来就瞄准着某
//! 次回退的面板太容易走火,所以它升起来时就停在这一行上。
//!
//! 什么都不折叠:面板的状态在 [`crate::moment::Moment`] 里,那是「这块屏幕此刻在干
//! 什么」该待的地方。

use crate::frame::{Line, Style};
use crate::i18n::{t, Msg};
use crate::module::{Height, View};
use crate::modules::chrome::{self, pad_to, panel_edge};
use crate::moment::{Moment, Viewport};
use crate::rewind::{Panel, RewindView, Scope, Stage};
use crate::theme::{self, Role};
use crate::width;

pub const ID: &str = "rewind";

/// 面板的名字,表头里和命中测试里是同一个串。
const NAME: &str = "rewind";

/// 不管走过多少回合,列表最多画这么多条。一条占两行加一行空,四条正好是另外四块
/// 面板占的那块地方。
const MOST: usize = 4;

#[derive(Default)]
pub struct State;

pub struct Rewind;

impl View for Rewind {
    type State = State;

    fn id() -> &'static str {
        ID
    }

    fn absorb(_state: &mut State, _fact: &atomcode_harness::session::SessionEvent) {}

    fn render(_state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        let Some(panel) = vp.moment.rewind_panel.as_ref() else {
            return Vec::new();
        };
        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let view = &vp.moment.rewind;
        let caps = vp.moment.caps;
        layout(view, panel, vp.rect.h as usize, w)
            .into_iter()
            .map(|row| draw(view, panel, row, w, caps))
            .collect()
    }

    /// 高度按**能画下的条数**算,不按屏幕给了多少:面板的下沿是钉住的那条边,跟着
    /// 内容长短走的高度会在人上下走列表时把整块面板顺着屏幕拽。
    fn height(_state: &State, moment: &Moment, width: u16) -> Height {
        let Some(panel) = moment.rewind_panel.as_ref() else {
            return Height::Hug(0);
        };
        if width == 0 {
            return Height::Hug(0);
        }
        Height::Hug(
            layout(&moment.rewind, panel, usize::MAX, width as usize)
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
    /// 表头底下那句话:在这块面板里按一下会发生什么。
    About,
    /// 这次会话整个儿没在记代码改动,以及为什么——折过行的第几段。
    ///
    /// **说一次,不是每条说一次**;而且是折行不是截断:宿主那句话的后半截写着怎么
    /// 打开,截掉它等于把人读这行的理由截掉。
    NoCode(usize),
    Blank,
    /// 第几个回合,人说过的那句话。
    Prompt(usize),
    /// 同一个回合,它动过什么。
    Changes(usize),
    /// 末尾那行「当前」。
    Current,
    /// 一个回合都没有。
    Nothing,
    Scroll {
        above: usize,
        below: usize,
    },
    /// 第二步:把什么一起带回去。
    ScopeAsk,
    /// 第二步的一行:第几档范围。
    Choice(usize),
    Note,
    Busy,
    Legend,
}

fn layout(view: &RewindView, panel: &Panel, h: usize, width: usize) -> Vec<Row> {
    let mut rows = vec![Row::Rule, Row::Header];
    // 有活在跑:列表此刻不许动,画的就该是那件活本身。
    if panel.busy.is_some() {
        rows.push(Row::Blank);
        rows.push(Row::Busy);
        rows.push(Row::Blank);
        rows.push(Row::Legend);
        return rows;
    }
    if panel.stage == Stage::Scope {
        rows.push(Row::ScopeAsk);
        rows.push(Row::Blank);
        for at in 0..Scope::ALL.len() {
            rows.push(Row::Choice(at));
        }
        if panel.note.is_some() {
            rows.push(Row::Note);
        }
        rows.push(Row::Blank);
        rows.push(Row::Legend);
        return rows;
    }
    rows.push(Row::About);
    // 代码整个儿回不去的时候(没开、没建起来),说一次,说在最上面。逐条画「没有
    // 代码改动」是假话:那些回合可能改了一堆文件,只是这次会话没在记。
    let recording = view.code_unavailable().is_none();
    if !recording {
        for at in 0..why_lines(view, width).len() {
            rows.push(Row::NoCode(at));
        }
    }
    rows.push(Row::Blank);
    let points = view.points().len();
    if points == 0 {
        rows.push(Row::Nothing);
    } else {
        // 上头已经排进去的那几行,加上底下钉住的三行:「当前」、空行、图例
        // ——再加上那句提示,如果有的话。多算一行,一块正好放得下的列表就会开始
        // 滚动,而**量高度时用的是同一个 `layout`**,于是量出来的和画出来的会是
        // 两份东西。
        let reserved = rows.len() + 3 + usize::from(panel.note.is_some());
        // 一条占几行:那句话、它动过什么、一行空。没在记的时候第二行没有话说,
        // 于是一条占两行。
        let per = if recording { 3 } else { 2 };
        let room = h.saturating_sub(reserved) / per;
        let cap = room.clamp(1, MOST);
        let (from, to) = if points > cap {
            let (from, to) = window(
                points,
                panel.cursor.min(points - 1),
                cap.saturating_sub(1).max(1),
            );
            rows.push(Row::Scroll {
                above: from,
                below: points - to,
            });
            (from, to)
        } else {
            (0, points)
        };
        for at in from..to {
            rows.push(Row::Prompt(at));
            if recording {
                rows.push(Row::Changes(at));
            }
            rows.push(Row::Blank);
        }
    }
    rows.push(Row::Current);
    if panel.note.is_some() {
        rows.push(Row::Note);
    }
    rows.push(Row::Blank);
    rows.push(Row::Legend);
    rows
}

/// 「工作区为什么回不去」这句话,折成面板放得下的几行。
///
/// 折行不截断:默认关着的时候这句话就是「怎么打开」,截掉后半截等于把人读它的理由
/// 截掉;而一次真失败的原因可以任意长。
fn why_lines(view: &RewindView, w: usize) -> Vec<String> {
    let Some(why) = view.code_unavailable() else {
        return Vec::new();
    };
    // 两格缩进,和这块面板上别的话对齐。
    width::wrap(&why.say(), w.saturating_sub(2).max(1))
}

/// `len` 条里让 `cursor` 留在视野里的那一段。夹住而不是居中,同另外四块。
fn window(len: usize, cursor: usize, room: usize) -> (usize, usize) {
    if len <= room {
        return (0, len);
    }
    let half = room / 2;
    let from = cursor.saturating_sub(half).min(len - room);
    (from, from + room)
}

fn draw(view: &RewindView, panel: &Panel, row: Row, w: usize, caps: crate::caps::Caps) -> Line {
    match row {
        Row::Rule => panel_edge(w, caps),
        Row::Header => Line::from_spans(chrome::header_parts(NAME, &[], usize::MAX).0).truncate(w),
        Row::About => Line::styled(
            width::take_width(&t(Msg::RewindPanelAbout), w),
            theme::fg(Role::Muted),
        ),
        // 宿主的原话原样传:怎么打开这件事是宿主知道的(它那句话里就写着),而屏幕
        // 转述一遍只会转错。
        Row::NoCode(at) => Line::styled(
            width::take_width(
                &format!(
                    "  {}",
                    why_lines(view, w).get(at).cloned().unwrap_or_default()
                ),
                w,
            ),
            theme::fg(Role::Warning),
        ),
        Row::ScopeAsk => Line::styled(
            width::take_width(&t(Msg::RewindPanelScopeAsk), w),
            theme::fg(Role::Muted),
        ),
        Row::Blank => Line::empty(),
        Row::Nothing => Line::styled(
            width::take_width(&t(Msg::RewindPanelNoPoints), w),
            theme::fg(Role::Muted),
        ),
        Row::Scroll { above, below } => Line::styled(
            width::take_width(&format!("  ↑{above} ↓{below}"), w),
            theme::fg(Role::Muted),
        ),
        Row::Prompt(at) => prompt_line(view, panel, at, w, caps),
        Row::Changes(at) => changes_line(view, at, w),
        Row::Current => current_line(view, panel, w, caps),
        Row::Choice(at) => scope_line(view, panel, at, w, caps),
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

/// 选中的那一行画成一条亮带,没选中的用本色——和另外四块面板同一个做法。
fn row_style(here: bool) -> Style {
    if here {
        theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg))
    } else {
        Style::new()
    }
}

fn pointer(here: bool, caps: crate::caps::Caps) -> String {
    if here {
        format!("{} ", caps.g(crate::caps::Glyph::Pointer))
    } else {
        "  ".to_string()
    }
}

/// 人说过的那句话。取第一行:一段贴进来的多行提示词,在列表里只该占一行。
fn prompt_line(
    view: &RewindView,
    panel: &Panel,
    at: usize,
    w: usize,
    caps: crate::caps::Caps,
) -> Line {
    let Some(point) = view.points().get(at) else {
        return Line::empty();
    };
    let here = panel.cursor == at && panel.stage == Stage::Points;
    let base = row_style(here);
    let said = point.prompt.lines().next().unwrap_or_default().trim();
    let head = format!("{}{said}", pointer(here, caps));
    pad_to(Line::styled(width::take_width(&head, w), base), w, base)
}

/// 同一个回合的第二行:它动过什么。永远有话说——一条没有第二行的条目,读起来是
/// 「还没算完」。
fn changes_line(view: &RewindView, at: usize, w: usize) -> Line {
    let Some(point) = view.points().get(at) else {
        return Line::empty();
    };
    let (plus, minus) = point.totals();
    let what = match point.changes.len() {
        0 => t(Msg::RewindPanelNoCodeChanges).into_owned(),
        1 => format!("{}{}", point.changes[0].path, counts(plus, minus)),
        files => format!(
            "{}{}",
            t(Msg::RewindPanelFiles { files }),
            counts(plus, minus)
        ),
    };
    Line::styled(
        width::take_width(&format!("    {what}"), w),
        theme::fg(Role::Muted),
    )
}

/// `+484 -12`,零的那一半不画:一个 `-0` 会让人去找那个不存在的删除。
fn counts(plus: u64, minus: u64) -> String {
    let mut out = String::new();
    if plus > 0 {
        out.push_str(&format!(" +{plus}"));
    }
    if minus > 0 {
        out.push_str(&format!(" -{minus}"));
    }
    out
}

/// 末尾那行「当前」:会话此刻在哪儿,也是反悔的出口。
fn current_line(view: &RewindView, panel: &Panel, w: usize, caps: crate::caps::Caps) -> Line {
    let here = panel.stage == Stage::Points && panel.cursor >= view.points().len();
    let base = row_style(here);
    let head = format!("{}{}", pointer(here, caps), t(Msg::RewindPanelCurrent));
    pad_to(
        Line::styled(
            width::take_width(&head, w),
            theme::fg(Role::Muted).under(base),
        ),
        w,
        base,
    )
}

/// 第二步的一行:一档范围。选不中的那档画暗,并把为什么留到按下去的时候说——列表
/// 上一句解释会长得盖住三档本身。
fn scope_line(
    view: &RewindView,
    panel: &Panel,
    at: usize,
    w: usize,
    caps: crate::caps::Caps,
) -> Line {
    let Some(scope) = Scope::ALL.get(at).copied() else {
        return Line::empty();
    };
    let here = panel.scope == at;
    let base = row_style(here);
    let usable = view.scope_why_not(panel, scope).is_none();
    let style = if usable {
        base
    } else {
        theme::fg(Role::Muted).under(base)
    };
    let head = format!("{}{}", pointer(here, caps), scope.about());
    pad_to(Line::styled(width::take_width(&head, w), style), w, base)
}

fn legend(panel: &Panel) -> Vec<(String, String)> {
    if panel.busy.is_some() {
        return vec![(
            "esc".to_string(),
            t(Msg::RewindPanelStopWaiting).into_owned(),
        )];
    }
    match panel.stage {
        Stage::Points => vec![
            ("↑↓".to_string(), t(Msg::RewindLegendChoose).into_owned()),
            ("⏎".to_string(), t(Msg::RewindLegendContinue).into_owned()),
            ("esc".to_string(), t(Msg::RewindLegendClose).into_owned()),
        ],
        Stage::Scope => vec![
            ("↑↓".to_string(), t(Msg::RewindLegendChoose).into_owned()),
            ("⏎".to_string(), t(Msg::RewindLegendGo).into_owned()),
            ("esc".to_string(), t(Msg::RewindLegendBack).into_owned()),
        ],
    }
}

/// 屏幕上的行 ↔ 列表里的第几个,给点击用。一条条目的两行都算它自己那一条:点在
/// 「动过什么」上和点在那句话上,指的是同一个回合。
pub struct Geometry {
    rows: Vec<Option<usize>>,
}

impl Geometry {
    pub fn row_at(&self, row: usize) -> Option<usize> {
        self.rows.get(row).copied().flatten()
    }
}

pub fn geometry(moment: &Moment, vp: &Viewport<'_>) -> Geometry {
    let Some(panel) = moment.rewind_panel.as_ref() else {
        return Geometry { rows: Vec::new() };
    };
    let points = moment.rewind.points().len();
    let rows = layout(
        &moment.rewind,
        panel,
        vp.rect.h as usize,
        vp.rect.w as usize,
    );
    Geometry {
        rows: rows
            .into_iter()
            .map(|row| match row {
                Row::Prompt(at) | Row::Changes(at) => Some(at),
                Row::Current => Some(points),
                _ => None,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewind::{Change, Point};

    fn view() -> RewindView {
        RewindView::new(
            vec![
                Point {
                    turn: 1,
                    prompt: "写个解析器\n第二行不该出现在列表里".into(),
                    changes: vec![Change {
                        path: "rewind.rs".into(),
                        additions: 484,
                        deletions: 12,
                    }],
                    code: true,
                },
                Point {
                    turn: 2,
                    prompt: "再加一个测试".into(),
                    changes: Vec::new(),
                    code: false,
                },
            ],
            None,
        )
    }

    fn open(view: &RewindView) -> Panel {
        let mut panel = Panel {
            busy: None,
            ..Panel::new()
        };
        panel.rest_at_current(view.rows());
        panel
    }

    fn text(view: &RewindView, panel: &Panel, w: usize) -> Vec<String> {
        layout(view, panel, 40, w)
            .into_iter()
            .map(|row| draw(view, panel, row, w, crate::caps::Caps::default()).plain())
            .collect()
    }

    /// 一条条目占两行:人说过的那句话,底下压一行它动过什么。只画一行的话,「回到
    /// 哪儿」和「要不要回」总有一个问题没法回答。
    #[test]
    fn every_turn_is_drawn_as_what_was_said_over_what_it_changed() {
        let view = view();
        let panel = open(&view);
        let lines = text(&view, &panel, 60);
        let said = lines
            .iter()
            .position(|l| l.contains("写个解析器"))
            .expect("那句话要在屏上");
        assert!(
            lines[said + 1].contains("rewind.rs") && lines[said + 1].contains("+484"),
            "紧挨着的下一行是它动过什么: {:?}",
            lines[said + 1]
        );
    }

    /// 贴进来的多行提示词在列表里只占一行:第二行不该跑出来把列表撑开。
    #[test]
    fn a_multi_line_prompt_takes_one_row() {
        let view = view();
        let panel = open(&view);
        assert!(
            !text(&view, &panel, 60)
                .iter()
                .any(|l| l.contains("第二行不该出现在列表里")),
            "多行提示词只画第一行"
        );
    }

    /// 没动过文件的回合也有第二行,说的是「没有代码改动」:一条只有一行的条目读起来
    /// 是「还没算完」。
    #[test]
    fn a_turn_that_changed_nothing_still_says_so() {
        let view = view();
        let panel = open(&view);
        let lines = text(&view, &panel, 60);
        let said = lines
            .iter()
            .position(|l| l.contains("再加一个测试"))
            .expect("那句话要在屏上");
        assert!(
            !lines[said + 1].trim().is_empty(),
            "没有改动也要说出来: {:?}",
            lines[said + 1]
        );
    }

    /// **没在记代码改动的时候,面板说的是这件事本身,不是「这些回合没改过文件」。**
    ///
    /// 这两句话差得很远:一句是「你改的东西都在,只是回不去」,另一句是「你没改过
    /// 东西」。真跑起来撞见的就是这个——Code Rewind 默认关着(省磁盘),而屏幕逐条
    /// 画着「没有代码改动」,人刚刚亲眼看着它写了两个文件。
    #[test]
    fn a_session_that_is_not_recording_code_says_that_once_rather_than_lying_per_turn() {
        let view = RewindView::new(
            vec![Point {
                turn: 1,
                prompt: "建一个 tmp.txt".into(),
                // 宿主没在记,所以明细是空的——而这**不**表示这一回合没改过文件。
                changes: Vec::new(),
                code: false,
            }],
            Some(crate::rewind::CodeOff::NotEnabled),
        );
        let panel = open(&view);
        let lines = text(&view, &panel, 78);
        assert!(
            lines.iter().any(|l| l.contains("ATOMCODE_CODE_REWIND")),
            "宿主给的原话要在最上面说一次: {lines:?}"
        );
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.contains(&t(Msg::RewindPanelNoCodeChanges).to_string()))
                .count(),
            0,
            "而且一条都不许说「没有代码改动」——那句话此刻是假的: {lines:?}"
        );
        // 阴性对照:在记的时候,那句话**应该**出现,而顶上那行不出现。
        let recording = RewindView::new(
            vec![Point {
                turn: 1,
                prompt: "建一个 tmp.txt".into(),
                changes: Vec::new(),
                code: true,
            }],
            None,
        );
        let panel = open(&recording);
        let lines = text(&recording, &panel, 78);
        assert!(
            lines
                .iter()
                .any(|l| l.contains(&t(Msg::RewindPanelNoCodeChanges).to_string())),
            "在记的时候,没改过就是没改过: {lines:?}"
        );
    }

    /// 最后一行是「当前」,面板升起来就停在它上面——一块刚升起来就瞄准着某次回退的
    /// 面板太容易走火。
    #[test]
    fn the_last_row_is_where_the_session_is_now() {
        let view = view();
        let panel = open(&view);
        let lines = text(&view, &panel, 60);
        let at = lines
            .iter()
            .rposition(|l| !l.trim().is_empty() && !l.contains("↑↓"))
            .expect("总有内容");
        assert!(
            lines[at].contains(&t(Msg::RewindPanelCurrent).to_string()),
            "列表的最后一行是「当前」: {:?}",
            lines[at]
        );
        assert!(
            lines[at].trim_start().starts_with('▸'),
            "而且光标停在它上面"
        );
    }

    /// 正在读回合的时候画的是那件活,不是一张空列表——空列表会说「还没有能回到的
    /// 回合」,而那句话此刻是假的。
    #[test]
    fn a_panel_still_reading_draws_the_work_rather_than_an_empty_list() {
        let view = RewindView::default();
        let panel = Panel::new();
        let lines = text(&view, &panel, 60);
        assert!(
            lines
                .iter()
                .any(|l| l.contains(t(Msg::RewindPanelReading).trim())),
            "屏上要说正在读: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains(t(Msg::RewindPanelNoPoints).trim())),
            "而不是说没有回合"
        );
    }

    /// 量出来的高度和画出来的行数是同一件事:两边都走 `layout`,而一块正好放得下
    /// 的列表不该在量完之后开始滚动。
    #[test]
    fn the_height_it_asks_for_is_the_height_it_draws() {
        let view = view();
        let panel = open(&view);
        // `height` 问的那一次。
        let asked = layout(&view, &panel, usize::MAX, 60).len();
        // `render` 画的那一次,拿到的正是上面那个数。
        let drawn = layout(&view, &panel, asked, 60);
        assert_eq!(drawn.len(), asked, "给多少画多少");
        assert!(
            !drawn.iter().any(|row| matches!(row, Row::Scroll { .. })),
            "放得下就不该滚动: {drawn:?}"
        );
        // 阴性对照:少给一行,它**应该**开始滚动——上面那条不是恒真。
        let tight = layout(&view, &panel, asked - 1, 60);
        assert!(
            tight.iter().any(|row| matches!(row, Row::Scroll { .. })),
            "地方不够的时候要滚动: {tight:?}"
        );
    }

    /// 第二步画的是那两档范围,不是列表。
    #[test]
    fn the_second_step_draws_the_scopes_rather_than_the_list() {
        let view = view();
        let mut panel = open(&view);
        panel.cursor = 0;
        panel.stage = Stage::Scope;
        let lines = text(&view, &panel, 60);
        for scope in Scope::ALL {
            assert!(
                lines.iter().any(|l| l.contains(&scope.about())),
                "{} 要在屏上: {lines:?}",
                scope.about()
            );
        }
        assert!(
            !lines.iter().any(|l| l.contains("写个解析器")),
            "第二步问的是范围,列表让位"
        );
    }

    /// 点在「动过什么」那一行上,指的是它上面那句话所在的回合——一条条目的两行是
    /// 一个东西。
    #[test]
    fn a_click_on_either_row_of_a_turn_finds_that_turn() {
        let view = view();
        let panel = open(&view);
        let rows = layout(&view, &panel, 40, 60);
        let mapped: Vec<Option<usize>> = rows
            .iter()
            .map(|row| match row {
                Row::Prompt(at) | Row::Changes(at) => Some(*at),
                Row::Current => Some(view.points().len()),
                _ => None,
            })
            .collect();
        let first = mapped.iter().position(|m| *m == Some(0)).expect("第一条在");
        assert_eq!(mapped[first + 1], Some(0), "紧挨着的那一行还是它");
        assert!(
            mapped.contains(&Some(view.points().len())),
            "「当前」也点得着"
        );
    }
}
