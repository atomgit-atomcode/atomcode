//! 底部那张单子([`crate::sheet`]):`/agents`、`/cd`、`/diff`、`/view` 共用的一块。
//!
//! 和 `/resume`、`/provider`、`/bg` 从同一个地方升起来、同一副骨架:一条规则线、
//! 表头,挑一行的是一个边框搜索框加一张单子,读的那页是一栏行号加正文,底下一行
//! 按键提示。住在这儿的是**画**;单子上有什么、按键做什么在 [`crate::sheet`]。

use crate::caps::{Caps, Glyph};
use crate::frame::{Line, Span, Style};
use crate::i18n::{t, Msg};
use crate::module::{Height, View};
use crate::modules::chrome::{self, box_edge, pad_to, panel_edge, search_line};
use crate::moment::{Moment, Viewport};
use crate::sheet::{doc_width, read_room, Doc, List, Mark, Page, Piece, Read, Sheet, Tone};
use crate::theme::{self, Role};
use crate::width;

pub const ID: &str = "sheet";
/// 单子最多画这么多行,和别的面板占同一块地方。
const MOST: usize = 12;
/// 选中那行的预览最多几行。
///
/// 预览块的高度按**整张单子里最长的那份**定、与选中哪行无关 —— 面板贴底往上长,
/// 预览多一行面板上沿就抬一行,上下选的时候整张列表就在屏幕上跳(`/resume` 的
/// `PREVIEW_ROWS` 是同一个理由)。
const PREVIEW_MOST: usize = 6;

#[derive(Default)]
pub struct State;

pub struct SheetView;

impl View for SheetView {
    type State = State;

    fn id() -> &'static str {
        ID
    }

    fn absorb(_state: &mut State, _fact: &atomcode_harness::session::SessionEvent) {}

    fn render(_state: &State, vp: &Viewport<'_>) -> Vec<Line> {
        let Some(sheet) = vp.moment.sheet.as_ref() else {
            return Vec::new();
        };
        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let caps = vp.moment.caps;
        let h = vp.rect.h as usize;
        if let Page::Doc(doc) = &sheet.page {
            return doc_lines(sheet, doc, h, w, caps);
        }
        layout(sheet, h)
            .into_iter()
            .map(|row| draw(sheet, row, w, caps, read_room(h)))
            .collect()
    }

    /// 按能画下的行数要,不按屏幕给了多少 —— 理由同别的面板(`crate::modules::rewind`)。
    fn height(_state: &State, moment: &Moment, width: u16) -> Height {
        let Some(sheet) = moment.sheet.as_ref() else {
            return Height::Hug(0);
        };
        if width == 0 {
            return Height::Hug(0);
        }
        if let Page::Doc(doc) = &sheet.page {
            let rows = doc_lines(sheet, doc, usize::MAX, width as usize, moment.caps).len();
            return Height::Hug(rows.min(u16::MAX as usize) as u16);
        }
        Height::Hug(layout(sheet, usize::MAX).len().min(u16::MAX as usize) as u16)
    }
}

/// 面板的一行,画出来之前。[`draw`]、[`geometry`]、`height` 走的是同一份。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Rule,
    Header,
    Summary,
    BoxTop,
    Search,
    BoxBottom,
    Blank,
    /// 单子上的一行:`at` 是它在筛出行里的位置(光标、命中测试按这个走),`row`
    /// 是它在整张单子里的下标。两个都在排版时定下来 —— 画每一行时再筛一遍,
    /// `/view` 那张几万个文件的单子就要每一帧筛十几遍。
    Item {
        at: usize,
        row: usize,
    },
    Nothing,
    Scroll {
        above: usize,
        below: usize,
    },
    /// 读的那页的第几行(原始下标)。
    Line(usize),
    /// 选中那行预览的第几行([`crate::sheet::Row::preview`])。
    Preview(usize),
    Legend,
}

