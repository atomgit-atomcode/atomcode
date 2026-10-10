//! 后台面板:放到后台接着跑的那些会话,按「要不要人管」分组,挑一个打开、就地回一句、
//! 丢掉,或者写一个任务新开一个(`docs/plans/2026-09-25-bg-design.md` §四)。
//!
//! 和 `/resume`、`/rewind` 是同一家的面板:从底下升起来,开着的时候键归它。住在这儿
//! 的是**数据和按键**;列表由宿主推来(`HostEvent::BackgroundChanged`,或 `/bg` 那一趟
//! 往返的回答),经 [`crate::host::Host::show_bg`] 落到 [`crate::moment::Moment::bg`];
//! 面板要做的事都说成一条命令(`/resume <id>`、`/background <任务>`、`/bg tell`、
//! `/bg drop`),由 [`Step`] 交出去——面板和敲命令走的是同一条路。

use crate::surface::{Key, KeyPress, Mods};

/// 面板上的三组,顺序就是画的顺序。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    /// 在等人回答,或出了错。
    NeedsInput,
    /// 回合在跑。
    Working,
    /// 做完了(或被停了、或从没跑过)。
    Completed,
}

impl Group {
    pub const ALL: [Group; 3] = [Group::NeedsInput, Group::Working, Group::Completed];

    /// 宿主说的状态落在哪一组。
    pub fn of(state: atomcode_host_api::BackgroundState) -> Self {
        use atomcode_host_api::BackgroundState as S;
        match state {
            S::Waiting | S::Failed => Group::NeedsInput,
            S::Running => Group::Working,
            _ => Group::Completed,
        }
    }
}

/// 一个后台会话,照面板要的样子。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    /// 会话名;没起名就退回 id 的前八位。
    pub title: String,
    pub group: Group,
    /// 那一行右边的摘要。
    pub last: Option<String>,
    /// 在等人回答(审批或提问)——前台那条提示只为这个出现,出错不算。
    pub waiting: bool,
    /// 没干完就停了(出错、熔断、轮数用尽):什么都不会投回来,所以前台要说一句。
    pub failed: bool,
    /// 替哪个会话干活(发起它、结果要投回去的那个)。`None` = 不替谁干活。
    pub origin: Option<String>,
    /// 这次活花掉的,宿主从它自己的日志折出来。`None` = 还没发过请求,或那个宿主
    /// 不读后台会话的日志——两种都没有可说的数。
    pub stats: Option<atomcode_host_api::BackgroundStats>,
}

impl Session {
    pub fn from_host(session: atomcode_host_api::BackgroundSession) -> Self {
        let title = session
            .title
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| session.session.chars().take(8).collect());
        Self {
            id: session.session,
            title,
            group: Group::of(session.state),
            last: session.last,
            waiting: session.state == atomcode_host_api::BackgroundState::Waiting,
            failed: session.state == atomcode_host_api::BackgroundState::Failed,
            origin: session.origin,
            stats: session.stats,
        }
    }
}

/// 那六个数字,画成本机那条回合汇总的样子
/// (`2 轮 · 2 工具 · 32.7s · 2.60K tokens · 97% cached`)。
///
/// 一条请求都没发过的用不着补零:本机的 `caption` 那时答 `None`,这里也就空着 ——
/// 「0 轮  0 工具」不是关于这次活的信息。空串 = 没得说。
pub(crate) fn figures(stats: Option<atomcode_host_api::BackgroundStats>) -> String {
    let Some(stats) = stats else {
        return String::new();
    };
    // The turn summary's own reckoning: what every request sent, summed. A host
    // from before the sums carries only the last request's — said as it was.
    let (prompt, cached) = if stats.sent > 0 {
        (stats.sent, stats.sent_cached)
    } else {
        (u64::from(stats.prompt), u64::from(stats.cached))
    };
    crate::content::TurnStats {
        steps: stats.steps,
        prompt,
        completion: u64::from(stats.completion),
        cached,
        tools: stats.tools,
        elapsed_ms: stats.elapsed_ms,
    }
    .caption(true)
    .unwrap_or_default()
}

