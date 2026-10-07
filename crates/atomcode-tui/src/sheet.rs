//! 底部的「一张单子」:挑一行的列表,或者从头读到尾的一段文字。
//!
//! `/agents`、`/cd`、`/diff`、`/view` 原来各开一个居中的 overlay 弹窗,盖在对话上;
//! 别的面板(`/resume`、`/provider`、`/bg`……)早就是从底下升起来、占住输入框位置的
//! 那一种。这四个命令要的其实只有两样东西 —— 一张挑一行的单子,和一段往下读的文字
//! —— 所以这里是这两样**各一份**,命令只说单子上有什么,长相与按键全在这一处,
//! 不会四个命令各长各的。
//!
//! 住在这儿的是**数据和按键**,和 [`crate::resume`] 同一个分工:画在
//! [`crate::modules::sheet`],挑中了什么由 [`Step`] 说出去,由宿主派发成一条命令
//! —— 挑一行和手打那条命令走的是同一条路。

use std::sync::Arc;

use crate::surface::{Key, KeyPress, Mods};

/// 升着的那张单子,和它背后的那一张。
///
/// `back` 是「esc 回哪儿」:从 `/diff` 的清单里挑一个文件读它的改动,esc 回到那张
/// 清单,**光标还在刚才那一行** —— 读一个文件的改动,几乎总是为了接着读下一个。
/// 原来的弹窗是把列表命令再派发一遍,于是列表从宿主重新拿、光标回到顶上。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sheet {
    pub page: Page,
    pub back: Option<Box<Page>>,
    /// 挑中了一行、那条命令还没回话:回话时认的就是这个号(`Host::settle_sheet_pick`)。
    ///
    /// 命令可能慢(`/diff git` 要问 git),其间人可能已经 esc 掉这张单子、开了别的
    /// 面板,或者又按了一次回车。回话只落在**还是这张、还在等这一次**的单子上;
    /// 在等的时候再按回车不再派发第二次。
    pub pending: Option<u64>,
}

impl Sheet {
    pub fn list(list: List) -> Self {
        Self {
            page: Page::List(list),
            back: None,
            pending: None,
        }
    }

    pub fn doc(doc: Doc) -> Self {
        Self {
            page: Page::Doc(doc),
            back: None,
            pending: None,
        }
    }

    pub fn read(read: Read) -> Self {
        Self {
            page: Page::Read(read),
            back: None,
            pending: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Page {
    List(List),
    Read(Read),
    Doc(Doc),
}

impl Page {
    /// 哪个命令的单子:表头那个名字,也是「这一页能不能垫在那一页下面」的依据。
    pub fn id(&self) -> &'static str {
        match self {
            Page::List(list) => list.id,
            Page::Read(read) => read.id,
            Page::Doc(doc) => doc.id,
        }
    }
}

/// 一小段带语气的字。语气是**意思**(加的、删的、次要的),怎么上色是画的那一层
/// 的事(`crate::theme::Role`)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Piece {
    pub text: String,
    pub tone: Tone,
}

impl Piece {
    pub fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Muted,
    Added,
    Removed,
    Warning,
}

/// 单子上的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// 挑中它时派发的那条命令。
    pub value: String,
    pub label: String,
    /// 标签后面那段灰字。
    pub about: String,
    /// 标签前面一个短记号 —— `/diff` 的 `M`/`A`/`D`。
    pub tag: Option<Piece>,
    /// 靠右的那几个数 —— `/diff` 的 `+12 -3`。
    pub figures: Vec<Piece>,
    /// 标签前一个实心 / 空心圆点:这一项是不是正在用的那个(`/proxy` 的三种模式)。
    pub on: Option<bool>,
    /// 光标停在这一行时,列表下面那几行预览 —— `/changelog` 里一个版本的要点。和
    /// `/resume` 选中会话时底下那几句是同一件事:列表说得出「是哪个」,说不出「里面
    /// 是什么」,而后者才是人决定要不要打开它的依据。
    pub preview: Vec<String>,
}

impl Row {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            about: String::new(),
            tag: None,
            figures: Vec::new(),
            on: None,
            preview: Vec::new(),
        }
    }

    pub fn marked(mut self, on: bool) -> Self {
        self.on = Some(on);
        self
    }

    pub fn about(mut self, about: impl Into<String>) -> Self {
        self.about = about.into();
        self
    }

    pub fn tag(mut self, tag: Piece) -> Self {
        self.tag = Some(tag);
        self
    }

    pub fn figures(mut self, figures: Vec<Piece>) -> Self {
        self.figures = figures;
        self
    }

    pub fn preview(mut self, preview: Vec<String>) -> Self {
        self.preview = preview;
        self
    }
}