fn layout(sheet: &Sheet, h: usize) -> Vec<Row> {
    let mut rows = vec![Row::Rule, Row::Header];
    match &sheet.page {
        Page::List(list) => {
            if !list.summary.is_empty() {
                rows.push(Row::Summary);
            }
            rows.extend([Row::BoxTop, Row::Search, Row::BoxBottom]);
            let listed = list.listed();
            let preview = preview_rows(list);
            // 预览块(一行空行加它自己)也从同一份高度里出。
            let preview_room = if preview > 0 { preview + 1 } else { 0 };
            if listed.is_empty() {
                rows.push(Row::Nothing);
            } else {
                // chrome 之外、底下空行加提示之后还剩多少行;`h == usize::MAX` 是
                // `height` 在问要多少,答案是整张单子,仍夹在上限里。
                let cap = h.saturating_sub(rows.len() + 2 + preview_room).min(MOST);
                let (from, to) = if listed.len() > cap {
                    // 那条「还有多少没画」自己也占一行,从同一份预算里扣。
                    let room = cap.saturating_sub(1).max(1);
                    let (from, to) = window(listed.len(), list.cursor, room);
                    rows.push(Row::Scroll {
                        above: from,
                        below: listed.len() - to,
                    });
                    (from, to)
                } else {
                    (0, listed.len())
                };
                rows.extend((from..to).map(|at| Row::Item {
                    at,
                    row: listed[at],
                }));
            }
            // 列表下面、图例上面:它说的是「这一行里是什么」,贴着列表;它是读的
            // 不是挑的,所以不进列表本身。筛空了也照样占着,面板不因此变矮。
            if preview > 0 {
                rows.push(Row::Blank);
                rows.extend((0..preview).map(Row::Preview));
            }
        }
        // Drawn whole by `doc_lines`: its rows depend on the width, which this
        // layout does not know. Only the hit test comes here, and a document has
        // no rows to pick.
        Page::Doc(_) => {}
        Page::Read(read) => {
            rows.push(Row::Blank);
            if read.lines.is_empty() {
                rows.push(Row::Nothing);
            } else {
                let (from, to) = read_window(read, read_room(h));
                rows.extend((from..to).map(Row::Line));
            }
        }
    }
    rows.push(Row::Blank);
    rows.push(Row::Legend);
    rows
}

/// 预览块占几行:单子里最长的那份,夹在 [`PREVIEW_MOST`] 里。没有一行带预览就是 0,
/// 面板和从前一样。
fn preview_rows(list: &List) -> usize {
    list.rows
        .iter()
        .map(|row| row.preview.len())
        .max()
        .unwrap_or(0)
        .min(PREVIEW_MOST)
}

/// 一份长为 `len` 的单子里,让 `cursor` 留在视野中的那 `room` 行。
fn window(len: usize, cursor: usize, room: usize) -> (usize, usize) {
    if len <= room {
        return (0, len);
    }
    let half = room / 2;
    let from = cursor.saturating_sub(half).min(len - room);
    (from, from + room)
}

/// 读的那页此刻画哪几行:从 `top` 起,但最后一屏总是满的。
fn read_window(read: &Read, room: usize) -> (usize, usize) {
    let len = read.lines.len();
    let from = read.top.min(len.saturating_sub(room));
    (from, (from + room).min(len))
}

/// `room`:读的那页此刻画出来的正文行数,表头「看到哪儿」按它说。
fn draw(sheet: &Sheet, row: Row, w: usize, caps: Caps, room: usize) -> Line {
    let muted = theme::fg(Role::Muted);
    match row {
        Row::Rule => panel_edge(w, caps),
        Row::Header => header(sheet, w, room),
        Row::Summary => match &sheet.page {
            Page::List(list) => pieces_line("  ", &list.summary, w),
            Page::Read(_) | Page::Doc(_) => Line::empty(),
        },
        Row::BoxTop => box_edge(w, caps, true),
        Row::Search => match &sheet.page {
            Page::List(list) => search_line(&list.query, Some(list.query.len()), w, caps),
            Page::Read(_) | Page::Doc(_) => Line::empty(),
        },
        Row::BoxBottom => box_edge(w, caps, false),
        Row::Blank => Line::empty(),
        Row::Nothing => {
            let said = match &sheet.page {
                Page::List(list) if list.rows.is_empty() && !list.empty.is_empty() => {
                    list.empty.clone()
                }
                Page::List(_) => t(Msg::OverlayNoMatch).trim().to_string(),
                Page::Read(read) if !read.empty.is_empty() => read.empty.clone(),
                Page::Read(_) | Page::Doc(_) => t(Msg::OverlayEmptyFile).trim().to_string(),
            };
            Line::styled(width::take_width(&format!("  {said}"), w), muted)
        }
        Row::Scroll { above, below } => {
            Line::styled(width::take_width(&format!("  ↑{above} ↓{below}"), w), muted)
        }
        Row::Item { at, row } => match &sheet.page {
            Page::List(list) => item_line(list, at, row, w, caps),
            Page::Read(_) | Page::Doc(_) => Line::empty(),
        },
        Row::Line(at) => match &sheet.page {
            Page::Read(read) => read_line(read, at, w, caps),
            Page::List(_) | Page::Doc(_) => Line::empty(),
        },
        Row::Preview(at) => match &sheet.page {
            Page::List(list) => {
                let said = list
                    .selected()
                    .and_then(|row| row.preview.get(at))
                    .map(|line| crate::text::for_screen(line).into_owned())
                    .unwrap_or_default();
                Line::styled(format!("  {said}"), muted).truncate(w)
            }
            Page::Read(_) | Page::Doc(_) => Line::empty(),
        },
        Row::Legend => {
            let said = match &sheet.page {
                Page::List(list) => t(Msg::SheetListLegend {
                    typed: list.typed.is_some(),
                }),
                Page::Read(_) => t(Msg::SheetReadLegend {
                    back: sheet.back.is_some(),
                }),
                Page::Doc(doc) => t(Msg::SheetDocLegend {
                    back: sheet.back.is_some(),
                    tabs: doc.tabs.len() > 1,
                    links: doc.has_links(),
                }),
            };
            Line::styled(format!("  {said}"), muted).truncate(w)
        }
    }
}

