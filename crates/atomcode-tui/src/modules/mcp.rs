//! MCP 面板:哪些服务器在、它们各自什么状态,以及钻进去之后能做什么。
//!
//! 住在这儿的是**画**。有哪些服务器、动作怎么落下去是数据与端口
//! ([`crate::mcp`])——这个模块不知道 host 命令长什么样,也不知道一趟要过几层
//! (`docs/adr/0022` §3)。
//!
//! 两级画的是同一块 [`crate::moment::Moment`] 上的两份不同数据:列表层按来源分组,
//! `Enter` 钻进去看一台的配置与诊断。处在哪一级是 [`crate::mcp::Panel`] 里的事实,
//! 所以 `render` 按级分派——画什么由数据说,不由这一层再记一遍。

use crate::caps::{Caps, Glyph};
use crate::frame::{Line, Span, Style};
use crate::i18n::{t, Msg};
use crate::mcp::{Action, Auth, Level, McpRow, McpState, McpView, Panel, Transport};
use crate::module::{Height, View};
use crate::modules::chrome::{
    self, box_edge, pad_to, panel_edge, search_line, LABEL_MAX, LABEL_MIN, LEAD,
};
use crate::moment::{Moment, Viewport};
use crate::theme::{self, Role};
use crate::width;

pub const ID: &str = "mcp";

/// 面板的名字,表头里画的就是这一个串。
const NAME: &str = "mcp";

/// 不管有多少服务器,列表最多占这么多行。和另外几块面板同一个数,因为占的是同一块地方。
const MOST: usize = 12;

#[derive(Default)]
pub struct State_;

pub struct Mcp;

impl View for Mcp {
    type State = State_;

    fn id() -> &'static str {
        ID
    }

    fn absorb(_state: &mut State_, _fact: &atomcode_harness::session::SessionEvent) {}

    fn render(_state: &State_, vp: &Viewport<'_>) -> Vec<Line> {
        let Some(panel) = vp.moment.mcp_panel.as_ref() else {
            return Vec::new();
        };
        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let view = &vp.moment.mcp;
        let caps = vp.moment.caps;
        layout(view, panel, vp.rect.h as usize)
            .into_iter()
            .map(|row| draw(view, panel, row, w, caps))
            .collect()
    }

    /// 按**没过滤**的清单数,理由同另外几块:面板的下沿是钉住的那条边,跟着过滤走
    /// 的高度会在人打字时把上沿顺着屏幕往下拽。
    fn height(_state: &State_, moment: &Moment, width: u16) -> Height {
        let Some(panel) = moment.mcp_panel.as_ref() else {
            return Height::Hug(0);
        };
        if width == 0 {
            return Height::Hug(0);
        }
        let mut open = panel.clone();
        open.query.clear();
        Height::Hug(
            layout(&moment.mcp, &open, usize::MAX)
                .len()
                .min(u16::MAX as usize) as u16,
        )
    }
}

/// 面板的一行,画出来之前。整张布局只有这一份:[`draw`]、[`geometry`] 和 `height`
/// 走的是同一个。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Row<'a> {
    Rule,
    /// 面板自己的表头:列表层是标题与数量,详情层是这一台的名字。
    Header,
    /// 列表层:一个来源的分组标题——它不是一行可以做点什么的东西,是一条分界。
    Group(String),
    /// 列表层:过滤后的第 `at` 个服务器。
    Server(usize, &'a McpRow),
    BoxTop,
    Search,
    BoxBottom,
    Scroll {
        above: usize,
        below: usize,
    },
    Nothing,
    /// 详情层:往返还没回来。
    Pending,
    /// 详情层的一行「标签 / 值」。
    Label(String, String),
    /// 详情层:第 `at` 个动作。
    Action(usize, Action),
    Note,
    Busy,
    Blank,
    Legend,
}