/// 挑一行的单子。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct List {
    pub id: &'static str,
    /// 表头名字后面那句话:这张单子是什么、在哪儿。
    pub title: String,
    /// 表头下面一行,说整张单子加起来是什么 —— `/diff` 的「3 个文件 +12 -3」。
    pub summary: Vec<Piece>,
    /// 行装在 `Arc` 里:按一个键就 clone 一次整张单子(`Moment` 是按值走的),而
    /// `/view` 的单子是整个项目的文件。
    pub rows: Arc<Vec<Row>>,
    /// 什么都没列出、或者全筛掉了的时候说什么。
    pub empty: String,
    /// 筛不出东西时,回车是不是把打的字当答案:`{}` 换成打的字。`/cd` 用它 ——
    /// 人本来就知道路径的时候,单子里一条也不会匹配,没有它回车就是个死键。
    pub typed: Option<String>,
    pub cursor: usize,
    pub query: String,
}

impl List {
    pub fn new(id: &'static str, title: impl Into<String>, rows: Vec<Row>) -> Self {
        Self {
            id,
            title: title.into(),
            summary: Vec::new(),
            rows: Arc::new(rows),
            empty: String::new(),
            typed: None,
            cursor: 0,
            query: String::new(),
        }
    }

    pub fn summary(mut self, summary: Vec<Piece>) -> Self {
        self.summary = summary;
        self
    }

    pub fn empty(mut self, empty: impl Into<String>) -> Self {
        self.empty = empty.into();
        self
    }

    pub fn accepting_typed(mut self, template: impl Into<String>) -> Self {
        self.typed = Some(template.into());
        self
    }

    /// 按搜索框筛出的行,每个是在 [`rows`](Self::rows) 里的下标。标签或灰字任一处
    /// 含有打的字(不分大小写)就留下。
    pub fn listed(&self) -> Vec<usize> {
        let needle = self.query.trim().to_lowercase();
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                needle.is_empty()
                    || row.label.to_lowercase().contains(&needle)
                    || row.about.to_lowercase().contains(&needle)
            })
            .map(|(at, _)| at)
            .collect()
    }

    pub fn selected(&self) -> Option<&Row> {
        self.listed()
            .get(self.cursor)
            .and_then(|&at| self.rows.get(at))
    }

    /// 光标点到筛出行的第 `row` 行。动了返回 `true`。
    pub fn point_at(&mut self, row: usize) -> bool {
        let want = row.min(self.listed().len().saturating_sub(1));
        if self.cursor == want {
            return false;
        }
        self.cursor = want;
        true
    }

    /// Tab 把打的字长到还匹配着的那些行共有的开头 —— shell 补全的规矩:只有以打的
    /// 字**开头**的行参加。`None` 是没什么可补,Tab 什么都不做,而不是把打的字缩短。
    fn completion(&self) -> Option<String> {
        if self.query.is_empty() {
            return None;
        }
        let lower = self.query.to_lowercase();
        let starting: Vec<&str> = self
            .listed()
            .into_iter()
            .filter_map(|at| self.rows.get(at))
            .map(|row| row.label.as_str())
            .filter(|label| label.to_lowercase().starts_with(&lower))
            .collect();
        let (first, rest) = starting.split_first()?;
        // 按字符,不按字节:在一个字符中间截断的开头不是字符串,而这些是文件名。
        let head: Vec<char> = first.chars().collect();
        let mut upto = head.len();
        for other in rest {
            let shared = head
                .iter()
                .zip(other.chars())
                .take_while(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
                .count();
            upto = upto.min(shared);
        }
        let grown: String = head[..upto].iter().collect();
        (grown.chars().count() > self.query.chars().count()).then_some(grown)
    }
}

/// 读的那一段里,一行是什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// 文件里的一行。
    Text,
    /// diff 里加的一行。
    Added,
    /// diff 里删的一行。
    Removed,
    /// diff 里没动的上下文。
    Same,
    /// 两段改动之间隔开的那些没画的行。
    Gap,
    /// 不是内容的一句话:`Binary files … differ`、`\ No newline at end of file`。
    Note,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadLine {
    /// 左边那一列的行号。diff 里加的和没动的用新文件的号,删的用旧文件的号。
    pub number: Option<usize>,
    pub mark: Mark,
    pub text: String,
}

