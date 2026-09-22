//! 回退面板:这次会话走过的回合,挑一句话回到它之前。
//!
//! 和 `/config`、`/provider`、`/plugin`、`/toolbox` 是同一件事的第五块——人要在
//! 里面看一会儿、比较几个回合、再决定回到哪儿、连不连代码一起回——所以同一副骨架:
//! 从底下升起来占住输入框的位置,而不是一个选完就没的 overlay。
//!
//! **人怎么拉起它**:双击 Esc(`crate::moment::Moment::escape_again`),或者
//! `/rewind` 不带参数。两个入口落到同一个 [`crate::keymap::Action::ToggleRewind`]。
//!
//! **两步,不是一步**:先挑回到哪一句之前,再挑那一下把什么带回去。范围不是列表旁
//! 边的一个开关——「只回对话」和「连代码一起回」是两件后果不同的事,值得单独按一下
//! 回车。列表的最后一行是「当前」:停在那儿按回车什么都不会发生,这是人反悔的出口。
//!
//! 住在这儿的是**数据和按键**。回合是什么、回退怎么落下去,由 [`Rewind`] 端口出去
//! (`docs/adr/0022` §3):这个模块不知道有宿主控制契约这回事,也不知道一次回退在日志
//! 里是追加一条 `Rewound` 事实(`docs/adr/0024`)。
//!
//! 一件要紧的事:**代码回不去的时候不给「连代码一起回」这个选项**。按下去什么都不
//! 会发生的开关,比不给还糟——所以那一档画暗、选不中,并且说出为什么。

use crate::i18n::{t, Msg};
use crate::surface::{Key, KeyPress, Mods};

/// 工作区为什么回不去。
///
/// **是个分类,不是一句话。** 这三种是三件不同的事:一个是人自己能打开的开关,一个
/// 是这次会话的性质,一个是这台机器上的一次失败。话由屏幕按人选的语言说
/// (`atomcode_host_api::CodeUnavailable` 同形,分开一份的理由同 [`Point`]);只有第
/// 三种带着宿主的原话,因为那是关于这台机器的事实,不是一句可以预先写好的话。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodeOff {
    /// 这个构建默认不记工作区检查点(省磁盘),人可以打开。
    NotEnabled,
    /// 这次会话不落盘,没有可比对的检查点。
    NoSession,
    /// 开了,但没建起来——带着宿主说的原因。
    Failed(String),
}

impl CodeOff {
    /// 说给人听。
    pub fn say(&self) -> String {
        match self {
            Self::NotEnabled => t(Msg::RewindCodeNotEnabled),
            Self::NoSession => t(Msg::RewindCodeNoSession),
            Self::Failed(why) => t(Msg::RewindCodeFailed { why }),
        }
        .into_owned()
    }
}

/// 一个回合动过的一个文件,照面板需要的样子:`rewind.rs +484 -12`。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
}

/// 一个能回到的回合。与 `atomcode_host_api::RewindPoint` 同形,分开一份是因为这一
/// 层不认那个 crate(`docs/adr/0022` §3)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Point {
    pub turn: u64,
    /// 开这一回合的那句话,列表要的短版本。
    pub prompt: String,
    /// 这一回合动过的文件。空着就是「没有代码改动」,而那句话要说出来——列表里一条
    /// 没有第二行的条目,读起来是「还没算完」。
    pub changes: Vec<Change>,
    /// 工作区能不能退回到它之前。
    pub code: bool,
}

impl Point {
    /// 这一回合一共加了多少行、删了多少行。
    pub fn totals(&self) -> (u64, u64) {
        self.changes.iter().fold((0, 0), |(plus, minus), change| {
            (plus + change.additions, minus + change.deletions)
        })
    }
}

/// 一次回退把什么带回去。
///
/// **两档,不是三档。** 契约里还有「只回工作区」那一档(`/rewind N 代码` 仍然能
/// 说),面板不给:把文件退回去却留着讲它们的对话,得到的是一份和自己的历史对不上
/// 的工作区,而人点开这块面板要的是「刚才那下别算数」。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    /// 只有对话:说过的话回去,磁盘上的文件不动。
    #[default]
    Conversation,
    /// 对话和工作区一起回。
    Both,
}