fn layout<'a>(view: &'a McpView, panel: &'a Panel, h: usize) -> Vec<Row<'a>> {
    let mut rows = vec![Row::Rule, Row::Header];
    if panel.busy.is_some() {
        rows.push(Row::Blank);
        rows.push(Row::Busy);
        rows.push(Row::Blank);
        rows.push(Row::Legend);
        return rows;
    }
    match panel.level {
        Level::List => {
            rows.push(Row::BoxTop);
            rows.push(Row::Search);
            rows.push(Row::BoxBottom);
            let listed = view.listed(panel);
            if listed.is_empty() {
                rows.push(Row::Nothing);
            } else {
                // 分组标题也占行,先把它们的数从上限里扣掉:窗口是给服务器算的,而屏上
                // 画出来的是服务器**加**标题。
                let frame = rows.len() + 2 + usize::from(panel.note.is_some());
                let cap = h
                    .saturating_sub(frame + distinct_sources(&listed))
                    .min(MOST);
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
                push_servers(&mut rows, &listed, from, to);
            }
        }
        Level::Detail => match view.detail() {
            Some(detail) => {
                let state = detail.state.about();
                let auth = auth_about(&detail.auth);
                let address = address_of(&detail.transport);
                // 来源那一栏摆的是**文件**:那是人要去看或者去改的东西。host 没给
                // 路径的时候退回来源的名字,而不是留一格空白。
                let source = match &detail.config_path {
                    Some(path) => path.clone(),
                    None => group_label(&detail.source),
                };
                // 工具数是整句,没有另外半边——就这么一条文案。
                let tools = t(Msg::McpLabelTools {
                    n: detail.tool_count,
                })
                .into_owned();
                rows.push(Row::Label(t(Msg::McpLabelState).into_owned(), state));
                rows.push(Row::Label(t(Msg::McpLabelAuth).into_owned(), auth));
                rows.push(Row::Label(t(Msg::McpLabelEndpoint).into_owned(), address));
                rows.push(Row::Label(t(Msg::McpLabelSource).into_owned(), source));
                rows.push(Row::Label(tools, String::new()));
                rows.push(Row::Blank);
                for (at, action) in panel.actions(detail).into_iter().enumerate() {
                    rows.push(Row::Action(at, action));
                }
            }
            // 刚按下回车,往返还没回来:说清楚在等什么,而不是给一张空表。
            None => rows.push(Row::Pending),
        },
    }
    if panel.note.is_some() {
        rows.push(Row::Note);
    }
    rows.push(Row::Blank);
    rows.push(Row::Legend);
    rows
}

/// `len` 行里让 `cursor` 留在视野里的那一段。夹住而不是居中,同另外几块。
fn window(len: usize, cursor: usize, room: usize) -> (usize, usize) {
    if len <= room {
        return (0, len);
    }
    let half = room / 2;
    let from = cursor.saturating_sub(half).min(len - room);
    (from, from + room)
}

/// 列出来的那些里有几种来源——分组标题要占的行数。清单按 (来源, 名字) 排过,
/// 所以同来源是连着的一段。
fn distinct_sources(listed: &[&McpRow]) -> usize {
    let mut sources: Vec<&str> = listed.iter().map(|row| row.source.as_str()).collect();
    sources.dedup();
    sources.len()
}

/// 从 `from` 到 `to` 的那一段,连同它经过的每一个来源标题。
fn push_servers<'a>(rows: &mut Vec<Row<'a>>, listed: &[&'a McpRow], from: usize, to: usize) {
    let mut last: Option<&str> = None;
    for (at, row) in listed.iter().enumerate().take(to).skip(from) {
        let row = *row;
        if last != Some(row.source.as_str()) {
            rows.push(Row::Group(group_label(&row.source)));
            last = Some(row.source.as_str());
        }
        rows.push(Row::Server(at, row));
    }
}