/// 后台会话们,**按画的顺序**:先按组,组内按宿主给的顺序(放进后台的先后)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BgView {
    sessions: std::sync::Arc<Vec<Session>>,
    /// 宿主给的顺序里的 id,`/bg <N>` 的 N 按它数。
    slots: std::sync::Arc<Vec<String>>,
}

impl BgView {
    pub fn new(sessions: Vec<Session>) -> Self {
        let slots = sessions.iter().map(|s| s.id.clone()).collect();
        let mut ordered = sessions;
        // 稳定排序:组内保持宿主的顺序。
        ordered.sort_by_key(|session| session.group);
        Self {
            sessions: std::sync::Arc::new(ordered),
            slots: std::sync::Arc::new(slots),
        }
    }

    pub fn from_host(sessions: Vec<atomcode_host_api::BackgroundSession>) -> Self {
        Self::new(sessions.into_iter().map(Session::from_host).collect())
    }

    /// 按画的顺序。
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// 某个会话在 `/bg list` 里的号(从 1 数)。
    pub fn slot_of(&self, id: &str) -> Option<usize> {
        self.slots.iter().position(|s| s == id).map(|at| at + 1)
    }

    /// 还在跑的(进行中,或在等人)有几个——退出前要问的就是它们。
    pub fn running(&self) -> usize {
        self.sessions
            .iter()
            .filter(|s| s.group != Group::Completed)
            .count()
    }

    /// 替 `session` 干活、还在干的有几个 —— 它的结果会回到那段对话里。
    ///
    /// 只数在干的(`Working`):在等人回答的有自己那条提示,停了的有自己那句话,
    /// 这个数回答的是「还有多少结果在路上」。
    pub fn working_for(&self, session: &str) -> usize {
        self.sessions
            .iter()
            .filter(|s| s.group == Group::Working && s.origin.as_deref() == Some(session))
            .count()
    }

    /// 还在跑的里面,有几个在等人回答。
    pub fn waiting(&self) -> usize {
        self.sessions.iter().filter(|s| s.waiting).count()
    }

    /// 前台那一行提示:第一个在等人回答的后台会话,和怎么打开它。
    pub fn waiting_caption(&self) -> Option<String> {
        let session = self.sessions.iter().find(|s| s.waiting)?;
        let slot = self.slot_of(&session.id)?;
        let title: String = session.title.chars().take(40).collect();
        Some(
            crate::i18n::t(crate::i18n::Msg::BgWaitingTip {
                slot,
                title: &title,
            })
            .into_owned(),
        )
    }

    /// 相对 `before`,刚刚停在「没干完」的那些:*(号, 标题)*。
    ///
    /// 干完的不在这里:它的内容由宿主投回发起它的那段对话,那才是该出现的消息。
    /// 没干完的什么都不投,发起它的人要是不看面板,就永远不知道它停了 —— 这一句
    /// 只说这件事,每个会话每次停下只说一次(之前已经是失败的不再说)。
    pub fn newly_failed(&self, before: &BgView) -> Vec<(usize, String)> {
        self.sessions
            .iter()
            .filter(|s| s.failed)
            .filter(|s| !before.sessions.iter().any(|b| b.id == s.id && b.failed))
            .filter_map(|s| {
                let slot = self.slot_of(&s.id)?;
                Some((slot, s.title.chars().take(40).collect()))
            })
            .collect()
    }

    /// 画的顺序里第几个是哪个会话的 id。
    pub fn at(&self, cursor: usize) -> Option<&Session> {
        self.sessions.get(cursor)
    }

    /// 某个 id 在画的顺序里的位置。
    pub fn position(&self, id: &str) -> Option<usize> {
        self.sessions.iter().position(|s| s.id == id)
    }
}