impl Scope {
    /// 两档,按面板从上到下的顺序。
    pub const ALL: [Scope; 2] = [Scope::Conversation, Scope::Both];

    /// 说给人听的名字。用的是屏幕那张表里已有的三句,不另起一套措辞。
    pub fn about(self) -> String {
        match self {
            Self::Conversation => t(Msg::RewindScopeConversation),
            Self::Both => t(Msg::RewindScopeBoth),
        }
        .into_owned()
    }

    /// 这个范围要不要动工作区。只有对话的那一档不要。
    pub fn touches_code(self) -> bool {
        !matches!(self, Self::Conversation)
    }
}

/// 面板此刻问的是哪个问题。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stage {
    /// 回到哪一句之前。
    #[default]
    Points,
    /// 那一下把什么带回去。
    Scope,
}

/// 这次会话此刻能回到哪儿。面板开的时候问一次,回退落地之后再问一次——不按帧问,
/// 因为那是一次跨进程往返,而 `render` 必须是纯的。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RewindView {
    points: Vec<Point>,
    /// 工作区为什么整个儿回不去。`None` 就是回得去。
    code_unavailable: Option<CodeOff>,
}

impl RewindView {
    pub fn new(mut points: Vec<Point>, code_unavailable: Option<CodeOff>) -> Self {
        // 老的在上、新的在下,和对话本身同一个方向:人是顺着读下来的,而「当前」在
        // 最底下——列表读起来就是这次会话本身。
        points.sort_by_key(|point| point.turn);
        Self {
            points,
            code_unavailable,
        }
    }

    pub fn points(&self) -> &[Point] {
        &self.points
    }

    pub fn code_unavailable(&self) -> Option<&CodeOff> {
        self.code_unavailable.as_ref()
    }

    /// 光标停的那个回合。停在最后那行「当前」上时是 `None`——那一行不是回合,是
    /// 人反悔的出口。
    pub fn at(&self, panel: &Panel) -> Option<&Point> {
        self.points.get(panel.cursor)
    }

    /// 列表一共几行:回合,加上末尾那行「当前」。
    pub fn rows(&self) -> usize {
        self.points.len() + 1
    }

    /// 这一回合为什么不能连代码一起回,能的话是 `None`。
    ///
    /// 两个理由分开说:整棵树回不去(宿主给的原话)和这一回合本来就没动过文件,是两件
    /// 不同的事,人要据此做的决定也不同。
    pub fn code_why_not(&self, point: &Point) -> Option<String> {
        if let Some(why) = self.code_unavailable.as_ref() {
            return Some(why.say());
        }
        if !point.code || point.changes.is_empty() {
            return Some(t(Msg::RewindPanelTurnNoFiles).into_owned());
        }
        None
    }

    /// 这个范围此刻选不选得中,选不中的话为什么。
    pub fn scope_why_not(&self, panel: &Panel, scope: Scope) -> Option<String> {
        if !scope.touches_code() {
            return None;
        }
        self.code_why_not(self.at(panel)?)
    }
}

/// 有活在外面跑着:读回合、回退,都是一趟往返,回来之前屏上要有话说。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Busy {
    pub what: String,
}

impl Busy {
    /// 正在读这次会话走过的回合。
    pub fn reading() -> Self {
        Self {
            what: t(Msg::RewindPanelReading).trim().to_string(),
        }
    }
}

/// 回退面板,开着的时候。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Panel {
    /// 光标停在第几个回合。等于回合数时停在末尾那行「当前」上。
    pub cursor: usize,
    /// 第二步的光标:[`Scope::ALL`] 的第几档。
    pub scope: usize,
    pub stage: Stage,
    /// 上一次按键之后要说的一句话,比如「这一回合没改过文件」。
    pub note: Option<String>,
    pub busy: Option<Busy>,
}

impl Panel {
    /// 刚拉起来:回合还没读回来,所以它是「正在读」的样子——一块画着空列表的面板会
    /// 说「没有能回到的回合」,而那句话此刻是假的。
    pub fn new() -> Self {
        Self {
            busy: Some(Busy::reading()),
            ..Self::default()
        }
    }