/// 从头读到尾的一段字:一个文件,或者一个文件的改动。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Read {
    pub id: &'static str,
    /// 表头里那段:路径、再加上说明(只读了前 N 行之类)、`+a -r`。
    pub title: Vec<Piece>,
    pub lines: Arc<Vec<ReadLine>>,
    /// 视野从第几行开始。
    pub top: usize,
    /// 什么都没有的时候说什么(空文件)。
    pub empty: String,
}

impl Read {
    /// 一个文件,每行带行号。
    pub fn file(id: &'static str, title: Vec<Piece>, text: &str) -> Self {
        Self {
            id,
            title,
            lines: Arc::new(
                text.lines()
                    .enumerate()
                    .map(|(at, line)| ReadLine {
                        number: Some(at + 1),
                        mark: Mark::Text,
                        text: line.to_string(),
                    })
                    .collect(),
            ),
            top: 0,
            empty: String::new(),
        }
    }

    /// 一个文件的改动,从统一 diff 拆成带行号的行 —— 经典界面(tuix)的画法:
    /// `@@`、`+++`、`---` 这些头不画,两段改动之间一行省略号,每行按新旧文件的
    /// 行号标出来。表头里跟着 `+a -r`。
    pub fn diff(id: &'static str, path: &str, text: &str) -> Self {
        let lines = parse_diff(text);
        let added = lines.iter().filter(|l| l.mark == Mark::Added).count();
        let removed = lines.iter().filter(|l| l.mark == Mark::Removed).count();
        Self {
            id,
            title: vec![
                Piece::new(path, Tone::Plain),
                Piece::new(format!("  +{added}"), Tone::Added),
                Piece::new(format!(" -{removed}"), Tone::Removed),
            ],
            lines: Arc::new(lines),
            top: 0,
            empty: String::new(),
        }
    }

    pub fn empty(mut self, empty: impl Into<String>) -> Self {
        self.empty = empty.into();
        self
    }

    /// 视野最多往下走到哪一行开始:最后一屏满着,而不是只剩最后一行。`room` 是
    /// **实际画出来**的行数 —— 屏幕矮的时候比 [`READ_ROWS`] 少,按固定的数算,
    /// 文件末尾那几行就永远滚不到。
    fn last_top(&self, room: usize) -> usize {
        self.lines.len().saturating_sub(room.max(1))
    }

    fn scroll(&mut self, by: isize, room: usize) {
        self.top = self.top.saturating_add_signed(by).min(self.last_top(room));
    }
}

/// 分页签读的一份文档:`/changelog` 里的一个版本,「概览」与「Issues」各一页。
///
/// 和 [`Read`] 不同,正文是 markdown,按面板的宽度折行 —— 链接画成可点的字、地址不
/// 露出来,标题、列表、加粗照回复的样子画。所以「滚到哪儿为止」要按**画出来的**行数
/// 算,按键这一层因此要知道宽度([`doc_width`]),和画的那一层用同一个数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Doc {
    pub id: &'static str,
    /// 表头里命令名后面那段:版本号、日期。
    pub title: Vec<Piece>,
    pub tabs: Arc<Vec<DocTab>>,
    /// 正在看第几页。
    pub tab: usize,
    /// 视野从画出来的第几行开始。
    pub top: usize,
}

/// 文档的一页。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocTab {
    /// 表头里的页签名。
    pub name: String,
    pub markdown: String,
}

impl DocTab {
    pub fn new(name: impl Into<String>, markdown: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            markdown: markdown.into(),
        }
    }
}

impl Doc {
    pub fn new(id: &'static str, title: Vec<Piece>, tabs: Vec<DocTab>) -> Self {
        Self {
            id,
            title,
            tabs: Arc::new(tabs),
            tab: 0,
            top: 0,
        }
    }

    /// 正在看的那一页,宽 `width` 时画出来的样子。
    pub fn lines(&self, width: usize) -> Vec<crate::frame::Line> {
        match self.tabs.get(self.tab) {
            Some(tab) => crate::markdown::render(
                &tab.markdown,
                width.clamp(1, u16::MAX as usize) as u16,
                crate::frame::Style::new(),
            ),
            None => Vec::new(),
        }
    }

    /// 视野最多往下走到哪一行开始:最后一屏满着(理由同 [`Read`])。
    fn last_top(&self, room: usize, width: usize) -> usize {
        self.lines(width).len().saturating_sub(room.max(1))
    }