/// 后台面板升着的时候。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Panel {
    /// 箭头停在画的顺序里的第几个。
    pub cursor: usize,
    /// 底下输入框里的字。
    pub input: String,
    /// 在就地回复哪个会话(space 进、esc 出)。`None` 时输入框里写的是新任务。
    pub replying: Option<String>,
    /// 按了 `?`,图例换成完整的按键说明。
    pub keys: bool,
    /// 是被 `/bg` 移走的那个会话打开的:esc 回到它。`None` 是 `/bg list` 打开的,
    /// esc 就只是收起面板,回到原来那个会话。
    pub moved: Option<String>,
    /// When this panel was opened from the team strip, the conversation that
    /// owns that strip. Opening the selected background session carries this
    /// breadcrumb across the ensuing `/resume`.
    pub return_to: Option<String>,
    /// 光标要落在的会话——列表可能晚一步到,到了再落。
    pub aim: Option<String>,
    /// 刚才想就地回复一个在等你回答的会话:话不发,图例那一行换成「按 Enter 打开」。
    /// 下一次按键就收起。
    pub waiting_note: bool,
    /// 按了一次 ctrl+d、等第二次的那个会话。跟着行走:按了别的键就没了 —— 丢掉
    /// 一个后台会话会把它跑着的回合一起取消,所以要问两次。
    pub armed: Option<String>,
}

impl Panel {
    pub fn new(moved: Option<String>) -> Self {
        Self {
            aim: moved.clone(),
            moved,
            return_to: None,
            ..Self::default()
        }
    }

    /// 点了第 `at` 行:没选中就选中它,已经选中的再点一次就打开。
    pub fn click(&mut self, view: &BgView, at: usize) -> Step {
        self.waiting_note = false;
        self.armed = None;
        let Some(session) = view.at(at) else {
            return Step::Stay;
        };
        if self.cursor == at {
            return Step::Open {
                id: session.id.clone(),
            };
        }
        self.cursor = at;
        Step::Stay
    }

    /// 滚轮:光标往上 / 往下走 `by` 行,夹在列表里。动了返回 `true`。
    pub fn wheel(&mut self, view: &BgView, by: i32) -> bool {
        let last = view.sessions().len().saturating_sub(1);
        let want = if by < 0 {
            self.cursor.saturating_sub(by.unsigned_abs() as usize)
        } else {
            self.cursor.saturating_add(by as usize).min(last)
        };
        let moved = want != self.cursor;
        self.cursor = want;
        // 移开了就不再等第二次 ctrl+d —— 和方向键、点击同一个规矩,不然那一行
        // 还说着「再按一次」,光标却已经在别的行上。
        if moved {
            self.armed = None;
        }
        moved
    }

    /// 列表换了:光标要是在等一个会话出现,它出现了就落上去;否则夹在列表里。
    pub fn settle(&mut self, view: &BgView) {
        if let Some(aim) = self.aim.as_deref() {
            if let Some(at) = view.position(aim) {
                self.cursor = at;
                self.aim = None;
                return;
            }
        }
        self.cursor = self.cursor.min(view.sessions().len().saturating_sub(1));
        if let Some(replying) = self.replying.as_deref() {
            // 回复的对象没了,回复也就没了。
            if view.position(replying).is_none() {
                self.replying = None;
                self.input.clear();
            }
        }
    }
}

/// 一次按键让面板的主人去做什么。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// 面板自己变了,外面没事。
    Stay,
    /// 收起面板,留在现在这个会话。
    Close,
    /// 把这个后台会话带到前台(面板随之收起)。
    Open { id: String },
    /// 新开一个后台会话去做这件事。
    Start { task: String },
    /// 给这个后台会话发一句话,不切过去。
    Tell { id: String, text: String },
    /// 丢掉这个后台会话。
    Drop { id: String },
}

