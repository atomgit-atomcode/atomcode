//! 恢复面板:挑一个别的会话接着聊。
//!
//! 和 `/config`、`/provider`、`/plugin`、`/toolbox`、`/rewind` 是同一件事的又一块——
//! 从底下升起来占住输入框的位置,而不是一个选完就没的 overlay。人从 `/resume` 拉起
//! 它,输入即筛,回车恢复选中的会话。
//!
//! 住在这儿的是**数据和按键**。会话目录怎么来(异步读盘)是 `atomcode-tui` 不认的
//! 事:数据由 `/resume` 命令那一趟异步往返带进来(`crate::commands`),经
//! [`crate::host::Host::show_resume`] 落到 [`crate::moment::Moment::resume`],这个模块
//! 只管挑和画。

use crate::surface::{Key, KeyPress, Mods};

/// 一个能恢复的会话,照面板需要的样子。与 `atomcode_host_api::StoredSession` 同形,
/// 分开一份是因为这一层不认那个 crate,且列表要 clone/比较(理由同
/// [`crate::rewind::Point`])。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    /// 会话名(首句派生或 `/rename`);空着就退回用 id。
    pub title: Option<String>,
    /// 这个会话是在哪个目录里跑的。
    pub working_dir: Option<String>,
    /// 最后活动时间(unix 秒),列表按它说「多久以前」。
    pub updated_at: u64,
    pub turns: u32,
    /// 这个会话是更新版本写的,当前构建读不了——列表说出来,选中也不恢复。
    pub needs_newer_version: bool,
}

impl Session {
    /// 列表里那行亮色标题:有名用名,没名退回 id。
    pub fn heading(&self) -> &str {
        self.title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(&self.id)
    }
}

/// 能恢复的会话们,照宿主最后答的样子。会话装在 `Arc` 里,所以 clone 是一次指针拷贝
/// ——`resume_key` 每敲一下键就 clone 一次这个 view,列表大时逐会话复制会白费力气(和
/// `crate::providers::ProvidersView` 同样的理由)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResumeView {
    sessions: std::sync::Arc<Vec<Session>>,
}

impl ResumeView {
    pub fn new(sessions: Vec<Session>) -> Self {
        Self {
            sessions: std::sync::Arc::new(sessions),
        }
    }

    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// 按搜索框筛出的行:每个是会话在 [`sessions`](Self::sessions) 里的下标,标题 /
    /// id / 目录任一命中即留;搜索框空着就全列。
    pub fn listed(&self, panel: &Panel) -> Vec<usize> {
        let needle = panel.query.trim().to_lowercase();
        self.sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| Self::hit(session, &needle))
            .map(|(index, _)| index)
            .collect()
    }

    fn hit(session: &Session, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        session.heading().to_lowercase().contains(needle)
            || session.id.to_lowercase().contains(needle)
            || session
                .working_dir
                .as_deref()
                .is_some_and(|dir| dir.to_lowercase().contains(needle))
    }

    /// 当前筛出多少行。
    pub fn rows(&self, panel: &Panel) -> usize {
        self.listed(panel).len()
    }
}

/// 恢复面板升着的时候。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Panel {
    /// 箭头停在**筛出行**里的第几个。
    pub cursor: usize,
    /// 搜索框里输的字。
    pub query: String,
}

impl Panel {
    pub fn new() -> Self {
        Self::default()
    }

    /// 把光标点到某一行(按筛出行数夹住)。动了返回 `true`。
    pub fn point_at(&mut self, row: usize, rows: usize) -> bool {
        let want = row.min(rows.saturating_sub(1));
        if self.cursor == want {
            return false;
        }
        self.cursor = want;
        true
    }
}

/// 一次按键让面板的主人去做什么。与 [`crate::rewind::Step`] 同形。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// 面板自己变了,外面没事。
    Stay,
    /// 收起面板。
    Close,
    /// 恢复这个会话。
    Resume { id: String },
}