/// 表头:命令的名字(品牌色),后面是这一页自己的话。读的那页再加上看到了哪儿:
/// `起-止/共几行`。
fn header(sheet: &Sheet, w: usize, room: usize) -> Line {
    let (mut spans, _) = chrome::header_parts(sheet.page.id(), &[], usize::MAX);
    match &sheet.page {
        Page::List(list) => spans.push(Span::styled(list.title.clone(), theme::fg(Role::Muted))),
        Page::Read(read) => {
            spans.extend(read.title.iter().map(piece_span));
            let len = read.lines.len();
            if len > 0 {
                let (from, to) = read_window(read, room);
                spans.push(Span::styled(
                    format!("  {}-{}/{len}", from + 1, to),
                    theme::fg(Role::Muted),
                ));
            }
        }
        Page::Doc(doc) => return Line::from_spans(doc_header(doc).0).truncate(w),
    }
    Line::from_spans(spans).truncate(w)
}

/// 文档页([`Doc`]):规则线、表头、空行、正文(按面板宽度渲染的 markdown)、空行、
/// 提示。和读的那页同一副骨架,只是正文是渲染过的,没有行号。
fn doc_lines(sheet: &Sheet, doc: &Doc, h: usize, w: usize, caps: Caps) -> Vec<Line> {
    let muted = theme::fg(Role::Muted);
    let room = read_room(h);
    let body = doc.lines(doc_width(w));
    let from = doc.top.min(body.len().saturating_sub(room));
    let to = (from + room).min(body.len());
    let (mut header, _) = doc_header(doc);
    if body.len() > room {
        header.push(Span::styled(
            format!("  {}-{}/{}", from + 1, to, body.len()),
            muted,
        ));
    }
    let mut out = vec![
        panel_edge(w, caps),
        Line::from_spans(header).truncate(w),
        Line::empty(),
    ];
    if body.is_empty() {
        out.push(Line::styled(
            width::take_width(&format!("  {}", t(Msg::OverlayEmptyFile).trim()), w),
            muted,
        ));
    }
    for line in &body[from..to] {
        let mut spans = vec![Span::styled("  ".to_string(), Style::new())];
        spans.extend(line.spans.iter().cloned());
        out.push(Line::from_spans(spans).truncate(w));
    }
    out.push(Line::empty());
    out.push(
        Line::styled(
            format!(
                "  {}",
                t(Msg::SheetDocLegend {
                    back: sheet.back.is_some(),
                    tabs: doc.tabs.len() > 1,
                    links: doc.has_links(),
                })
            ),
            muted,
        )
        .truncate(w),
    );
    out
}