    fn scroll(&mut self, by: isize, room: usize, width: usize) {
        self.top = self
            .top
            .saturating_add_signed(by)
            .min(self.last_top(room, width));
    }

    /// 换到前一页 / 后一页,绕回;换了页从头读。只有一页时什么都不做。
    fn switch(&mut self, by: isize) {
        let n = self.tabs.len();
        if n <= 1 {
            return;
        }
        self.tab = (self.tab as isize + by).rem_euclid(n as isize) as usize;
        self.top = 0;
    }

    /// 正在看的那一页有没有可点的链接 —— 提示里才说「点击标题打开链接」。
    pub fn has_links(&self) -> bool {
        self.tabs
            .get(self.tab)
            .is_some_and(|tab| tab.markdown.contains("]("))
    }

    /// 直接换到第 `tab` 页(点了页签)。动了返回 `true`。
    pub fn show(&mut self, tab: usize) -> bool {
        if tab >= self.tabs.len() || tab == self.tab {
            return false;
        }
        self.tab = tab;
        self.top = 0;
        true
    }
}

/// 面板宽 `w` 时文档正文按多宽折行:左边空两格,和面板里别的字对齐;右边留两格。
pub fn doc_width(w: usize) -> usize {
    w.saturating_sub(4).max(1)
}

/// 读的那一页最多画多少行字。高度是这一块自己要的,宿主再按屏幕夹 —— 和别的面板
/// 一样按能画下的算,不按屏幕给了多少。
pub const READ_ROWS: usize = 24;

/// 读的那一页除了正文还占几行:规则线、表头、空行,底下空行加提示。
pub const READ_CHROME: usize = 5;

/// 高 `h` 行的一块里,正文能画几行。画的那一层(`crate::modules::sheet`)和按键
/// 这一层用的是这同一个数 —— 两边各算各的,一矮就对不上。
pub fn read_room(h: usize) -> usize {
    h.saturating_sub(READ_CHROME).clamp(1, READ_ROWS)
}

/// 把一段统一 diff 拆成要画的行。
///
/// 认得的:`@@ -a,b +c,d @@`(从这儿起算行号)、`+`/`-`/空格开头的内容行、`\` 开头
/// 的附注、`Binary files`。认不得的头(`diff --git`、`index`、`---`、`+++`、
/// `new file mode`……)不画 —— 它们是给 git 看的,人读的是改了什么。
pub fn parse_diff(text: &str) -> Vec<ReadLine> {
    let mut out = Vec::new();
    let (mut old, mut new) = (0usize, 0usize);
    let mut in_hunk = false;
    let mut hunks = 0usize;
    for line in text.lines() {
        if let Some((from, to)) = hunk_start(line) {
            if hunks > 0 {
                out.push(ReadLine {
                    number: None,
                    mark: Mark::Gap,
                    text: String::new(),
                });
            }
            hunks += 1;
            old = from;
            new = to;
            in_hunk = true;
            continue;
        }
        if line.starts_with("diff --git ") {
            in_hunk = false;
            continue;
        }
        if !in_hunk {
            // 一段改动之前的那些头。给 git 看的(`index`、`---`、`+++`)不画;说出
            // 「改了什么」的留着 —— 只改了名字或权限的文件没有一段内容改动,
            // 这几句就是它全部的改动,丢了就成了一个空页。
            const TOLD: [&str; 9] = [
                "Binary files ",
                "rename from ",
                "rename to ",
                "copy from ",
                "copy to ",
                "old mode ",
                "new mode ",
                "new file mode ",
                "deleted file mode ",
            ];
            if TOLD.iter().any(|head| line.starts_with(head)) {
                out.push(ReadLine {
                    number: None,
                    mark: Mark::Note,
                    text: line.to_string(),
                });
            }
            continue;
        }
        let (mark, number, rest) = match line.as_bytes().first() {
            Some(b'+') => {
                new += 1;
                (Mark::Added, Some(new - 1), line.get(1..).unwrap_or(""))
            }
            Some(b'-') => {
                old += 1;
                (Mark::Removed, Some(old - 1), line.get(1..).unwrap_or(""))
            }
            Some(b' ') => {
                old += 1;
                new += 1;
                (Mark::Same, Some(new - 1), line.get(1..).unwrap_or(""))
            }
            Some(b'\\') => (Mark::Note, None, line),
            // 空行在一段改动里是没动的空上下文(有的工具把行首的空格也吃掉了)。
            None => {
                old += 1;
                new += 1;
                (Mark::Same, Some(new - 1), "")
            }
            _ => (Mark::Note, None, line),
        };
        out.push(ReadLine {
            number,
            mark,
            text: rest.to_string(),
        });
    }
    out
}

