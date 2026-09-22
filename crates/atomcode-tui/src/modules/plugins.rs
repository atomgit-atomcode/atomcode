//! 插件面板:这台机器能装什么、装了什么、从哪儿装,在输入框的位置上列出来、走
//! 一走、装上或者拿掉。
//!
//! 和 provider 面板、设置面板从同一个地方升起来,是同一件事的三块:人在里面干活
//! 的面板占住输入框的位置,因为那本身就是一个回合。老前端把这块画成屏幕中央的一个
//! 模态（`atomcode-tuix/src/modals/plugin_manager.rs`,九个子屏）,这里不是。
//!
//! 住在这儿的是**画**。插件**是什么**是数据（[`crate::plugins::PluginsView`]）,
//! 改动从 [`crate::plugins::Plugins`] 出去——这个模块不知道市场是一次 git clone,
//! 也叫不出任何一个文件名（`docs/adr/0022` §3）。
//!
//! 什么都不折叠。日志记的是「装了一个插件」,不是「一张表单里打了三个字」,所以没有
//! 可折的事实:面板的状态在 [`crate::moment::Moment`] 里,那是「这块屏幕此刻在干
//! 什么」该待的地方。

use crate::frame::{Line, Span, Style};
use crate::i18n::{t, Msg};
use crate::module::{Height, View};
use crate::modules::chrome::{
    self, box_edge, caret_spans, pad_to, panel_edge, search_line, LABEL_MAX, LABEL_MIN, LEAD,
};
use crate::moment::{Moment, Viewport};
use crate::plugins::{
    AddMarketForm, Form, Listed, MarketForm, Panel, PluginAction, PluginForm, PluginsView, Scope,
    ScopeForm, Tab,
};
use crate::theme::{self, Role};
use crate::width;

pub const ID: &str = "plugins";

/// 面板的名字,表头里和命中测试里是同一个串——不然点击会拿去和一个已经不画了的
/// 标题比。
const NAME: &str = "plugin";

/// 不管有多少插件,列表最多占这么多行。
///
/// 一个装了四十个插件的市场不该把对话顶出屏幕;再多就在光标底下滚。和 provider
/// 面板同一个数,因为它们占的是同一块地方。
const MOST: usize = 12;

/// 什么都不折叠——理由见本模块自己的文档。
#[derive(Default)]
pub struct State;

pub struct Plugins;

impl View for Plugins {
    type State = State;

    fn id() -> &'static str {
        ID
    }

    fn absorb(_state: &mut State, _fact: &atomcode_harness::session::SessionEvent) {}

    fn render(_state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        let Some(panel) = vp.moment.plugins_panel.as_ref() else {
            return Vec::new();
        };
        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let view = &vp.moment.plugins;
        let caps = vp.moment.caps;
        layout(view, panel, vp.rect.h as usize)
            .into_iter()
            .map(|row| draw(view, panel, row, w, caps))
            .collect()
    }

    /// 这块面板在这个宽度上要多少行。
    ///
    /// 按**没过滤**的清单数,不管打了什么,理由同 provider 面板:面板是尾巴上最新的
    /// 东西,它的下沿是钉住的那条边,而一个跟着过滤走的高度会在人打字时把搜索框
    /// 顺着屏幕往下拽。
    fn height(_state: &State, moment: &Moment, width: u16) -> Height {
        let Some(panel) = moment.plugins_panel.as_ref() else {
            return Height::Hug(0);
        };
        if width == 0 {
            return Height::Hug(0);
        }
        let mut open = panel.clone();
        open.query.clear();
        Height::Hug(
            layout(&moment.plugins, &open, usize::MAX)
                .len()
                .min(u16::MAX as usize) as u16,
        )
    }
}