/// 文档页的表头:命令名,版本与日期,然后是页签;以及每个页签占哪几列(点页签用)。
/// 只有一页时不画页签 —— 一个页签没有可切换的。
fn doc_header(doc: &Doc) -> (Vec<Span>, Vec<(usize, usize, usize)>) {
    let labels: Vec<&str> = if doc.tabs.len() > 1 {
        doc.tabs.iter().map(|tab| tab.name.as_str()).collect()
    } else {
        Vec::new()
    };
    let (mut spans, ranges) = chrome::header_parts(doc.id, &labels, doc.tab);
    // `header_parts` 是「两格、命令名、三格、页签……」:版本号插在命令名之后、页签之前,
    // 页签的列跟着右移同样的宽度 —— 画的和点的用同一份。
    let mut title: Vec<Span> = doc.title.iter().map(piece_span).collect();
    if !labels.is_empty() {
        title.push(Span::styled("    ".to_string(), Style::new()));
    }
    let shift: usize = title.iter().map(|span| width::str_width(&span.text)).sum();
    let at = spans.len().min(3);
    spans.splice(at..at, title);
    let ranges = ranges
        .into_iter()
        .map(|(tab, from, to)| (tab, from + shift, to + shift))
        .collect();
    (spans, ranges)
}

/// 文档页第 `row` 行、第 `col` 列落在哪个页签上。表头是第二行(规则线之下)。
pub fn doc_tab_at(doc: &Doc, row: usize, col: usize) -> Option<usize> {
    if row != 1 {
        return None;
    }
    doc_header(doc)
        .1
        .into_iter()
        .find(|&(_, from, to)| col >= from && col < to)
        .map(|(tab, _, _)| tab)
}

fn tone_style(tone: Tone) -> Style {
    match tone {
        Tone::Plain => theme::fg(Role::PanelFg),
        Tone::Muted => theme::fg(Role::Muted),
        Tone::Added => theme::fg(Role::Success),
        Tone::Removed => theme::fg(Role::Error),
        Tone::Warning => theme::fg(Role::Warning),
    }
}

fn piece_span(piece: &Piece) -> Span {
    Span::styled(piece.text.clone(), tone_style(piece.tone))
}

fn pieces_line(lead: &str, pieces: &[Piece], w: usize) -> Line {
    let mut spans = vec![Span::styled(lead.to_string(), Style::new())];
    spans.extend(pieces.iter().map(piece_span));
    Line::from_spans(spans).truncate(w)
}

/// 单子上一行:指针、记号、标签、灰字,靠右是那几个数。光标所在的行铺一层选中底色。
fn item_line(list: &List, at: usize, index: usize, w: usize, caps: Caps) -> Line {
    let Some(row) = list.rows.get(index) else {
        return Line::empty();
    };
    let here = at == list.cursor;
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
    let mut left = vec![Span::styled(pointer, base)];
    if let Some(on) = row.on {
        let (glyph, role) = match on {
            true => (Glyph::ToolMark, Role::Success),
            false => (Glyph::Hollow, Role::Muted),
        };
        left.push(Span::styled(
            format!("{} ", caps.g(glyph)),
            base.under(theme::fg(role)),
        ));
    }
    if let Some(tag) = &row.tag {
        left.push(Span::styled(
            format!("{} ", tag.text),
            base.under(tone_style(tag.tone)),
        ));
    }
    left.push(Span::styled(
        row.label.clone(),
        base.under(theme::fg(Role::PanelFg)),
    ));
    if !row.about.is_empty() {
        left.push(Span::styled(
            format!("  {}", row.about),
            base.under(theme::fg(Role::Muted)),
        ));
    }
    // 靠右的数:先量它要多宽,左边那段只能用剩下的 —— 数永远画得全,被截的是灰字。
    let right: Vec<Span> = row
        .figures
        .iter()
        .map(|piece| Span::styled(piece.text.clone(), base.under(tone_style(piece.tone))))
        .collect();
    let right_w: usize = row.figures.iter().map(|p| width::str_width(&p.text)).sum();
    if right_w == 0 {
        return pad_to(Line::from_spans(left).truncate(w), w, base);
    }
    let room = w.saturating_sub(right_w + 2);
    let left = Line::from_spans(left).truncate(room);
    let gap = w.saturating_sub(left.width() + right_w + 1);
    let mut spans = left.spans;
    spans.push(Span::styled(" ".repeat(gap), base));
    spans.extend(right);
    pad_to(Line::from_spans(spans).truncate(w), w, base)
}