/// 跑一个键。纯函数:面板写回去,要做的事由 [`Step`] 说出来。
///
/// ctrl+c 不在这里:它照常走两次退出(路由那一侧不把它交给面板)。
pub fn key(view: &BgView, panel: &mut Panel, press: KeyPress) -> Step {
    let selected = view.at(panel.cursor).map(|s| s.id.clone());
    panel.waiting_note = false;
    // 除了 ctrl+d 自己,任何一次按键都解除待删:人已经去做别的事了。
    let dropping = matches!((press.key, press.mods), (Key::Char('d'), Mods::CTRL));
    let armed = if dropping {
        panel.armed.take()
    } else {
        panel.armed = None;
        None
    };
    // 在等审批或提问的会话不收「回复」:它等的是那个问题的答案,一句话塞进去只会
    // 换回运行时的一个拒绝。要答,得把它打开。
    let waits = |id: &str| {
        view.position(id)
            .and_then(|at| view.at(at))
            .is_some_and(|s| s.waiting)
    };
    if !matches!(press.key, Key::Char('?')) || !panel.input.is_empty() {
        panel.keys = false;
    }
    match (press.key, press.mods) {
        (Key::Esc, _) => {
            if panel.replying.take().is_some() || !panel.input.is_empty() {
                // 先退出回复 / 清掉没发的字:esc 是「退一步」,不是一下退到底。
                panel.input.clear();
                return Step::Stay;
            }
            match panel.moved.clone() {
                // 刚移走的那个还在后台:回到它。
                Some(moved) if view.position(&moved).is_some() => Step::Open { id: moved },
                _ => Step::Close,
            }
        }
        // ← 在空输入框上拉起了这块面板,空着的时候 ← 再把它收起——留在现在这个
        // 会话。`/bg` 之后要在新的前台说话,走的就是这一下(esc 是回到刚移走的那个)。
        (Key::Left, Mods::NONE) if panel.input.is_empty() && panel.replying.is_none() => {
            Step::Close
        }
        // 丢掉选中的那个:ctrl+d 按两次 —— 和 `/resume`、`/provider`、`/plugin`、
        // `/settings` 同一个手势。原来是 ctrl+x 按一下就丢,连它跑着的回合一起
        // 取消,没有第二次机会。
        _ if dropping => match selected {
            Some(id) if armed.as_deref() == Some(id.as_str()) => Step::Drop { id },
            Some(id) => {
                panel.armed = Some(id);
                Step::Stay
            }
            None => Step::Stay,
        },
        (Key::Up, _) => {
            panel.cursor = panel.cursor.saturating_sub(1);
            Step::Stay
        }
        (Key::Down, _) => {
            if panel.cursor + 1 < view.sessions().len() {
                panel.cursor += 1;
            }
            Step::Stay
        }
        (Key::Enter, _) => {
            let text = panel.input.trim().to_string();
            if let Some(id) = panel.replying.clone() {
                if text.is_empty() {
                    return Step::Stay;
                }
                if waits(&id) {
                    // 回复写到一半,它开始等人了:话留在框里,不发。
                    panel.waiting_note = true;
                    return Step::Stay;
                }
                panel.input.clear();
                panel.replying = None;
                return Step::Tell { id, text };
            }
            if !text.is_empty() {
                panel.input.clear();
                return Step::Start { task: text };
            }
            match selected {
                Some(id) => Step::Open { id },
                None => Step::Stay,
            }
        }
        (Key::Backspace, _) => {
            panel.input.pop();
            Step::Stay
        }
        // 空着的输入框里,space 是「回复选中的那个」,`?` 是按键说明;写了字之后
        // 它们就只是字。
        (Key::Char(' '), Mods::NONE) if panel.input.is_empty() && panel.replying.is_none() => {
            match selected {
                Some(id) if waits(&id) => panel.waiting_note = true,
                Some(id) => panel.replying = Some(id),
                None => {}
            }
            Step::Stay
        }
        (Key::Char('?'), Mods::NONE | Mods::SHIFT)
            if panel.input.is_empty() && panel.replying.is_none() =>
        {
            panel.keys = !panel.keys;
            Step::Stay
        }
        (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
            panel.input.push(c);
            Step::Stay
        }
        _ => Step::Stay,
    }
}

#[cfg(test)]
mod tests {

    /// The panel's figures are the turn summary's reckoning — summed over every
    /// request — and fall back to the last request's from a host that does not
    /// send the sums.
    #[test]
    fn figures_use_the_summed_cost_when_the_host_sends_it() {
        let stats = |sent, sent_cached| atomcode_host_api::BackgroundStats {
            steps: 2,
            tools: 0,
            prompt: 1200,
            cached: 1100,
            completion: 20,
            elapsed_ms: 1000,
            sent,
            sent_cached,
        };
        // 1100 of 2200 summed: 50%, and 20 + 1100 missed = 1.12K.
        let summed = figures(Some(stats(2200, 1100)));
        assert!(
            summed.contains("50%") && summed.contains("1.12K"),
            "{summed}"
        );
        // An older host: the last request's 1100 of 1200.
        let older = figures(Some(stats(0, 0)));
        assert!(older.contains("91%"), "{older}");
    }
    use super::*;