/// `@@ -12,5 +12,7 @@ …` 的两个起始行号。
fn hunk_start(line: &str) -> Option<(usize, usize)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    let first = |range: &str| range.split(',').next()?.parse::<usize>().ok();
    Some((first(old)?, first(new)?))
}

/// 一次按键让单子的主人去做什么。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// 单子自己变了(或没变),外面没事。
    Stay,
    /// 收起来。
    Close,
    /// 挑中了:派发这条命令。
    Chose(String),
}

/// 列表每次翻一页走几行。
const LIST_PAGE: usize = 10;

/// 文档页不知道面板多宽时按这个宽度折行(测试,以及还没画过的那一刻)。
pub const DOC_WIDTH: usize = 80;

/// 跑一个键。自由函数、纯的:单子写回去,挑中什么由 [`Step::Chose`] 说出去。
/// `room` 是读的那页此刻画出来的正文行数([`read_room`]),翻页与滚到底按它算。
pub fn key(sheet: &mut Sheet, press: KeyPress, room: usize) -> Step {
    key_in(sheet, press, room, DOC_WIDTH)
}

/// [`key`],知道面板有多宽:文档页按画出来的行数滚,而行数跟着宽度变
/// ([`doc_width`])。
pub fn key_in(sheet: &mut Sheet, press: KeyPress, room: usize, width: usize) -> Step {
    // Ctrl-C 哪一页都是「不要了」:不回到背后那张,整个收起来。
    if matches!((press.key, press.mods), (Key::Char('c'), Mods::CTRL)) {
        return Step::Close;
    }
    match &mut sheet.page {
        Page::List(list) => list_key(list, press),
        Page::Read(read) => {
            let leave = matches!(
                (press.key, press.mods),
                (Key::Esc, _) | (Key::Left, _) | (Key::Char('q'), Mods::NONE)
            );
            if leave {
                // 背后有一张单子就回到它 —— 光标还在原来那一行;没有就收起。
                return match sheet.back.take() {
                    Some(page) => {
                        sheet.page = *page;
                        Step::Stay
                    }
                    None => Step::Close,
                };
            }
            read_key(read, press, room);
            Step::Stay
        }
        Page::Doc(doc) => {
            // 左右键在这儿是换页签,所以只有 esc / q 是离开 —— 和读的那页一样,
            // 背后有一张单子就回到它。
            if matches!(
                (press.key, press.mods),
                (Key::Esc, _) | (Key::Char('q'), Mods::NONE)
            ) {
                return match sheet.back.take() {
                    Some(page) => {
                        sheet.page = *page;
                        Step::Stay
                    }
                    None => Step::Close,
                };
            }
            doc_key(doc, press, room, doc_width(width));
            Step::Stay
        }
    }
}

fn doc_key(doc: &mut Doc, press: KeyPress, room: usize, width: usize) {
    let page = room.saturating_sub(1).max(1) as isize;
    match (press.key, press.mods) {
        (Key::Tab, _) | (Key::Right, _) | (Key::Char('l'), Mods::NONE) => doc.switch(1),
        (Key::BackTab, _) | (Key::Left, _) | (Key::Char('h'), Mods::NONE) => doc.switch(-1),
        (Key::Up, _) | (Key::Char('k'), Mods::NONE) => doc.scroll(-1, room, width),
        (Key::Down, _) | (Key::Char('j'), Mods::NONE) => doc.scroll(1, room, width),
        (Key::PageUp, _) => doc.scroll(-page, room, width),
        (Key::PageDown, _) | (Key::Char(' '), Mods::NONE) => doc.scroll(page, room, width),
        (Key::Home, _) | (Key::Char('g'), Mods::NONE) => doc.top = 0,
        (Key::End, _) | (Key::Char('G'), Mods::NONE | Mods::SHIFT) => {
            doc.top = doc.last_top(room, width)
        }
        _ => {}
    }
}