/// 读的那页一行:一栏灰色行号,diff 再多一栏 `+`/`-`,然后是字。
fn read_line(read: &Read, at: usize, w: usize, caps: Caps) -> Line {
    let Some(line) = read.lines.get(at) else {
        return Line::empty();
    };
    let muted = theme::fg(Role::Muted);
    let digits = read
        .lines
        .iter()
        .filter_map(|l| l.number)
        .max()
        .unwrap_or(0)
        .to_string()
        .len()
        .max(2);
    // Two cells in, where everything else in a panel starts — the header, the
    // list's rows, the legend.
    let number = match line.number {
        Some(n) => format!("  {n:>digits$}"),
        None => format!("  {}", " ".repeat(digits)),
    };
    let text = crate::text::for_screen(&line.text).into_owned();
    let spans = match line.mark {
        Mark::Text => vec![
            Span::styled(format!("{number}  "), muted),
            Span::styled(text, Style::new()),
        ],
        Mark::Added => vec![
            Span::styled(format!("{number} "), muted),
            Span::styled(format!("+ {text}"), theme::fg(Role::Success)),
        ],
        Mark::Removed => vec![
            Span::styled(format!("{number} "), muted),
            Span::styled(format!("- {text}"), theme::fg(Role::Error)),
        ],
        Mark::Same => vec![
            Span::styled(format!("{number} "), muted),
            Span::styled(format!("  {text}"), Style::new()),
        ],
        Mark::Gap => vec![
            Span::styled(format!("{number} "), muted),
            Span::styled(caps.g(Glyph::Pending).to_string(), muted),
        ],
        Mark::Note => vec![
            Span::styled(format!("{number} "), muted),
            Span::styled(text, muted),
        ],
    };
    Line::from_spans(spans).truncate(w)
}

/// 命中测试要的:每一屏行对应单子上筛出行的第几行。
pub struct Geometry {
    rows: Vec<Option<usize>>,
}

impl Geometry {
    pub fn listed_at(&self, row: usize) -> Option<usize> {
        self.rows.get(row).copied().flatten()
    }
}