    fn session(id: &str, group: Group) -> Session {
        Session {
            id: id.into(),
            title: format!("title {id}"),
            group,
            last: None,
            waiting: false,
            failed: false,
            origin: None,
            stats: None,
        }
    }

    fn view() -> BgView {
        BgView::new(vec![
            session("done", Group::Completed),
            session("running", Group::Working),
            session("asking", Group::NeedsInput),
        ])
    }

    fn press(key: Key) -> KeyPress {
        KeyPress::plain(key)
    }

    fn typed(panel: &mut Panel, view: &BgView, text: &str) {
        for c in text.chars() {
            key(view, panel, KeyPress::ch(c));
        }
    }

    /// 停在「没干完」的那一刻说一次:之前就失败的、干完的、还在跑的都不说。
    #[test]
    fn a_session_that_stops_unfinished_is_named_once() {
        let failed = |id: &str| Session {
            failed: true,
            ..session(id, Group::NeedsInput)
        };
        let before = BgView::new(vec![session("a", Group::Working), failed("b")]);
        let after = BgView::new(vec![
            failed("a"),
            failed("b"),
            session("c", Group::Completed),
        ]);
        assert_eq!(
            after.newly_failed(&before),
            vec![(1, "title a".to_string())]
        );
        // 下一次列表变化时它还是失败的:不再说一遍。
        assert!(after.newly_failed(&after).is_empty());
    }