fn draw(view: &McpView, panel: &Panel, row: Row<'_>, w: usize, caps: Caps) -> Line {
    match row {
        Row::Rule => panel_edge(w, caps),
        Row::Header => {
            let labels = header_labels(view, panel);
            let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
            Line::from_spans(chrome::header_parts(NAME, &labels, usize::MAX).0).truncate(w)
        }
        Row::Group(label) => Line::styled(
            width::take_width(&format!("  {label}"), w),
            theme::fg(Role::Muted).bold(),
        ),
        Row::Server(at, server) => server_line(server, at == panel.cursor, w, caps),
        Row::BoxTop => box_edge(w, caps, true),
        Row::Search => search_line(&panel.query, Some(panel.query.len()), w, caps),
        Row::BoxBottom => box_edge(w, caps, false),
        Row::Blank => Line::empty(),
        Row::Scroll { above, below } => Line::styled(
            width::take_width(&format!("  ↑{above} ↓{below}"), w),
            theme::fg(Role::Muted),
        ),
        // 过滤到一台都不剩时说的也是这一句:表里没有第二种「空」的说法,而
        // 上面的搜索框里摆着人刚打的字,那才是这里为什么空着。
        Row::Nothing => Line::styled(
            width::take_width(&t(Msg::McpPanelEmpty), w),
            theme::fg(Role::Muted),
        ),
        Row::Pending => Line::styled(
            width::take_width(&t(Msg::McpDetailPending), w),
            theme::fg(Role::Muted),
        ),
        Row::Label(label, value) => label_line(&label, &value, w),
        Row::Action(at, action) => action_line(at, action, at == panel.cursor, w, caps),
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
                        .map(|busy| busy.what.clone())
                        .unwrap_or_default()
                ),
                w,
            ),
            theme::fg(Role::Warning),
        ),
        Row::Legend => Line::styled(
            width::take_width(&format!("  {}", legend(panel)), w),
            theme::fg(Role::Muted),
        ),
    }
}

/// 表头右边那几块:列表层是标题与此刻列出来的数量,详情层是这一台的名字。
fn header_labels(view: &McpView, panel: &Panel) -> Vec<String> {
    match panel.level {
        Level::List => vec![
            t(Msg::McpPanelTitle).into_owned(),
            t(Msg::McpPanelServers {
                n: view.listed(panel).len(),
            })
            .into_owned(),
        ],
        Level::Detail => vec![match view.detail() {
            Some(detail) => format!("{} MCP Server", detail.name),
            // 详情还没到,连这一台叫什么都不知道:说的是这是哪块面板。
            None => t(Msg::McpPanelTitle).into_owned(),
        }],
    }
}

/// 列表层的一行:光标、记号、名字,右边是状态词与工具数(设计 §5.1)。
fn server_line(row: &McpRow, here: bool, w: usize, caps: Caps) -> Line {
    let base = if here {
        theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg))
    } else {
        Style::new()
    };
    let pointer = if here {
        format!("{} ", caps.g(Glyph::Pointer))
    } else {
        "  ".to_string()
    };
    // 停用与断开的那几台暗一档:它们此刻不是模型能用的东西——同 `/toolbox` 里
    // 被关掉的那些。
    let name_style = match row.state {
        McpState::Disabled | McpState::Disconnected => base.under(theme::fg(Role::Muted)),
        _ => base,
    };
    let room = w.saturating_sub(LEAD + 2).clamp(LABEL_MIN, LABEL_MAX);
    let shown = width::take_width(&row.name, room);
    let pad = room.saturating_sub(width::str_width(&shown));
    let tools = t(Msg::McpLabelTools { n: row.tool_count }).into_owned();
    let about = format!("{}  {}", row.state.about(), tools);
    // 出了错的那一台,右边那几句话要看得见:设计 §6 说,失败一律说出来,不静默。
    let about_style = match row.state {
        McpState::Failed(_) => base.under(theme::fg(Role::Warning)),
        _ => base.under(theme::fg(Role::Muted)),
    };
    let spans = vec![
        Span::styled(pointer, base),
        Span::styled(format!("{} ", caps.g(row.state.glyph())), name_style),
        Span::styled(shown, name_style),
        Span::styled(" ".repeat(pad + 2), base),
        Span::styled(about, about_style),
    ];
    pad_to(Line::from_spans(spans), w, base)
}