fn list_key(list: &mut List, press: KeyPress) -> Step {
    let rows = list.listed().len();
    match (press.key, press.mods) {
        (Key::Esc, _) => Step::Close,
        (Key::Up, _) | (Key::Char('p'), Mods::CTRL) => {
            list.cursor = list.cursor.saturating_sub(1);
            Step::Stay
        }
        (Key::Down, _) | (Key::Char('n'), Mods::CTRL) => {
            if list.cursor + 1 < rows {
                list.cursor += 1;
            }
            Step::Stay
        }
        (Key::PageUp, _) => {
            list.cursor = list.cursor.saturating_sub(LIST_PAGE);
            Step::Stay
        }
        (Key::PageDown, _) => {
            list.cursor = (list.cursor + LIST_PAGE).min(rows.saturating_sub(1));
            Step::Stay
        }
        (Key::Home, _) => {
            list.cursor = 0;
            Step::Stay
        }
        (Key::End, _) => {
            list.cursor = rows.saturating_sub(1);
            Step::Stay
        }
        (Key::Enter, _) => match list.selected() {
            Some(row) => Step::Chose(row.value.clone()),
            // 一条都没筛出来,而这张单子认打的字:打的就是答案。
            None => match (&list.typed, list.query.trim()) {
                (Some(template), typed) if !typed.is_empty() => {
                    Step::Chose(template.replace("{}", typed))
                }
                _ => Step::Stay,
            },
        },
        (Key::Tab, _) => {
            if let Some(grown) = list.completion() {
                list.query = grown;
                list.cursor = 0;
            }
            Step::Stay
        }
        (Key::Backspace, _) => {
            list.query.pop();
            list.cursor = 0;
            Step::Stay
        }
        // 输入即筛。任何一次键入把光标带回第一行,不然它会停在一个筛掉了的行上。
        (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
            list.query.push(c);
            list.cursor = 0;
            Step::Stay
        }
        _ => Step::Stay,
    }
}

fn read_key(read: &mut Read, press: KeyPress, room: usize) {
    // 翻一页走的比一屏少一行:翻过去还看得见上一页的最后一行,知道接在哪儿。
    let page = room.saturating_sub(1).max(1) as isize;
    match (press.key, press.mods) {
        (Key::Up, _) | (Key::Char('k'), Mods::NONE) => read.scroll(-1, room),
        (Key::Down, _) | (Key::Char('j'), Mods::NONE) => read.scroll(1, room),
        (Key::PageUp, _) => read.scroll(-page, room),
        (Key::PageDown, _) | (Key::Char(' '), Mods::NONE) => read.scroll(page, room),
        (Key::Home, _) | (Key::Char('g'), Mods::NONE) => read.top = 0,
        (Key::End, _) | (Key::Char('G'), Mods::NONE | Mods::SHIFT) => {
            read.top = read.last_top(room)
        }
        // 别的键什么都不做 —— 原来的弹窗是按任何键都关掉,一个手滑就得从头打开。
        _ => {}
    }
}

/// 滚轮:列表挪光标,读的那页挪视野。
pub fn wheel(sheet: &mut Sheet, by: i32, room: usize) {
    wheel_in(sheet, by, room, DOC_WIDTH)
}