pub fn geometry(moment: &Moment, vp: &Viewport<'_>) -> Geometry {
    let Some(sheet) = moment.sheet.as_ref() else {
        return Geometry { rows: Vec::new() };
    };
    Geometry {
        rows: layout(sheet, vp.rect.h as usize)
            .into_iter()
            .map(|row| match row {
                Row::Item { at, .. } => Some(at),
                _ => None,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sheet::{List, Row as Item};

    fn drawn(sheet: &Sheet, w: usize) -> Vec<String> {
        layout(sheet, usize::MAX)
            .into_iter()
            .map(|row| draw(sheet, row, w, Caps::default(), read_room(usize::MAX)).plain())
            .collect()
    }

    fn versions(cursor: usize) -> Sheet {
        let mut list = List::new(
            "changelog",
            "pick one",
            vec![
                Item::new("/changelog v5.2.1", "v5.2.1")
                    .preview(vec!["• 网页端按服务商管理模型".into(), "• 推理强度".into()]),
                Item::new("/changelog v5.2.0", "v5.2.0").preview(vec!["• 只有一点".into()]),
                Item::new("/changelog v5.1.0", "v5.1.0"),
            ],
        );
        list.cursor = cursor;
        Sheet::list(list)
    }

    /// 光标停在哪一行,列表下面就是那一行的预览 —— `/resume` 选中会话时底下那几句
    /// 是同一件事。
    #[test]
    fn the_selected_row_shows_its_preview_under_the_list() {
        let first = drawn(&versions(0), 60).join("\n");
        assert!(first.contains("网页端按服务商管理模型"), "{first}");
        assert!(
            !first.contains("只有一点"),
            "only the selected row's: {first}"
        );
        let second = drawn(&versions(1), 60).join("\n");
        assert!(second.contains("只有一点"), "{second}");
        assert!(!second.contains("网页端"), "{second}");
    }

    /// 预览长短不一、甚至这一行没有预览,面板一样高:它贴底往上长,高度一变整张
    /// 列表就跳。
    #[test]
    fn the_sheet_is_as_tall_whichever_row_is_selected() {
        let heights: Vec<usize> = (0..3).map(|at| drawn(&versions(at), 60).len()).collect();
        assert!(heights.iter().all(|h| *h == heights[0]), "{heights:?}");
    }

    /// 一行都不带预览的单子,和从前一模一样。
    #[test]
    fn a_list_without_previews_is_what_it_was() {
        let plain = Sheet::list(List::new(
            "cd",
            "",
            vec![Item::new("/cd a", "a"), Item::new("/cd b", "b")],
        ));
        let rows = layout(&plain, usize::MAX);
        assert!(!rows.iter().any(|row| matches!(row, Row::Preview(_))));
    }

    fn release() -> Doc {
        Doc::new(
            "changelog",
            vec![Piece::new("v5.2.2", Tone::Plain)],
            vec![
                crate::sheet::DocTab::new(
                    "概览",
                    "这一版的核心是**架构**。\n\n- **全新架构**:分层",
                ),
                crate::sheet::DocTab::new(
                    "Issues (1)",
                    "- [#1182 Windows 终端二维码显示异常](https://atomgit.com/x/issues/1182)",
                ),
            ],
        )
    }

    fn doc_text(sheet: &Sheet, w: usize) -> Vec<String> {
        let Page::Doc(doc) = &sheet.page else {
            panic!("a document");
        };
        doc_lines(sheet, doc, usize::MAX, w, Caps::default())
            .into_iter()
            .map(|line| line.plain())
            .collect()
    }

    /// 文档页:表头是命令名、版本、页签;正文按 markdown 画 —— 加粗没有星号,
    /// 列表是圆点。
    #[test]
    fn a_document_draws_its_markdown_under_its_tabs() {
        let sheet = Sheet::doc(release());
        let text = doc_text(&sheet, 80);
        assert!(text[1].contains("changelog"), "{text:?}");
        assert!(
            text[1].contains("v5.2.2")
                && text[1].contains("概览")
                && text[1].contains("Issues (1)")
        );
        let body = text.join("\n");
        assert!(body.contains("这一版的核心是架构"), "{body}");
        assert!(!body.contains("**"), "drawn, not source: {body}");
    }

    /// Issues 那一页:看到的是编号和标题,地址不露出来 —— 它是标题上的链接。
    #[test]
    fn an_issue_shows_its_title_and_links_it_without_printing_the_url() {
        let mut doc = release();
        doc.show(1);
        let sheet = Sheet::doc(doc.clone());
        let lines = doc_lines(&sheet, &doc, usize::MAX, 80, Caps::default());
        let text: String = lines
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("#1182 Windows 终端二维码显示异常"), "{text}");
        assert!(!text.contains("https://"), "{text}");
        let linked = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|span| span.text.contains("#1182"))
            .and_then(|span| span.link.clone());
        assert_eq!(linked.as_deref(), Some("https://atomgit.com/x/issues/1182"));
    }

    /// 点表头上的页签就换到那一页 —— 版本号挤在前面,点的列也跟着挪。
    #[test]
    fn a_click_on_a_tab_finds_the_tab_drawn_there() {
        let doc = release();
        let header = doc_text(&Sheet::doc(doc.clone()), 80)[1].clone();
        let col = |label: &str| width::str_width(header.split(label).next().unwrap_or_default());
        assert_eq!(doc_tab_at(&doc, 1, col("概览")), Some(0));
        assert_eq!(doc_tab_at(&doc, 1, col("Issues")), Some(1));
        assert_eq!(
            doc_tab_at(&doc, 1, col("v5.2.2")),
            None,
            "the title is no tab"
        );
        assert_eq!(
            doc_tab_at(&doc, 2, col("Issues")),
            None,
            "only the header row"
        );
    }

    /// 一个版本没有 Issues 时只有一页,不画页签,提示里也不说 tab。
    #[test]
    fn a_document_with_one_page_draws_no_tabs() {
        let doc = Doc::new(
            "changelog",
            vec![Piece::new("v5.0.0", Tone::Plain)],
            vec![crate::sheet::DocTab::new("概览", "- **一点**")],
        );
        let text = doc_text(&Sheet::doc(doc), 80);
        assert!(!text[1].contains("概览"), "{text:?}");
        assert!(!text.last().unwrap().contains("tab"), "{text:?}");
    }

    /// 面板一样高:正文比一屏长时,往下翻不改变高度。
    #[test]
    fn a_long_document_is_as_tall_wherever_it_is_scrolled() {
        let long: String = (1..=80).map(|n| format!("- point {n}\n")).collect();
        let mut doc = Doc::new(
            "changelog",
            Vec::new(),
            vec![crate::sheet::DocTab::new("a", long)],
        );
        let top = doc_lines(&Sheet::doc(doc.clone()), &doc, 30, 80, Caps::default()).len();
        doc.top = 40;
        let scrolled = doc_lines(&Sheet::doc(doc.clone()), &doc, 30, 80, Caps::default());
        assert_eq!(scrolled.len(), top);
        assert!(
            scrolled[1].plain().contains("41-"),
            "where it is: {:?}",
            scrolled[1].plain()
        );
    }

    /// 单子:表头说是哪个命令的、这一张是什么;选中那行有指针;靠右的数画全,
    /// 窄了先截灰字。
    #[test]
    fn a_list_draws_like_the_other_panels() {
        let sheet = Sheet::list(
            List::new(
                "diff",
                "enter opens one",
                vec![
                    Item::new("/diff a.rs", "a.rs")
                        .about("staged")
                        .tag(Piece::new("M", Tone::Muted))
                        .figures(vec![
                            Piece::new("+12", Tone::Added),
                            Piece::new(" -3", Tone::Removed),
                        ]),
                    Item::new("/diff b.rs", "b.rs"),
                ],
            )
            .summary(vec![Piece::new("2 files", Tone::Muted)]),
        );
        let lines = drawn(&sheet, 40);
        assert!(
            lines[1].contains("diff") && lines[1].contains("enter opens one"),
            "{lines:?}"
        );
        assert!(lines[2].contains("2 files"), "{lines:?}");
        let first = lines.iter().find(|l| l.contains("a.rs")).expect("the row");
        assert!(first.contains("M a.rs"), "{first:?}");
        assert!(first.trim_end().ends_with("+12 -3"), "{first:?}");
        let narrow = drawn(&sheet, 16);
        let first = narrow
            .iter()
            .find(|l| l.contains("+12 -3"))
            .expect("figures kept");
        assert!(!first.contains("staged"), "{first:?}");
        let wide = drawn(&sheet, 200);
        assert!(wide
            .last()
            .unwrap()
            .contains(&*t(Msg::SheetListLegend { typed: false })));
    }

    /// 屏幕矮的时候,表头说的是**实际画出来**的那几行,不是按一整屏算的。
    #[test]
    fn a_short_reader_says_what_it_actually_shows() {
        let text: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        let sheet = Sheet::read(Read::file("view", Vec::new(), &text));
        let h = 20;
        let room = read_room(h);
        let lines: Vec<String> = layout(&sheet, h)
            .into_iter()
            .map(|row| draw(&sheet, row, 60, Caps::default(), room).plain())
            .collect();
        assert_eq!(lines.len(), h, "{lines:?}");
        assert!(lines[1].contains(&format!("1-{room}/100")), "{lines:?}");
    }

    /// 读的那页:表头有看到哪儿,行号一栏右对齐;diff 的行带 `+`/`-`,两段之间一个
    /// 省略号;esc 回不回得去,提示说的不一样。
    #[test]
    fn a_reader_numbers_its_lines_and_says_where_it_is() {
        let text: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        let sheet = Sheet::read(Read::file(
            "view",
            vec![Piece::new("a.rs", Tone::Plain)],
            &text,
        ));
        let lines = drawn(&sheet, 60);
        assert!(
            lines[1].contains(&format!("1-{}/30", crate::sheet::READ_ROWS)),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("   1  line 1")),
            "{lines:?}"
        );
        assert!(lines
            .last()
            .unwrap()
            .contains(&*t(Msg::SheetReadLegend { back: false })));

        let diff = "@@ -1,2 +1,2 @@\n a\n-b\n+c\n@@ -9,1 +9,1 @@\n z\n";
        let sheet = Sheet {
            page: Page::Read(Read::diff("diff", "x.rs", diff)),
            back: Some(Box::new(Page::List(List::new("diff", "t", Vec::new())))),
            pending: None,
        };
        let lines = drawn(&sheet, 60);
        assert!(lines[1].contains("x.rs  +1 -1"), "{lines:?}");
        assert!(lines.iter().any(|l| l == "   2 - b"), "{lines:?}");
        assert!(lines.iter().any(|l| l == "   2 + c"), "{lines:?}");
        assert!(lines
            .iter()
            .any(|l| l.contains(Caps::default().g(Glyph::Pending))));
        assert!(lines
            .last()
            .unwrap()
            .contains(&*t(Msg::SheetReadLegend { back: true })));
    }
}