    /// 读回来之后停在哪儿:最后一行「当前」。人是从现在往回看的,而停在「当前」上
    /// 的回车什么都不会发生——一块刚升起来就瞄准着某次回退的面板太容易走火。
    pub fn rest_at_current(&mut self, rows: usize) {
        self.cursor = rows.saturating_sub(1);
    }

    /// 指到第几行,夹在列出来的范围里。变了才返回 true。
    pub fn point_at(&mut self, row: usize, rows: usize) -> bool {
        let want = row.min(rows.saturating_sub(1));
        if self.cursor == want || self.stage != Stage::Points {
            return false;
        }
        self.cursor = want;
        self.note = None;
        true
    }
}

/// 一次按键要面板的主人去做的事。凡是碰得到外面世界的都在这儿,不在 [`key`] 里。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// 面板自己动了,外面没有。
    Stay,
    /// 收起来。
    Close,
    /// 回到 `turn` 之前,把 `scope` 说的那些带回去。
    Go { turn: u64, scope: Scope },
}

/// 一次按键。纯函数:面板改自己,要外面做的事从返回值出去。
pub fn key(view: &RewindView, panel: &mut Panel, press: KeyPress) -> Step {
    // 有活在跑:只认 Esc。回退是有副作用的一趟——别的键按下去会派出第二次,而第一次
    // 还没回来。
    if panel.busy.is_some() {
        return match (press.key, press.mods) {
            (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => {
                panel.busy = None;
                Step::Close
            }
            _ => Step::Stay,
        };
    }
    match panel.stage {
        Stage::Points => points_key(view, panel, press),
        Stage::Scope => scope_key(view, panel, press),
    }
}

/// 第一步:回到哪一句之前。
fn points_key(view: &RewindView, panel: &mut Panel, press: KeyPress) -> Step {
    match (press.key, press.mods) {
        (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => Step::Close,
        (Key::Up, _) | (Key::Char('p'), Mods::CTRL) => {
            panel.note = None;
            panel.cursor = panel.cursor.saturating_sub(1);
            Step::Stay
        }
        (Key::Down, _) | (Key::Char('n'), Mods::CTRL) => {
            panel.note = None;
            if panel.cursor + 1 < view.rows() {
                panel.cursor += 1;
            }
            Step::Stay
        }
        (Key::Enter, _) => {
            panel.note = None;
            // 停在「当前」上:这就是人反悔的出口,收起来,什么都不发生。
            let Some(point) = view.at(panel) else {
                return Step::Close;
            };
            // 第二步开在哪一档:能连代码一起回的时候开在「两样都回」——人点开回退
            // 面板通常是想把刚才那下整个儿撤掉;回不去的时候只有对话那一档。
            panel.scope = match view.code_why_not(point) {
                Some(_) => 0,
                None => Scope::ALL.len() - 1,
            };
            panel.stage = Stage::Scope;
            Step::Stay
        }
        _ => Step::Stay,
    }
}

/// 第二步:那一下把什么带回去。
fn scope_key(view: &RewindView, panel: &mut Panel, press: KeyPress) -> Step {
    match (press.key, press.mods) {
        // 回上一步,不是关面板:人到这儿是来挑范围的,挑错了该退回列表,而不是从头
        // 再把面板拉起来一次。
        (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => {
            panel.note = None;
            panel.stage = Stage::Points;
            Step::Stay
        }
        (Key::Up, _) | (Key::Char('p'), Mods::CTRL) => {
            panel.note = None;
            panel.scope = panel.scope.saturating_sub(1);
            Step::Stay
        }
        (Key::Down, _) | (Key::Char('n'), Mods::CTRL) => {
            panel.note = None;
            if panel.scope + 1 < Scope::ALL.len() {
                panel.scope += 1;
            }
            Step::Stay
        }
        (Key::Enter, _) => {
            panel.note = None;
            let Some(point) = view.at(panel).cloned() else {
                return Step::Close;
            };
            let scope = Scope::ALL[panel.scope.min(Scope::ALL.len() - 1)];
            // 选不中的那一档按下去什么都不发生,说出为什么——而不是默默换一档回退,
            // 那是拿人没要求过的事当成他要求的。
            if let Some(why) = view.scope_why_not(panel, scope) {
                panel.note = Some(why);
                return Step::Stay;
            }
            panel.busy = Some(Busy {
                what: t(Msg::RewindPanelGoing { turn: point.turn }).into_owned(),
            });
            Step::Go {
                turn: point.turn,
                scope,
            }
        }
        _ => Step::Stay,
    }
}

/// 一次回退落地之后,宿主说的话。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Done {
    /// 被撤回的那一轮里,人说过的话——回到人打字的地方去,改一改再发一次
    /// (`docs/adr/0024` §17)。
    pub prompt: Option<String>,
    /// 工作区被放回去了几个文件。
    pub files: usize,
}

/// 回合从哪儿来,回退往哪儿去。
///
/// 实现住在装配它的那一侧(cli),因为「这次会话走过哪些回合」是运行中那棵树的事,而
/// 屏幕不许伸手进 agent 的 App(`docs/adr/0022` §3)。
#[async_trait::async_trait]
pub trait Rewind: Send + Sync {
    /// 此刻能回到哪些回合。
    async fn points(&self) -> Result<RewindView, String>;

    /// 回到 `turn` 之前。答的是**发生过的事**,不是自己以为发生了的事。
    async fn rewind(&self, turn: u64, scope: Scope) -> Result<Done, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key: Key) -> KeyPress {
        KeyPress::plain(key)
    }

    fn changed(path: &str, additions: u64, deletions: u64) -> Change {
        Change {
            path: path.to_string(),
            additions,
            deletions,
        }
    }

    fn view() -> RewindView {
        RewindView::new(
            vec![
                Point {
                    turn: 1,
                    prompt: "写个解析器".into(),
                    changes: vec![changed("rewind.rs", 484, 12)],
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

    /// 读回来的样子:回合在上,「当前」在最底下,光标停在「当前」上。
    fn open(view: &RewindView) -> Panel {
        let mut panel = Panel {
            busy: None,
            ..Panel::new()
        };
        panel.rest_at_current(view.rows());
        panel
    }

    /// 列表和对话同一个方向:老的在上、新的在下,最后一行是「当前」。
    #[test]
    fn the_list_runs_the_way_the_conversation_does() {
        let view = view();
        assert_eq!(
            view.points().iter().map(|p| p.turn).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(view.rows(), 3, "两个回合,加上末尾那行「当前」");
    }

    /// 刚拉起来时是「正在读」:此刻列表还空着,而一块画着「没有能回到的回合」的面板
    /// 说的是假话。
    #[test]
    fn a_panel_just_opened_is_reading_rather_than_empty() {
        let panel = Panel::new();
        assert_eq!(panel.busy, Some(Busy::reading()));
    }

    /// 读回来之后停在「当前」上,回车什么都不会发生:一块刚升起来就瞄准着某次回退
    /// 的面板太容易走火。
    #[test]
    fn it_rests_on_current_where_enter_does_nothing() {
        let view = view();
        let mut panel = open(&view);
        assert!(view.at(&panel).is_none(), "「当前」不是一个回合");
        assert_eq!(key(&view, &mut panel, press(Key::Enter)), Step::Close);
        assert!(panel.busy.is_none(), "没派活");
    }

    /// 两步:先挑回到哪一句之前,再挑那一下把什么带回去。
    #[test]
    fn choosing_a_turn_then_a_scope_is_what_goes_out() {
        let view = view();
        let mut panel = open(&view);
        // 「当前」→ 第 2 回合 → 第 1 回合。
        key(&view, &mut panel, press(Key::Up));
        key(&view, &mut panel, press(Key::Up));
        assert_eq!(view.at(&panel).map(|p| p.turn), Some(1));
        assert_eq!(key(&view, &mut panel, press(Key::Enter)), Step::Stay);
        assert_eq!(panel.stage, Stage::Scope, "第一次回车进的是第二步");
        assert_eq!(
            Scope::ALL[panel.scope],
            Scope::Both,
            "代码回得去的时候,开在「两样都回」上"
        );
        assert_eq!(
            key(&view, &mut panel, press(Key::Enter)),
            Step::Go {
                turn: 1,
                scope: Scope::Both
            }
        );
        assert!(panel.busy.is_some(), "活派出去了,屏上要有话说");
    }

    /// **面板只给两档。** 「只回工作区」把文件退回去却留着讲它们的对话,得到的是一
    /// 份和自己的历史对不上的工作区——契约里还有那一档,面板不给。
    #[test]
    fn the_panel_offers_two_scopes_and_taking_only_the_files_back_is_not_one() {
        assert_eq!(Scope::ALL, [Scope::Conversation, Scope::Both]);
        let view = view();
        let mut panel = open(&view);
        key(&view, &mut panel, press(Key::Up));
        key(&view, &mut panel, press(Key::Up));
        key(&view, &mut panel, press(Key::Enter));
        // 走到底,再往下走不动:底下没有第三档。
        for _ in 0..5 {
            key(&view, &mut panel, press(Key::Down));
        }
        assert_eq!(
            key(&view, &mut panel, press(Key::Enter)),
            Step::Go {
                turn: 1,
                scope: Scope::Both
            },
            "最后一档是「对话与工作区」"
        );
    }

    /// 第二步按 Esc 回上一步,而不是把面板关掉:挑错了范围该退回列表。
    #[test]
    fn esc_in_the_second_step_goes_back_to_the_list() {
        let view = view();
        let mut panel = open(&view);
        key(&view, &mut panel, press(Key::Up));
        key(&view, &mut panel, press(Key::Enter));
        assert_eq!(panel.stage, Stage::Scope);
        assert_eq!(key(&view, &mut panel, press(Key::Esc)), Step::Stay);
        assert_eq!(panel.stage, Stage::Points, "回上一步");
        assert_eq!(key(&view, &mut panel, press(Key::Esc)), Step::Close);
    }

    /// 这一回合没改过文件:两档带代码的范围按下去什么都不发生,并且说出为什么。一个
    /// 选了之后按回车会默默换一档回退的面板,是拿人没要求过的事当成他要求的。
    #[test]
    fn a_turn_that_changed_no_file_refuses_to_put_code_back() {
        let view = view();
        let mut panel = open(&view);
        // 第 2 回合:没动过文件。
        key(&view, &mut panel, press(Key::Up));
        key(&view, &mut panel, press(Key::Enter));
        assert_eq!(
            Scope::ALL[panel.scope],
            Scope::Conversation,
            "代码回不去的时候,第二步开在「只回对话」上"
        );
        key(&view, &mut panel, press(Key::Down));
        assert_eq!(key(&view, &mut panel, press(Key::Enter)), Step::Stay);
        assert!(panel.busy.is_none(), "没派活");
        assert!(panel.note.is_some(), "说了为什么");
    }

    /// 整棵树回不去的时候(没有快照、不是 git 树),宿主给的原话要出现在面板上,而不是
    /// 让人以为自己按错了。
    #[test]
    fn a_workspace_that_cannot_go_back_says_the_hosts_own_words() {
        let view = RewindView::new(
            vec![Point {
                turn: 1,
                prompt: "写个解析器".into(),
                changes: vec![changed("rewind.rs", 484, 0)],
                code: true,
            }],
            Some(CodeOff::Failed("这棵树不是 git 仓库".into())),
        );
        let mut panel = open(&view);
        key(&view, &mut panel, press(Key::Up));
        key(&view, &mut panel, press(Key::Enter));
        key(&view, &mut panel, press(Key::Down));
        assert_eq!(key(&view, &mut panel, press(Key::Enter)), Step::Stay);
        assert!(
            panel.note.as_deref().unwrap_or_default().contains("git"),
            "宿主说的原话要传到人眼前: {:?}",
            panel.note
        );
    }

    /// 一条条目的第二行说的是这一回合动过什么,加删各自成账。
    #[test]
    fn a_turn_carries_what_it_changed() {
        let view = view();
        assert_eq!(view.points()[0].totals(), (484, 12));
    }

    /// 有活在跑的时候别的键一律吞掉:回退是有副作用的一趟,第二次按下去会派出第二次
    /// 回退,而第一次还没回来。
    #[test]
    fn nothing_but_esc_is_taken_while_a_rewind_is_out_there() {
        let view = view();
        let mut panel = Panel::new();
        assert_eq!(key(&view, &mut panel, press(Key::Enter)), Step::Stay);
        assert_eq!(key(&view, &mut panel, press(Key::Up)), Step::Stay);
        assert_eq!(panel.cursor, 0);
        assert_eq!(key(&view, &mut panel, press(Key::Esc)), Step::Close);
    }
}