    /// 画的顺序是按组的,而 `/bg <N>` 的号按放进后台的先后——两者不能混。
    #[test]
    fn groups_order_the_panel_and_slots_keep_their_numbers() {
        let view = view();
        let ids: Vec<&str> = view.sessions().iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["asking", "running", "done"]);
        assert_eq!(view.slot_of("done"), Some(1));
        assert_eq!(view.slot_of("asking"), Some(3));
    }

    #[test]
    fn enter_opens_the_selected_one() {
        let view = view();
        let mut panel = Panel::new(None);
        key(&view, &mut panel, press(Key::Down));
        assert_eq!(
            key(&view, &mut panel, press(Key::Enter)),
            Step::Open {
                id: "running".into()
            }
        );
    }

    /// esc 回到刚移走的那个;从 `/bg list` 打开的就只是收起来。
    #[test]
    fn esc_returns_to_the_conversation_the_panel_was_opened_from() {
        let view = view();
        let mut moved = Panel::new(Some("done".into()));
        moved.settle(&view);
        assert_eq!(moved.cursor, 2, "光标落在刚移走的那个上");
        assert_eq!(
            key(&view, &mut moved, press(Key::Esc)),
            Step::Open { id: "done".into() }
        );
        let mut looking = Panel::new(None);
        assert_eq!(key(&view, &mut looking, press(Key::Esc)), Step::Close);
    }

    #[test]
    fn typing_a_task_and_enter_starts_one() {
        let view = view();
        let mut panel = Panel::new(None);
        typed(&mut panel, &view, "run the tests");
        assert_eq!(
            key(&view, &mut panel, press(Key::Enter)),
            Step::Start {
                task: "run the tests".into()
            }
        );
        assert!(panel.input.is_empty());
    }

    /// space 进入回复,回车发给选中的那个;esc 先退出回复,不收面板。
    #[test]
    fn space_replies_to_the_selected_one_in_place() {
        let view = view();
        let mut panel = Panel::new(None);
        key(&view, &mut panel, press(Key::Char(' ')));
        assert_eq!(panel.replying.as_deref(), Some("asking"));
        typed(&mut panel, &view, "yes go");
        assert_eq!(
            key(&view, &mut panel, press(Key::Enter)),
            Step::Tell {
                id: "asking".into(),
                text: "yes go".into()
            }
        );
        key(&view, &mut panel, press(Key::Char(' ')));
        assert_eq!(key(&view, &mut panel, press(Key::Esc)), Step::Stay);
        assert!(panel.replying.is_none());
    }

    /// 在等你回答的会话不收就地回复:space 不进入回复,只提示去打开它。
    #[test]
    fn space_on_a_waiting_session_does_not_reply() {
        let mut sessions = vec![session("asking", Group::NeedsInput)];
        sessions[0].waiting = true;
        let view = BgView::new(sessions);
        let mut panel = Panel::new(None);
        assert_eq!(key(&view, &mut panel, press(Key::Char(' '))), Step::Stay);
        assert!(panel.replying.is_none());
        assert!(panel.waiting_note);
        key(&view, &mut panel, press(Key::Down));
        assert!(!panel.waiting_note, "下一次按键就收起");
    }

    /// 点一行是选中,再点同一行才打开。
    #[test]
    fn a_click_selects_and_a_second_click_opens() {
        let view = view();
        let mut panel = Panel::new(None);
        assert_eq!(panel.click(&view, 1), Step::Stay);
        assert_eq!(panel.cursor, 1);
        assert_eq!(
            panel.click(&view, 1),
            Step::Open {
                id: "running".into()
            }
        );
        assert!(panel.wheel(&view, 5));
        assert_eq!(panel.cursor, 2, "滚轮夹在列表里");
    }

    /// ← on an empty box puts the panel away and stays on this session.
    #[test]
    fn left_on_an_empty_box_puts_the_panel_away() {
        let view = view();
        let mut panel = Panel::new(Some("done".into()));
        assert_eq!(key(&view, &mut panel, press(Key::Left)), Step::Close);
        typed(&mut panel, &view, "a");
        assert_eq!(key(&view, &mut panel, press(Key::Left)), Step::Stay);
    }

    /// 滚轮挪开了,待删也作废:那一行不能还说着「再按一次」而光标已经在别处。
    #[test]
    fn the_wheel_moving_off_ends_an_armed_drop() {
        let view = view();
        let mut panel = Panel::new(None);
        key(&view, &mut panel, KeyPress::ctrl('d'));
        assert!(panel.armed.is_some());
        assert!(panel.wheel(&view, 1));
        assert!(panel.armed.is_none());
    }

    /// 丢掉一个后台会话要按两次 ctrl+d:第一次只是标上,第二次才丢;中间按了
    /// 别的键(比如走到另一行),标记就没了,下一次 ctrl+d 标的是新的那一行。
    #[test]
    fn ctrl_d_twice_drops_the_selected_one() {
        let view = view();
        let mut panel = Panel::new(None);
        assert_eq!(key(&view, &mut panel, KeyPress::ctrl('d')), Step::Stay);
        assert_eq!(panel.armed.as_deref(), Some("asking"));
        assert_eq!(
            key(&view, &mut panel, KeyPress::ctrl('d')),
            Step::Drop {
                id: "asking".into()
            }
        );

        key(&view, &mut panel, KeyPress::ctrl('d'));
        key(&view, &mut panel, KeyPress::plain(Key::Down));
        assert!(panel.armed.is_none(), "moving off ends it");
        assert_eq!(
            key(&view, &mut panel, KeyPress::ctrl('d')),
            Step::Stay,
            "so this arms the new row rather than dropping it"
        );
        assert_eq!(
            key(
                &view,
                &mut panel,
                KeyPress {
                    key: Key::Char('x'),
                    mods: Mods::CTRL
                }
            ),
            Step::Stay,
            "ctrl+x no longer drops anything"
        );
    }

    #[test]
    fn question_mark_shows_the_keys_only_in_an_empty_box() {
        let view = view();
        let mut panel = Panel::new(None);
        key(&view, &mut panel, KeyPress::ch('?'));
        assert!(panel.keys);
        typed(&mut panel, &view, "why");
        key(&view, &mut panel, KeyPress::ch('?'));
        assert_eq!(panel.input, "why?", "写了字之后 ? 就是字");
    }
}