/// 详情层的一行「标签 / 值」:标签占一列,值跟在后面(设计 §5.2)。
fn label_line(label: &str, value: &str, w: usize) -> Line {
    let room = w.saturating_sub(LEAD + 2).clamp(LABEL_MIN, LABEL_MAX);
    let shown = width::take_width(label, room);
    let pad = room.saturating_sub(width::str_width(&shown));
    let value_room = w.saturating_sub(LEAD + room + 2);
    let spans = vec![
        Span::styled("  ".to_string(), Style::new()),
        Span::styled(
            format!("{shown}{}", " ".repeat(pad + 2)),
            theme::fg(Role::Muted),
        ),
        Span::styled(
            width::take_width(value, value_room),
            theme::fg(Role::PanelFg),
        ),
    ];
    Line::from_spans(spans).truncate(w)
}

/// 详情层的一个动作:编号,回车就是它(设计 §5.2)。
fn action_line(at: usize, action: Action, here: bool, w: usize, caps: Caps) -> Line {
    let base = if here {
        theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg))
    } else {
        Style::new()
    };
    let pointer = if here {
        format!("{} ", caps.g(Glyph::Pointer))
    } else {
        "  ".to_string()
    };
    let text = format!("{}. {}", at + 1, action.about());
    let spans = vec![
        Span::styled(pointer, base),
        Span::styled(width::take_width(&text, w), base),
    ];
    pad_to(Line::from_spans(spans), w, base)
}

/// 底下那行按键图例:两级各一句,有活在跑的时候是第三句(设计 §5.3、§5.4)。
fn legend(panel: &Panel) -> String {
    if panel.busy.is_some() {
        return t(Msg::McpLegendBusy).into_owned();
    }
    match panel.level {
        Level::List => t(Msg::McpLegendList).into_owned(),
        Level::Detail => t(Msg::McpLegendDetail).into_owned(),
    }
}

/// 来源那三个串是 host 的说法(`"global"` / `"project"` / `"driver"`),画的时候译成
/// 人的话;**认不出的原样画**,不吞——同 `non_exhaustive` 那套思路:不知道的照实说。
fn group_label(source: &str) -> String {
    match source {
        "global" => t(Msg::McpGroupGlobal).into_owned(),
        "project" => t(Msg::McpGroupProject).into_owned(),
        "driver" => t(Msg::McpGroupDriver).into_owned(),
        other => other.to_string(),
    }
}

/// 认证那一栏:不需要、已认证、未认证。
fn auth_about(auth: &Auth) -> String {
    match auth {
        Auth::None => t(Msg::McpAuthNone),
        Auth::OAuth { authenticated } => {
            if *authenticated {
                t(Msg::McpAuthAuthenticated)
            } else {
                t(Msg::McpAuthNotAuthenticated)
            }
        }
    }
    .into_owned()
}