/// 跑一个键。自由函数、纯的,和 [`crate::rewind::key`] 一样:面板写回去,要恢复哪个
/// 会话由 [`Step::Resume`] 说出来,交给宿主那一趟往返。
pub fn key(view: &ResumeView, panel: &mut Panel, press: KeyPress) -> Step {
    let listed = view.listed(panel);
    match (press.key, press.mods) {
        (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => Step::Close,
        (Key::Up, _) => {
            panel.cursor = panel.cursor.saturating_sub(1);
            Step::Stay
        }
        (Key::Down, _) => {
            if panel.cursor + 1 < listed.len() {
                panel.cursor += 1;
            }
            Step::Stay
        }
        (Key::Enter, _) => {
            match listed.get(panel.cursor).and_then(|&index| view.sessions().get(index)) {
                // 读不了的会话(更新版本写的)不恢复——选中也是空动作,像 rewind 停在
                // 「当前」上按回车。
                Some(session) if !session.needs_newer_version => {
                    Step::Resume { id: session.id.clone() }
                }
                _ => Step::Stay,
            }
        }
        (Key::Backspace, _) => {
            panel.query.pop();
            panel.cursor = 0;
            Step::Stay
        }
        // 输入即筛。任何一次键入把光标带回第一行,不然它会停在一个筛掉了的行上。
        (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
            panel.query.push(c);
            panel.cursor = 0;
            Step::Stay
        }
        _ => Step::Stay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            session("ccc", None),
        ])
    }

    fn press(key: Key) -> KeyPress {
        KeyPress::plain(key)
    }

    #[test]
    fn heading_falls_back_to_id_when_unnamed() {
        assert_eq!(session("ccc", None).heading(), "ccc");
        assert_eq!(session("a", Some("  ")).heading(), "a", "blank name is no name");
        assert_eq!(session("a", Some("real")).heading(), "real");
    }

    #[test]
    fn typing_filters_and_resets_the_cursor() {
        let view = view();
        let mut panel = Panel::new();
        panel.cursor = 2;
        // Type "死" — only the CJK-titled session matches, and the cursor returns
        // to the first row so it never sits on a filtered-out one.
        assert_eq!(key(&view, &mut panel, press(Key::Char('死'))), Step::Stay);
        assert_eq!(panel.query, "死");
        assert_eq!(panel.cursor, 0);
        assert_eq!(view.listed(&panel), vec![1]);
        // Backspace clears it, all three list again.
        assert_eq!(key(&view, &mut panel, press(Key::Backspace)), Step::Stay);
        assert_eq!(view.listed(&panel), vec![0, 1, 2]);
    }

    #[test]
    fn enter_resumes_the_pointed_session_by_id() {
        let view = view();
        let mut panel = Panel::new();
        panel.point_at(1, view.rows(&panel));
        assert_eq!(
            key(&view, &mut panel, press(Key::Enter)),
            Step::Resume { id: "bbb".into() }
        );
    }

    #[test]
    fn a_session_a_newer_build_wrote_cannot_be_resumed() {
        let mut sessions = vec![session("aaa", Some("ok"))];
        sessions[0].needs_newer_version = true;
        let view = ResumeView::new(sessions);
        let mut panel = Panel::new();
        assert_eq!(key(&view, &mut panel, press(Key::Enter)), Step::Stay);
    }

    #[test]
    fn arrows_stay_inside_the_filtered_list() {
        let view = view();
        let mut panel = Panel::new();
        // Down past the end stops at the last listed row.
        for _ in 0..10 {
            key(&view, &mut panel, press(Key::Down));
        }
        assert_eq!(panel.cursor, 2);
        for _ in 0..10 {
            key(&view, &mut panel, press(Key::Up));
        }
        assert_eq!(panel.cursor, 0);
    }

    #[test]
    fn esc_and_ctrl_c_close() {
        let view = view();
        let mut panel = Panel::new();
        assert_eq!(key(&view, &mut panel, press(Key::Esc)), Step::Close);
        assert_eq!(
            key(
                &view,
                &mut panel,
                KeyPress {
                    key: Key::Char('c'),
                    mods: Mods::CTRL,
                }
            ),
            Step::Close
        );
    }
}