/// [`wheel`],知道面板有多宽(理由同 [`key_in`])。
pub fn wheel_in(sheet: &mut Sheet, by: i32, room: usize, width: usize) {
    match &mut sheet.page {
        Page::List(list) => {
            let rows = list.listed().len();
            list.cursor =
                (list.cursor as i64 + by as i64).clamp(0, rows.saturating_sub(1) as i64) as usize;
        }
        Page::Read(read) => read.scroll(by as isize, room),
        Page::Doc(doc) => doc.scroll(by as isize, room, doc_width(width)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key: Key) -> KeyPress {
        KeyPress::plain(key)
    }

    fn typed(sheet: &mut Sheet, text: &str) {
        for c in text.chars() {
            key(sheet, KeyPress::ch(c), READ_ROWS);
        }
    }

    fn list() -> Sheet {
        Sheet::list(List::new(
            "diff",
            "t",
            vec![
                Row::new("/diff a.rs", "a.rs").about("modified"),
                Row::new("/diff b.rs", "b.rs").about("added"),
                Row::new("/diff lib/c.rs", "lib/c.rs").about("modified"),
            ],
        ))
    }

    fn cursor(sheet: &Sheet) -> usize {
        match &sheet.page {
            Page::List(list) => list.cursor,
            Page::Read(_) | Page::Doc(_) => panic!("a list"),
        }
    }

    fn doc() -> Sheet {
        let overview: String = (1..=60).map(|n| format!("- point {n}\n")).collect();
        Sheet::doc(Doc::new(
            "changelog",
            vec![Piece::new("v5.2.2", Tone::Plain)],
            vec![
                DocTab::new("Overview", overview),
                DocTab::new("Issues (1)", "- [#1182 QR codes](https://example.com/1182)"),
            ],
        ))
    }

    fn doc_of(sheet: &Sheet) -> &Doc {
        match &sheet.page {
            Page::Doc(doc) => doc,
            _ => panic!("a document"),
        }
    }

    /// Tab 往后、Shift+Tab 往前,绕回;换了页从头读。
    #[test]
    fn a_document_switches_tabs_and_starts_each_from_the_top() {
        let mut sheet = doc();
        key(&mut sheet, press(Key::Down), 10);
        assert_eq!(doc_of(&sheet).top, 1);
        key(&mut sheet, press(Key::Tab), 10);
        assert_eq!((doc_of(&sheet).tab, doc_of(&sheet).top), (1, 0));
        key(&mut sheet, press(Key::Tab), 10);
        assert_eq!(doc_of(&sheet).tab, 0, "wraps");
        key(&mut sheet, press(Key::BackTab), 10);
        assert_eq!(doc_of(&sheet).tab, 1);
        key(&mut sheet, press(Key::Left), 10);
        assert_eq!(doc_of(&sheet).tab, 0);
    }

    /// 滚到底停在最后一屏满着的地方,按画出来的行数算,不越过去。
    #[test]
    fn a_document_scrolls_no_further_than_its_last_screen() {
        let mut sheet = doc();
        let lines = doc_of(&sheet).lines(doc_width(DOC_WIDTH)).len();
        key(&mut sheet, press(Key::End), 10);
        assert_eq!(doc_of(&sheet).top, lines - 10);
        key(&mut sheet, press(Key::PageDown), 10);
        assert_eq!(doc_of(&sheet).top, lines - 10, "no further");
        key(&mut sheet, press(Key::Home), 10);
        assert_eq!(doc_of(&sheet).top, 0);
        wheel(&mut sheet, 3, 10);
        assert_eq!(doc_of(&sheet).top, 3);
    }

    /// 从单子里点进来的文档,esc 回到单子;直接打开的,esc 收起。
    #[test]
    fn esc_on_a_document_goes_back_to_the_list_it_came_from() {
        let mut sheet = doc();
        assert_eq!(key(&mut sheet, press(Key::Esc), 10), Step::Close);
        let mut sheet = doc();
        let mut behind = list();
        if let Page::List(list) = &mut behind.page {
            list.cursor = 2;
        }
        sheet.back = Some(Box::new(behind.page));
        assert_eq!(key(&mut sheet, press(Key::Esc), 10), Step::Stay);
        assert_eq!(cursor(&sheet), 2, "the cursor where it was");
    }

    /// 上下走、不绕回;回车派发那一行的命令;esc 收起。
    #[test]
    fn a_list_moves_picks_and_closes() {
        let mut sheet = list();
        key(&mut sheet, press(Key::Up), READ_ROWS);
        assert_eq!(cursor(&sheet), 0, "不从顶上绕到底下");
        key(&mut sheet, press(Key::Down), READ_ROWS);
        key(&mut sheet, press(Key::Down), READ_ROWS);
        key(&mut sheet, press(Key::Down), READ_ROWS);
        assert_eq!(cursor(&sheet), 2, "也不从底下绕回顶上");
        assert_eq!(
            key(&mut sheet, press(Key::Enter), READ_ROWS),
            Step::Chose("/diff lib/c.rs".into())
        );
        key(&mut sheet, press(Key::Home), READ_ROWS);
        assert_eq!(cursor(&sheet), 0);
        assert_eq!(key(&mut sheet, press(Key::Esc), READ_ROWS), Step::Close);
    }

    /// 输入即筛,标签和灰字都算;Tab 补到共有的开头;筛不出东西时认打的字的单子
    /// 把它当答案,不认的回车什么都不做。
    #[test]
    fn typing_filters_completes_and_can_be_the_answer() {
        let mut sheet = list();
        typed(&mut sheet, "ADD");
        assert_eq!(
            key(&mut sheet, press(Key::Enter), READ_ROWS),
            Step::Chose("/diff b.rs".into()),
            "灰字里命中也算,不分大小写"
        );

        let mut sheet = list();
        typed(&mut sheet, "l");
        key(&mut sheet, press(Key::Tab), READ_ROWS);
        match &sheet.page {
            Page::List(list) => assert_eq!(list.query, "lib/c.rs"),
            Page::Read(_) | Page::Doc(_) => unreachable!(),
        }

        let mut sheet = list();
        typed(&mut sheet, "zzz");
        assert_eq!(key(&mut sheet, press(Key::Enter), READ_ROWS), Step::Stay);

        let mut sheet = Sheet::list(List::new("cd", "t", Vec::new()).accepting_typed("/cd {}"));
        typed(&mut sheet, "/tmp/x");
        assert_eq!(
            key(&mut sheet, press(Key::Enter), READ_ROWS),
            Step::Chose("/cd /tmp/x".into())
        );
    }

    /// 读的那页:上下、翻页、首尾都在界内;别的键不关;esc 回到背后那张单子,
    /// 光标还在原来那一行,没有背后那张就收起。
    #[test]
    fn a_reader_scrolls_and_goes_back_to_the_list_it_came_from() {
        let text: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        let mut sheet = Sheet::read(Read::file("view", Vec::new(), &text));
        let top = |sheet: &Sheet| match &sheet.page {
            Page::Read(read) => read.top,
            Page::List(_) | Page::Doc(_) => panic!("a reader"),
        };
        key(&mut sheet, press(Key::Up), READ_ROWS);
        assert_eq!(top(&sheet), 0);
        key(&mut sheet, press(Key::PageDown), READ_ROWS);
        assert_eq!(top(&sheet), READ_ROWS - 1);
        key(&mut sheet, press(Key::End), READ_ROWS);
        assert_eq!(top(&sheet), 100 - READ_ROWS, "最后一屏是满的");
        key(&mut sheet, press(Key::Down), READ_ROWS);
        assert_eq!(top(&sheet), 100 - READ_ROWS);
        key(&mut sheet, press(Key::Char('g')), READ_ROWS);
        assert_eq!(top(&sheet), 0);
        assert_eq!(
            key(&mut sheet, press(Key::Enter), READ_ROWS),
            Step::Stay,
            "手滑不关"
        );

        // 屏幕矮、只画得下 15 行的时候,滚到底就是最后 15 行 —— 按固定的一屏
        // 算,最后那几行永远到不了。
        key(&mut sheet, press(Key::End), 15);
        assert_eq!(top(&sheet), 100 - 15);
        key(&mut sheet, press(Key::Up), 15);
        assert_eq!(top(&sheet), 100 - 16, "从底下往回走,第一下就动");
        assert_eq!(key(&mut sheet, press(Key::Esc), READ_ROWS), Step::Close);

        let mut behind = list();
        key(&mut behind, press(Key::Down), READ_ROWS);
        let mut sheet = Sheet {
            page: Page::Read(Read::diff("diff", "b.rs", "")),
            back: Some(Box::new(behind.page)),
            pending: None,
        };
        assert_eq!(key(&mut sheet, press(Key::Esc), READ_ROWS), Step::Stay);
        assert_eq!(cursor(&sheet), 1, "回到清单,光标还在读的那一行上");
        assert!(sheet.back.is_none());
    }

    /// 统一 diff 拆成带行号的行:头不画,两段之间一行省略,加的用新号、删的用旧号。
    #[test]
    fn a_diff_is_numbered_the_way_the_file_is() {
        let text = "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n\
                    @@ -3,3 +3,3 @@ fn f() {\n keep\n-old\n+new\n@@ -20,1 +20,2 @@\n same\n+more\n\
                    \\ No newline at end of file\n";
        let lines = parse_diff(text);
        let got: Vec<(Option<usize>, Mark, &str)> = lines
            .iter()
            .map(|l| (l.number, l.mark, l.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (Some(3), Mark::Same, "keep"),
                (Some(4), Mark::Removed, "old"),
                (Some(4), Mark::Added, "new"),
                (None, Mark::Gap, ""),
                (Some(20), Mark::Same, "same"),
                (Some(21), Mark::Added, "more"),
                (None, Mark::Note, "\\ No newline at end of file"),
            ]
        );
        // 只改了名字、没有一段内容改动的:说出改了什么的那几句留着,不是一个空页。
        let renamed = parse_diff(
            "diff --git a/old.rs b/new.rs\nsimilarity index 100%\nrename from old.rs\nrename to new.rs\n",
        );
        let told: Vec<&str> = renamed.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(told, vec!["rename from old.rs", "rename to new.rs"]);
        assert!(renamed.iter().all(|l| l.mark == Mark::Note));

        let read = Read::diff("diff", "x", text);
        let title: String = read.title.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(title, "x  +2 -1");
    }
}