/// 地址那一栏随传输方式变:http 是 URL,stdio 是命令带参数(设计 §5.2)。
fn address_of(transport: &Transport) -> String {
    match transport {
        Transport::Http { url } => url.clone(),
        // 照着这一行能把它打出来,所以参数一个不少。
        Transport::Stdio { command, args } => {
            let mut out = command.clone();
            for arg in args {
                out.push(' ');
                out.push_str(arg);
            }
            out
        }
    }
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
    let Some(panel) = moment.mcp_panel.as_ref() else {
        return Geometry {
            rows: Vec::new(),
            header: None,
        };
    };
    let rows = layout(&moment.mcp, panel, vp.rect.h as usize);
    Geometry {
        header: rows.iter().position(|row| matches!(row, Row::Header)),
        rows: rows
            .into_iter()
            .map(|row| match row {
                Row::Server(at, _) => Some(at),
                _ => None,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rect;
    use crate::mcp::McpDetail;

    /// 一台服务器,只有这几个字段是这一次要的。
    fn row(name: &str, source: &str, state: McpState, tools: usize) -> McpRow {
        McpRow {
            name: name.to_string(),
            state,
            source: source.to_string(),
            tool_count: tools,
            config_path: None,
        }
    }

    /// 一台服务器的详情页,只有名字、状态与认证是这一次要的。
    fn detail_of(name: &str, state: McpState, auth: Auth) -> McpDetail {
        McpDetail {
            name: name.to_string(),
            state,
            source: "project".to_string(),
            transport: Transport::Http {
                url: "https://mcp.figma.com/mcp".into(),
            },
            auth,
            tool_count: 0,
            config_path: Some("/tmp/example/.mcp.json".to_string()),
        }
    }

    /// 列表层画出来的那些行。走的是 `render` 本身,不另抄一条画法。
    fn render_list(view: &McpView, panel: &Panel, w: usize) -> Vec<Line> {
        drawn(view, panel, w)
    }

    /// 详情层同理:处在哪一级是 `panel` 里的事实,所以两个助手调的是同一个。
    fn render_detail(view: &McpView, panel: &Panel, w: usize) -> Vec<Line> {
        drawn(view, panel, w)
    }

    fn drawn(view: &McpView, panel: &Panel, w: usize) -> Vec<Line> {
        let moment = Moment {
            mcp: view.clone(),
            mcp_panel: Some(panel.clone()),
            ..Moment::default()
        };
        let vp = Viewport::new(Rect::sized(w as u16, 24), &moment);
        Mcp::render(&State_, &vp)
    }

    fn line_text(line: &Line) -> String {
        line.plain()
    }

    #[test]
    fn the_list_groups_by_source_and_the_detail_numbers_its_actions() {
        let view = McpView::new(vec![
            row("context7", "global", McpState::Connected, 8),
            row("figma", "project", McpState::NeedsAuthentication, 0),
        ]);

        let lines = render_list(&view, &Panel::new(), 60);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            text.iter().any(|l| l.contains("全局")),
            "来源分组: {text:?}"
        );
        assert!(
            text.iter().any(|l| l.contains("项目")),
            "来源分组: {text:?}"
        );

        let detail = detail_of(
            "figma",
            McpState::NeedsAuthentication,
            Auth::OAuth {
                authenticated: false,
            },
        );
        let panel = Panel {
            level: Level::Detail,
            ..Panel::new()
        };
        let lines = render_detail(&view.with_detail(detail), &panel, 60);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            text.iter().any(|l| l.contains("1.")),
            "动作带编号: {text:?}"
        );
        assert!(text.iter().any(|l| l.contains("认证")), "动作名: {text:?}");
    }

    /// 每一行都不能比它拿到的宽度更宽:宽一格就是压在旁边的东西上。名字里带中日韩
    /// 宽字符、值里带长路径,是最容易算错的两处(`crate::width`,不是 `chars().count()`)。
    #[test]
    fn no_row_ever_draws_wider_than_its_rect() {
        let view = McpView::new(vec![
            row("上下文七号服务器", "global", McpState::Connected, 128),
            row("figma", "project", McpState::NeedsAuthentication, 0),
            row("gone", "driver", McpState::Disabled, 3),
        ])
        .with_detail(detail_of(
            "figma",
            McpState::NeedsAuthentication,
            Auth::OAuth {
                authenticated: false,
            },
        ));
        let detail = Panel {
            level: Level::Detail,
            ..Panel::new()
        };
        for w in [4usize, 12, 40, 80, 200] {
            let list = render_list(&view, &Panel::new(), w);
            let opened = render_detail(&view, &detail, w);
            for line in list.iter().chain(opened.iter()) {
                assert!(line.width() <= w, "宽度 {w}: {:?}", line.plain());
            }
        }
    }
}