/// 面板的一行,画出来之前。
///
/// 整张布局只有这一份:[`draw`] 走它去画,[`geometry`] 走它去认哪一行是哪一个,
/// `height` 走它去数——和 `crate::modules::providers` 同一个形状,同一条理由。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Row {
    Rule,
    Header,
    BoxTop,
    Search,
    BoxBottom,
    Blank,
    /// 列表的一行,索引进 [`PluginsView::listed`]。
    Listed(usize),
    /// 过滤没筛着东西时说的那句话。
    Nothing,
    /// 上下各还有多少行看不见。
    Scroll {
        above: usize,
        below: usize,
    },
    /// 有活在外面跑着的那句话。
    Busy,
    /// 表单的标题。
    FormHead,
    /// 表单的一行,索引进它自己的选项。
    Choice(usize),
    /// 加市场那张表单唯一的字段。
    Url,
    /// 表单底下的一段说明。
    /// One line of the note under the add-a-marketplace form.
    ///
    /// The message rather than the text: `Row` is `Copy` and built afresh every
    /// frame, and `Msg` is `Copy` too — so the words are looked up when the row
    /// is drawn, in whatever language is in force then.
    Note(Msg<'static>),
    Legend,
}

fn layout(view: &PluginsView, panel: &Panel, h: usize) -> Vec<Row> {
    let mut rows = vec![Row::Rule, Row::Header];
    // 有活在跑的时候,列表和表单都让位:此刻屏上唯一有意义的事实是那件活,而一个
    // 还能走的列表会让人以为自己还能再按一次。
    if panel.busy.is_some() {
        rows.push(Row::Blank);
        rows.push(Row::Busy);
        rows.push(Row::Blank);
        rows.push(Row::Legend);
        return rows;
    }
    match &panel.form {
        Some(form) => {
            rows.push(Row::FormHead);
            rows.push(Row::Blank);
            match form {
                Form::Scope(_) => {
                    for at in 0..Scope::ALL.len() {
                        rows.push(Row::Choice(at));
                    }
                }
                Form::Plugin(_) => {
                    for at in 0..PluginAction::ALL.len() {
                        rows.push(Row::Choice(at));
                    }
                }
                Form::Market(f) => {
                    for at in 0..f.actions().len() {
                        rows.push(Row::Choice(at));
                    }
                }
                Form::AddMarket(_) => {
                    rows.push(Row::Url);
                    rows.push(Row::Blank);
                    for note in ADD_MARKET_NOTES {
                        rows.push(Row::Note(note));
                    }
                }
            }
            rows.push(Row::Blank);
            rows.push(Row::Legend);
        }
        None => {
            rows.push(Row::BoxTop);
            rows.push(Row::Search);
            rows.push(Row::BoxBottom);
            let listed = view.listed(panel);
            if listed.is_empty() {
                rows.push(Row::Nothing);
            } else {
                let cap = h.saturating_sub(rows.len() + 2).min(MOST);
                let (from, to) = if listed.len() > cap {
                    // 说「还有多少看不见」的那一行,行数从同一份预算里出。事后再数
                    // 它,面板就会比拿到的矩形高一行。
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
            rows.push(Row::Blank);
            rows.push(Row::Legend);
        }
    }
    rows
}

/// 加市场那张表单底下的例子。
///
/// 三种写法各一行,因为人手里拿的多半是其中一种,而「支持哪几种」这件事,只有把
/// 它们摆出来才说得清。
const ADD_MARKET_NOTES: [Msg<'static>; 4] = [
    Msg::AddMarketNotesHead,
    Msg::AddMarketNoteHttps,
    Msg::AddMarketNoteSsh,
    Msg::AddMarketNoteLocal,
];

/// `len` 行里让 `cursor` 留在视野里的那一段,一次 `room` 行。
///
/// 夹住而不是居中:比视野短的列表整张画出来,光标靠着哪一头就把那一头留在屏上,
/// 而不是为了把自己摆在中间把那一头滚出去。
fn window(len: usize, cursor: usize, room: usize) -> (usize, usize) {
    if len <= room {
        return (0, len);
    }
    let half = room / 2;
    let from = cursor.saturating_sub(half).min(len - room);
    (from, from + room)
}

fn draw(view: &PluginsView, panel: &Panel, row: Row, w: usize, caps: crate::caps::Caps) -> Line {
    match row {
        Row::Rule => panel_edge(w, caps),
        Row::Header => {
            let labels: Vec<String> = Tab::ALL
                .iter()
                .map(|t| match t {
                    Tab::Installed => format!("{}（{}）", t.label(), view.installed_count()),
                    other => other.label().to_string(),
                })
                .collect();
            let labels: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
            let at = Tab::ALL.iter().position(|t| *t == panel.tab).unwrap_or(0);
            Line::from_spans(chrome::header_parts(NAME, &labels, at).0).truncate(w)
        }
        Row::BoxTop => box_edge(w, caps, true),
        Row::Search => search_line(&panel.query, Some(panel.query.len()), w, caps),
        Row::BoxBottom => box_edge(w, caps, false),
        Row::Blank => Line::empty(),
        Row::Nothing => Line::styled(
            width::take_width(
                &match panel.tab {
                    Tab::All if view.plugins().is_empty() => t(Msg::PluginsNoMarketsYet),
                    Tab::All => t(Msg::PluginsNoMatch),
                    Tab::Installed if panel.query.is_empty() => t(Msg::PluginsNothingInstalled),
                    Tab::Installed => t(Msg::PluginsNoMatchInstalled),
                    Tab::Markets => t(Msg::PluginsNoMatchMarkets),
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
        Row::Busy => {
            let what = panel
                .busy
                .as_ref()
                .map(|b| b.what.clone())
                .unwrap_or_default();
            Line::styled(
                width::take_width(&format!("  {what}"), w),
                theme::fg(Role::Warning),
            )
        }
        Row::FormHead => Line::styled(
            width::take_width(&format!("  {}", form_title(panel)), w),
            theme::fg(Role::Brand),
        ),
        Row::Choice(at) => choice_line(panel, at, w, caps),
        Row::Url => url_line(panel, w, caps),
        Row::Note(msg) => Line::styled(width::take_width(&t(msg), w), theme::fg(Role::Muted)),
        Row::Legend => Line::styled(
            format!("  {}", crate::widget::keys(&legend(panel), caps)),
            theme::fg(Role::Muted),
        )
        .truncate(w),
    }
}

fn form_title(panel: &Panel) -> String {
    match &panel.form {
        Some(Form::Scope(f)) => t(Msg::PluginFormScopeTitle {
            plugin: &f.plugin,
            marketplace: &f.marketplace,
        })
        .into_owned(),
        Some(Form::Plugin(f)) => format!("{}@{}", f.plugin, f.marketplace),
        Some(Form::Market(f)) => f.name.clone(),
        Some(Form::AddMarket(_)) => t(Msg::PluginFormAddMarketTitle).into_owned(),
        None => String::new(),
    }
}

/// 列表的一行:它是什么,以及关于它值得知道的那一点。
fn listed_line(
    view: &PluginsView,
    panel: &Panel,
    at: usize,
    w: usize,
    caps: crate::caps::Caps,
) -> Line {
    let listed = view.listed(panel);
    let Some(what) = listed.get(at).copied() else {
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
    let (mark, label, about, dim) = match what {
        Listed::AddMarket => (
            " ".to_string(),
            t(Msg::PluginFormAddMarketTitle).into_owned(),
            "^a".to_string(),
            true,
        ),
        Listed::Plugin(i) => {
            let Some(row) = view.plugins().get(i) else {
                return Line::empty();
            };
            let mut about = vec![format!("@{}", row.marketplace)];
            if let Some(scope) = row.installed {
                about.push(scope.short().to_string());
            }
            if !row.description.is_empty() {
                about.push(row.description.clone());
            }
            (
                // 装上的那一个打点。和 provider 面板里「正在用的那个模型」是同一个
                // 记号,因为答的是同一种问题:这一行此刻是不是生效的。
                mark_for(row.installed.is_some(), caps),
                row.name.clone(),
                about.join(" · "),
                false,
            )
        }
        Listed::Market(i) => {
            let Some(row) = view.markets().get(i) else {
                return Line::empty();
            };
            let mut about = vec![t(Msg::MarketPluginCount { n: row.plugins }).into_owned()];
            if row.installed > 0 {
                about.push(t(Msg::MarketInstalledCount { n: row.installed }).into_owned());
            }
            if !row.updated.is_empty() {
                about.push(row.updated.clone());
            }
            about.push(row.source.clone());
            (
                mark_for(row.installed > 0, caps),
                row.name.clone(),
                about.join(" · "),
                false,
            )
        }
    };
    // 再按一次 ^d 就会没的那一行,把这句话写在它自己的描述位置上:人要扔掉一样东西
    // 之前,该在那样东西身上读到它,而不是在屏幕角落里。
    let armed = panel.pending_delete.as_deref().is_some_and(|armed| {
        armed
            == match what {
                Listed::Plugin(i) => match view.plugins().get(i) {
                    Some(row) => row.id(),
                    None => return false,
                },
                Listed::Market(i) => match view.markets().get(i) {
                    Some(row) => format!("market:{}", row.name),
                    None => return false,
                },
                Listed::AddMarket => return false,
            }
    });
    let label_room = w.saturating_sub(LEAD + 2).clamp(LABEL_MIN, LABEL_MAX);
    let shown = width::take_width(&label, label_room);
    let pad = label_room.saturating_sub(width::str_width(&shown));
    let label_style = if dim {
        base.under(theme::fg(Role::Muted))
    } else {
        base
    };
    let about_style = if armed {
        base.under(theme::fg(Role::Warning))
    } else {
        base.under(theme::fg(Role::Muted))
    };
    let about = if armed {
        match what {
            // 删一个市场连带卸掉从它装的插件——这件事只在这里说得出口,而且必须
            // 在按下去之前说。
            Listed::Market(i) => match view.markets().get(i).map(|m| m.installed).unwrap_or(0) {
                0 => t(Msg::ArmedRemoveMarket).into_owned(),
                n => t(Msg::ArmedRemoveMarketWithPlugins { n }).into_owned(),
            },
            _ => t(Msg::ArmedUninstall).into_owned(),
        }
    } else {
        about
    };
    let spans = vec![
        Span::styled(pointer, base),
        Span::styled(format!("{mark} "), label_style),
        Span::styled(shown, label_style),
        Span::styled(" ".repeat(pad + 2), base),
        Span::styled(about, about_style),
    ];
    pad_to(Line::from_spans(spans), w, base)
}

fn mark_for(on: bool, caps: crate::caps::Caps) -> String {
    match on {
        true => caps.g(crate::caps::Glyph::Bullet).to_string(),
        false => " ".to_string(),
    }
}

/// 表单里的一行选项。
fn choice_line(panel: &Panel, at: usize, w: usize, caps: crate::caps::Caps) -> Line {
    let Some(form) = panel.form.as_ref() else {
        return Line::empty();
    };
    let (label, about, focused) = match form {
        Form::Scope(ScopeForm { at: cursor, .. }) => {
            let Some(scope) = Scope::ALL.get(at).copied() else {
                return Line::empty();
            };
            (
                scope.label().to_string(),
                scope.about().to_string(),
                at == *cursor,
            )
        }
        Form::Plugin(PluginForm { at: cursor, .. }) => {
            let Some(action) = PluginAction::ALL.get(at).copied() else {
                return Line::empty();
            };
            (
                action.label().to_string(),
                action.about().to_string(),
                at == *cursor,
            )
        }
        Form::Market(f @ MarketForm { at: cursor, .. }) => {
            let actions = f.actions();
            let Some(action) = actions.get(at).copied() else {
                return Line::empty();
            };
            (
                action.label().to_string(),
                action.about().to_string(),
                at == *cursor,
            )
        }
        Form::AddMarket(_) => return Line::empty(),
    };
    let base = if focused {
        theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg))
    } else {
        Style::new()
    };
    let pointer = if focused {
        format!("{} ", caps.g(crate::caps::Glyph::Pointer))
    } else {
        "  ".to_string()
    };
    let label_room = w.saturating_sub(LEAD + 2).clamp(LABEL_MIN, LABEL_MAX);
    let shown = width::take_width(&label, label_room);
    let pad = label_room.saturating_sub(width::str_width(&shown));
    let spans = vec![
        Span::styled(pointer, base),
        Span::styled(shown, base),
        Span::styled(" ".repeat(pad + 2), base),
        Span::styled(about, base.under(theme::fg(Role::Muted))),
    ];
    pad_to(Line::from_spans(spans), w, base)
}

/// 加市场那张表单的地址栏,光标在里面。
fn url_line(panel: &Panel, w: usize, caps: crate::caps::Caps) -> Line {
    let Some(Form::AddMarket(AddMarketForm { url, caret })) = panel.form.as_ref() else {
        return Line::empty();
    };
    let base = theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg));
    let pointer = format!("{} ", caps.g(crate::caps::Glyph::Pointer));
    let label = t(Msg::FieldAddress);
    let label_room = w.saturating_sub(LEAD + 2).clamp(LABEL_MIN, LABEL_MAX);
    let shown = width::take_width(&label, label_room);
    let pad = label_room.saturating_sub(width::str_width(&shown));
    let room = w.saturating_sub(LEAD + label_room + 2);
    let mut spans = vec![
        Span::styled(pointer, base),
        Span::styled(shown, base),
        Span::styled(" ".repeat(pad + 2), base),
    ];
    spans.extend(caret_spans(url, *caret, base, room));
    pad_to(Line::from_spans(spans), w, base)
}

fn legend(panel: &Panel) -> Vec<(String, String)> {
    let key = |k: &str, msg: Msg<'_>| (k.to_string(), t(msg).into_owned());
    if panel.busy.is_some() {
        return vec![key("esc", Msg::LegendStopWaiting)];
    }
    match &panel.form {
        Some(Form::AddMarket(_)) => {
            vec![key("⏎", Msg::LegendAdd), key("esc", Msg::LegendCancel)]
        }
        Some(_) => vec![
            key("↑↓", Msg::LegendSelect),
            key("⏎", Msg::LegendThisOne),
            key("esc", Msg::LegendBack),
        ],
        None => {
            if panel.pending_delete.is_some() {
                return vec![
                    key("^d", Msg::LegendPressAgain),
                    (
                        t(Msg::LegendAnyOtherKey).into_owned(),
                        t(Msg::LegendCancel).into_owned(),
                    ),
                ];
            }
            let mut out = vec![key("↑↓", Msg::LegendSelect)];
            out.push(match panel.tab {
                Tab::All => key("⏎", Msg::LegendInstallOrOpen),
                Tab::Installed => key("⏎", Msg::LegendUpdateOrRemove),
                Tab::Markets => key("⏎", Msg::LegendOpen),
            });
            if panel.tab == Tab::Markets {
                out.push(key("^a", Msg::LegendAddMarket));
            }
            out.push(key("^d", Msg::LegendTakeAway));
            out.push(key("⇥", Msg::LegendChangePage));
            out.push(key("esc", Msg::LegendClose));
            out
        }
    }
}

/// 屏上每一行载着列表的第几行,给点击读。
///
/// 用画这一帧的那份 [`layout`] 建的,所以点到的行和亮起来的行不会来自两套排布。
pub struct Geometry {
    rows: Vec<Option<usize>>,
    header: Option<usize>,
}

impl Geometry {
    pub fn listed_at(&self, row: usize) -> Option<usize> {
        self.rows.get(row).copied().flatten()
    }

    pub fn header_row(&self) -> Option<usize> {
        self.header
    }
}

pub fn geometry(moment: &Moment, vp: &Viewport<'_>) -> Geometry {
    let Some(panel) = moment.plugins_panel.as_ref() else {
        return Geometry {
            rows: Vec::new(),
            header: None,
        };
    };
    let rows = layout(&moment.plugins, panel, vp.rect.h as usize);
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

/// 指针落在表头的第几页上。
///
/// 标签要跟着 moment 走:「已装」后面带着数字,而那个数字的宽度会变——拿一份写死的
/// 标签去算列,数字一变,点到的就是隔壁那一页。
pub fn tab_at(moment: &Moment, col: usize) -> Option<Tab> {
    let labels = tab_labels(moment);
    let labels: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
    chrome::tab_at(NAME, &labels, col).map(|at| Tab::ALL[at])
}

/// 三个页签的字面,「已装」那个带着数字。
fn tab_labels(moment: &Moment) -> Vec<String> {
    Tab::ALL
        .iter()
        .map(|t| match t {
            Tab::Installed => format!("{}（{}）", t.label(), moment.plugins.installed_count()),
            other => other.label().to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rect;
    use crate::plugins::{Busy, MarketRow, PluginRow};

    fn plugin(name: &str, market: &str, about: &str, installed: Option<Scope>) -> PluginRow {
        PluginRow {
            name: name.into(),
            marketplace: market.into(),
            description: about.into(),
            installed,
        }
    }

    fn view() -> PluginsView {
        PluginsView::new(
            vec![
                plugin("lens", "official", "看一眼改了什么", Some(Scope::Project)),
                plugin("tidy", "official", "把代码排整齐", None),
            ],
            vec![
                MarketRow {
                    name: "official".into(),
                    source: "https://example.com/official.git".into(),
                    plugins: 2,
                    installed: 1,
                    updated: "今天更新".into(),
                    official: true,
                },
                MarketRow {
                    name: "mine".into(),
                    source: "https://example.com/mine.git".into(),
                    plugins: 3,
                    installed: 2,
                    updated: "3 天前更新".into(),
                    official: false,
                },
            ],
        )
    }

    fn moment(panel: Option<Panel>) -> Moment {
        Moment {
            plugins: view(),
            plugins_panel: panel,
            ..Moment::default()
        }
    }

    fn lines(m: &Moment, w: u16, h: u16) -> Vec<Line> {
        let vp = Viewport::new(Rect::sized(w, h), m);
        Plugins::render(&State, &vp)
    }

    fn drawn(m: &Moment, w: u16, h: u16) -> String {
        lines(m, w, h)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_panel_that_is_not_up_draws_nothing() {
        assert!(lines(&moment(None), 60, 20).is_empty());
    }

    /// The check every panel in this crate keeps: a row wider than its rect is a
    /// row that writes over whatever is beside it.
    #[test]
    fn no_row_ever_draws_wider_than_its_rect() {
        for w in [4u16, 12, 40, 80, 200] {
            for panel in [
                Panel::new(),
                Panel {
                    tab: Tab::Markets,
                    ..Panel::new()
                },
                Panel {
                    query: "nothing".into(),
                    ..Panel::new()
                },
                Panel {
                    form: Some(Form::Scope(crate::plugins::ScopeForm {
                        plugin: "tidy".into(),
                        marketplace: "official".into(),
                        at: 0,
                    })),
                    ..Panel::new()
                },
                Panel {
                    form: Some(Form::AddMarket(crate::plugins::AddMarketForm {
                        url: "https://example.com/a-rather-long-marketplace-url.git".into(),
                        caret: 5,
                    })),
                    ..Panel::new()
                },
                Panel {
                    busy: Some(Busy {
                        what: "正在装 tidy@official …".into(),
                        job: "tidy@official".into(),
                    }),
                    ..Panel::new()
                },
            ] {
                let m = moment(Some(panel));
                for line in lines(&m, w, 20) {
                    assert!(
                        line.width() <= w as usize,
                        "a row {} wide in a rect {w} wide",
                        line.width()
                    );
                }
            }
        }
    }

    /// While a job is out there the list is gone, and what is on screen says
    /// what is happening and how to stop waiting.
    ///
    /// The list staying up is the bug this pins: a list a person can still walk
    /// while every key is being swallowed is a panel that looks broken.
    #[test]
    fn a_running_job_takes_the_list_off_the_screen() {
        let m = moment(Some(Panel {
            busy: Some(Busy {
                what: "正在装 tidy@official …".into(),
                job: "tidy@official".into(),
            }),
            ..Panel::new()
        }));
        // Wide enough that a row of the list would be legible if one were drawn:
        // at 60 columns a description is truncated anyway, so asserting on one
        // there would pass whether or not the list was on screen.
        let text = drawn(&m, 100, 20);
        assert!(text.contains("正在装 tidy@official"));
        assert!(text.contains("不等了"), "and how to stop waiting");
        assert!(
            !text.contains("lens"),
            "no list while nothing on it can be pressed:\n{text}"
        );
        assert!(
            !text.contains("看一眼改了什么"),
            "and none of what it says either:\n{text}"
        );
    }

    /// An installed row says so, and says where it went.
    #[test]
    fn an_installed_row_says_where_it_went() {
        let m = moment(Some(Panel::new()));
        let text = drawn(&m, 80, 20);
        assert!(text.contains("lens"));
        assert!(
            text.contains(&Scope::Project.short()),
            "the scope is on the row, because `/plugin uninstall` needs it and a \
             person reading the list is deciding whether to run it"
        );
    }

    /// The press that throws something away says what it will take with it.
    ///
    /// Removing a marketplace uninstalls everything installed from it. That is
    /// the one fact a person has to have *before* the second press, and the
    /// place to put it is the row being thrown away.
    #[test]
    fn arming_a_marketplace_says_what_goes_with_it() {
        let m = moment(Some(Panel {
            tab: Tab::Markets,
            cursor: 2, // +market, official, mine
            pending_delete: Some("market:mine".into()),
            ..Panel::new()
        }));
        let text = drawn(&m, 80, 20);
        assert!(
            text.contains("连同从它装的 2 个插件"),
            "what a person is about to lose, on the row they are about to lose it from: {text}"
        );
    }

    /// Arming a plugin says what the next press does, and nothing else does.
    #[test]
    fn arming_a_plugin_says_the_next_press_uninstalls() {
        let m = moment(Some(Panel {
            tab: Tab::Installed,
            pending_delete: Some("lens@official".into()),
            ..Panel::new()
        }));
        assert!(drawn(&m, 80, 20).contains("再按一次 ^d 卸载"));

        // Nothing armed, nothing said.
        let quiet = moment(Some(Panel {
            tab: Tab::Installed,
            ..Panel::new()
        }));
        assert!(!drawn(&quiet, 80, 20).contains("再按一次"));
    }

    /// An empty page says which empty it is.
    ///
    /// "No marketplaces yet" and "nothing matched what you typed" are different
    /// problems with different next steps, and a panel that said one sentence
    /// for both would send half the people to the wrong one.
    #[test]
    fn an_empty_page_says_which_empty_it_is() {
        let nothing = Moment {
            plugins: PluginsView::default(),
            plugins_panel: Some(Panel::new()),
            ..Moment::default()
        };
        assert!(drawn(&nothing, 60, 20).contains("一个市场都还没有"));

        let filtered = moment(Some(Panel {
            query: "zzz".into(),
            ..Panel::new()
        }));
        assert!(drawn(&filtered, 60, 20).contains("没有匹配的插件"));

        let none_installed = moment(Some(Panel {
            tab: Tab::Installed,
            query: "zzz".into(),
            ..Panel::new()
        }));
        assert!(drawn(&none_installed, 60, 20).contains("装上的里头没有匹配的"));
    }

    /// The header is where the click test says it is, and the row under a
    /// pointer is the row that was drawn there.
    #[test]
    fn a_click_finds_the_row_that_was_drawn() {
        let m = moment(Some(Panel::new()));
        let vp = Viewport::new(Rect::sized(80, 20), &m);
        let geom = geometry(&m, &vp);
        let header = geom.header_row().expect("the pages are drawn");
        assert_eq!(
            tab_at(&m, 0),
            None,
            "the panel's own name is not a page to click"
        );
        // Every listed row maps back to a row of the list, in order.
        let mapped: Vec<usize> = (0..20).filter_map(|row| geom.listed_at(row)).collect();
        assert_eq!(mapped, vec![0, 1]);
        assert!(
            geom.listed_at(header).is_none(),
            "the header is not one of the list's rows"
        );
    }

    /// The page a person is on is the page whose label is lit, and the installed
    /// one carries its count.
    #[test]
    fn the_pages_say_how_many_are_installed() {
        let m = moment(Some(Panel::new()));
        assert!(
            drawn(&m, 80, 20).contains("已装（1）"),
            "the count is on the page label, so it is readable without going there"
        );
    }
}
