//! 插件与市场：人能装什么、装了什么、从哪装，以及写这些的端口。
//!
//! 和 [`crate::providers`]、[`crate::settings`] 是同一副骨架，理由也同一条
//! （`docs/adr/0022` §3）：**插件是什么** —— 哪些市场在册、每个市场带几个插件、
//! 哪些已经装上、装在哪个范围 —— 是产品那边读出来的数据；**怎么画、怎么走** 是
//! 这个 crate 的事。屏幕不知道市场是一次 `git clone`，不知道 `installed_plugins.json`
//! 这个文件名，也不该知道。
//!
//! 与那两块唯一不同的一件事：**这里的写是慢的**。改一行配置是写文件，装一个插件是
//! 克隆一个仓库——1 到 10 秒。所以端口是 async 的，面板多一个 [`Panel::busy`]：
//! 活派出去之后屏上要有话说，而人得能在等待里按 Esc 走人。取消不是「假装没发生」：
//! 活还在外面跑着，端口收到 [`Step::Cancel`] 记下它，落地时自己回滚——否则磁盘上
//! 会留下一个人已经放弃、却装好了的插件。
//!
//! 老前端把这块画成屏幕中央的一个模态，里面自带九个子屏
//! （`atomcode-tuix/src/modals/plugin_manager.rs`）。这里不是：它和它的两个
//! 姊妹一样从底下升起来占住输入框的位置，于是「人在里面干活的三块面板」是同一个
//! 形状、同一套键。

use std::sync::Arc;

use crate::surface::{Key, KeyPress, Mods};

/// 装到哪里去。
///
/// 是个枚举而不是字符串：屏幕要把三个去处并排画出来让人选，所以它得知道一共有
/// 哪几个。至于每一个在磁盘上是哪个目录，端口知道，屏幕不知道。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// 这台机器上处处可用。
    User,
    /// 跟着这个项目走，会提交进 git。
    Project,
    /// 只在这个项目、只对自己，不进 git。
    Local,
}

impl Scope {
    /// 三个去处，画出来的顺序。
    pub const ALL: [Scope; 3] = [Scope::User, Scope::Project, Scope::Local];

    pub fn label(self) -> &'static str {
        match self {
            Scope::User => "这台机器",
            Scope::Project => "这个项目",
            Scope::Local => "只有自己",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            Scope::User => "装进 ~/.atomcode/plugins,哪个项目都能用",
            Scope::Project => "装进 .atomcode/plugins,跟着仓库走、同事也有",
            Scope::Local => "装进 .atomcode/plugins/local,不进 git,只有自己有",
        }
    }

    /// 已装行后面那个短标。
    pub fn short(self) -> &'static str {
        match self {
            Scope::User => "机器",
            Scope::Project => "项目",
            Scope::Local => "自己",
        }
    }
}

/// 一个插件，如启动器所见。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginRow {
    /// 插件名——装它、卸它都用这个。
    pub name: String,
    /// 它属于哪个市场。同名插件可以来自两个市场，所以这两个字段合起来才是身份。
    pub marketplace: String,
    /// 一句话说它是干嘛的。市场清单里没写就是空的。
    pub description: String,
    /// 已经装上了就是它装在哪个范围;`None` 是还没装。
    pub installed: Option<Scope>,
}

impl PluginRow {
    /// 装它、卸它、取消它，用的都是这一个名字。
    pub fn id(&self) -> String {
        format!("{}@{}", self.name, self.marketplace)
    }
}

/// 一个市场。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketRow {
    pub name: String,
    /// 从哪儿克隆来的。
    pub source: String,
    /// 它带着几个插件。
    pub plugins: usize,
    /// 其中已经装上的有几个。
    pub installed: usize,
    /// 上一次更新是什么时候——**已经排好版的一句话**。屏幕不懂时间戳,也不该懂
    /// 人所在的时区。
    pub updated: String,
    /// 这个构建自带的市场。删不掉:下一次启动它自己会回来,而中间那段时间人会
    /// 以为自己删掉了什么东西。
    pub official: bool,
}

/// 插件与市场,截至这一帧。
///
/// 和 [`crate::providers::ProvidersView`] 一样是不可变、克隆便宜的:一个
/// `Moment` 带着它,同一帧里画两次看到的是同一份。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginsView {
    plugins: Arc<Vec<PluginRow>>,
    markets: Arc<Vec<MarketRow>>,
}

impl PluginsView {
    pub fn new(mut plugins: Vec<PluginRow>, markets: Vec<MarketRow>) -> Self {
        // 一处排序,在这里:两个页签走的是同一份清单,排序若交给端口,两个端口就会
        // 有两种顺序。
        plugins.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.marketplace.cmp(&b.marketplace))
        });
        Self {
            plugins: Arc::new(plugins),
            markets: Arc::new(markets),
        }
    }

    pub fn plugins(&self) -> &[PluginRow] {
        &self.plugins
    }

    pub fn markets(&self) -> &[MarketRow] {
        &self.markets
    }

    /// 已经装上的有几个——页签上那个数字。
    pub fn installed_count(&self) -> usize {
        self.plugins
            .iter()
            .filter(|p| p.installed.is_some())
            .count()
    }

    pub fn plugin(&self, name: &str, marketplace: &str) -> Option<&PluginRow> {
        self.plugins
            .iter()
            .find(|p| p.name == name && p.marketplace == marketplace)
    }

    pub fn market(&self, name: &str) -> Option<&MarketRow> {
        self.markets.iter().find(|m| m.name == name)
    }

    /// 这一页此刻列着哪些行——画、点、走光标,都读它这一份。
    ///
    /// 和 provider 面板同一条理由:三处各数一遍清单,迟早数出三个不一样的长度,
    /// 而症状是光标停在一行不存在的东西上。
    pub fn listed(&self, panel: &Panel) -> Vec<Listed> {
        let q = panel.query.to_lowercase();
        let hit = |haystack: &[&str]| {
            q.is_empty()
                || haystack
                    .iter()
                    .any(|s| s.to_lowercase().contains(q.as_str()))
        };
        match panel.tab {
            Tab::All => self
                .plugins
                .iter()
                .enumerate()
                .filter(|(_, p)| {
                    hit(&[
                        p.name.as_str(),
                        p.marketplace.as_str(),
                        p.description.as_str(),
                    ])
                })
                .map(|(i, _)| Listed::Plugin(i))
                .collect(),
            Tab::Installed => self
                .plugins
                .iter()
                .enumerate()
                .filter(|(_, p)| p.installed.is_some())
                .filter(|(_, p)| {
                    hit(&[
                        p.name.as_str(),
                        p.marketplace.as_str(),
                        p.description.as_str(),
                    ])
                })
                .map(|(i, _)| Listed::Plugin(i))
                .collect(),
            Tab::Markets => {
                // 加市场那一行排在最前,而且**不参与过滤**:它是这一页唯一的出路,
                // 一个筛空了的市场页若连它都没有,人就只剩 Esc 可按。
                let mut out = vec![Listed::AddMarket];
                out.extend(
                    self.markets
                        .iter()
                        .enumerate()
                        .filter(|(_, m)| hit(&[m.name.as_str(), m.source.as_str()]))
                        .map(|(i, _)| Listed::Market(i)),
                );
                out
            }
        }
    }
}

/// 列表里的一行,走它的人看到的样子。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listed {
    /// 索引进 [`PluginsView::plugins`]。
    Plugin(usize),
    /// 索引进 [`PluginsView::markets`]。
    Market(usize),
    /// 市场页的第一行:加一个。
    AddMarket,
}

/// 哪一页在前面。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    /// 在册市场里能装的全部。
    #[default]
    All,
    /// 已经装上的。
    Installed,
    /// 市场本身。
    Markets,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::All, Tab::Installed, Tab::Markets];

    pub fn label(self) -> &'static str {
        match self {
            Tab::All => "全部",
            Tab::Installed => "已装",
            Tab::Markets => "市场",
        }
    }

    fn at(self) -> usize {
        match self {
            Tab::All => 0,
            Tab::Installed => 1,
            Tab::Markets => 2,
        }
    }

    fn step(self, by: i32) -> Tab {
        let n = Tab::ALL.len() as i32;
        let at = (self.at() as i32 + by).rem_euclid(n);
        Tab::ALL[at as usize]
    }
}

/// 给一个还没装的插件选去处。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeForm {
    pub plugin: String,
    pub marketplace: String,
    /// 光标停在 [`Scope::ALL`] 的第几个。
    pub at: usize,
}

/// 一个已经装上的插件能做的两件事。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginAction {
    /// 重新装一遍:先卸干净再装,这样市场那边改了什么都能跟上。
    Update,
    Uninstall,
}

impl PluginAction {
    pub const ALL: [PluginAction; 2] = [PluginAction::Update, PluginAction::Uninstall];

    pub fn label(self) -> &'static str {
        match self {
            PluginAction::Update => "更新",
            PluginAction::Uninstall => "卸载",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            PluginAction::Update => "从市场再取一遍,卸掉旧的装上新的",
            PluginAction::Uninstall => "拿掉它,它带来的技能和钩子一并没有",
        }
    }
}

/// 一个已经装上的插件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginForm {
    pub plugin: String,
    pub marketplace: String,
    pub scope: Scope,
    pub at: usize,
}

/// 一个市场能做的三件事。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarketAction {
    /// 只看它带的插件——把过滤框填成市场名,回到全部页。
    Browse,
    /// `git pull` 一次。
    Update,
    Remove,
}

impl MarketAction {
    pub fn label(self) -> &'static str {
        match self {
            MarketAction::Browse => "看它带的插件",
            MarketAction::Update => "更新",
            MarketAction::Remove => "删掉这个市场",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            MarketAction::Browse => "回到全部页,只留它的",
            MarketAction::Update => "再拉一次,看它有没有新插件",
            MarketAction::Remove => "连同从它装的插件一起拿掉",
        }
    }
}

/// 一个市场。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketForm {
    pub name: String,
    /// 官方市场少一行——删不掉的东西不该列出来让人按。
    pub official: bool,
    pub at: usize,
}

impl MarketForm {
    pub fn actions(&self) -> Vec<MarketAction> {
        let mut out = vec![MarketAction::Browse, MarketAction::Update];
        if !self.official {
            out.push(MarketAction::Remove);
        }
        out
    }
}

/// 加一个市场:一个地址。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AddMarketForm {
    pub url: String,
    /// 字节光标。和 provider 的表单一样,永远落在字符边界上。
    pub caret: usize,
}

/// 屏上那张表单,有的话。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Form {
    Scope(ScopeForm),
    Plugin(PluginForm),
    Market(MarketForm),
    AddMarket(AddMarketForm),
}

/// 派出去还没回来的那件活。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Busy {
    /// 屏上那句话:`正在装 xxx…`。
    pub what: String,
    /// 谁在跑——取消时报给端口的就是它。
    pub job: String,
}

/// 插件面板,开着的时候。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Panel {
    pub tab: Tab,
    /// 光标停在 *listed* 的第几行。
    pub cursor: usize,
    /// 过滤框里打了什么。
    pub query: String,
    /// 再按一次 ctrl-d 就会拿掉的那一行。
    ///
    /// 两下而不是一下,和 provider 面板同一条理由:这是这块面板上唯一会扔掉东西的
    /// 手势,一按就生效的键是人去按别的键路上会误触的键。
    pub pending_delete: Option<String>,
    pub form: Option<Form>,
    /// 有活在外面跑着。跑着的时候除了 Esc 什么都不收——否则人会在一次克隆还没
    /// 落地时按出第二次。
    pub busy: Option<Busy>,
}

impl Panel {
    pub fn new() -> Self {
        Self::default()
    }

    /// 换一页:从头开始,过滤框清空。
    ///
    /// 清空而不是留着,和 provider 面板同一条理由:三页列的不是同一种东西,按插件名
    /// 打的字拿到市场页上会把整页筛没,而一个因为别处打的字而空掉的列表,看上去就是
    /// 坏了。
    pub fn show(&mut self, tab: Tab) -> bool {
        if self.tab == tab && self.query.is_empty() && self.form.is_none() {
            return false;
        }
        self.tab = tab;
        self.cursor = 0;
        self.query.clear();
        self.pending_delete = None;
        self.form = None;
        true
    }

    /// 只留某个市场的插件:市场详情里「看它带的插件」走的就是这条。
    pub fn only(&mut self, marketplace: &str) {
        self.tab = Tab::All;
        self.query = marketplace.to_string();
        self.cursor = 0;
        self.pending_delete = None;
        self.form = None;
    }

    /// 指到第几行,夹在列出来的范围里。变了才返回 true。
    pub fn point_at(&mut self, row: usize, rows: usize) -> bool {
        let want = row.min(rows.saturating_sub(1));
        if self.cursor == want {
            return false;
        }
        self.cursor = want;
        self.pending_delete = None;
        true
    }
}

/// 一次按键要面板的主人去做的事。
///
/// 凡是碰得到外面世界的都在这儿,而不在 [`key`] 里,理由同
/// `crate::providers::Step`:值得脱开屏幕去测的是分支,而写只有调用方够得着。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// 面板自己动了,外面没有。
    Stay,
    /// 收起来。
    Close,
    Install {
        plugin: String,
        marketplace: String,
        scope: Scope,
    },
    /// 先卸后装。
    Update {
        plugin: String,
        marketplace: String,
        scope: Scope,
    },
    Uninstall {
        plugin: String,
        marketplace: String,
        scope: Scope,
    },
    AddMarket {
        url: String,
    },
    UpdateMarket {
        name: String,
    },
    /// 连同从它装的插件一起。
    RemoveMarket {
        name: String,
    },
    /// 人不等了。活还在外面跑着,落地时归端口收拾。
    Cancel {
        job: String,
    },
}

/// 跑一次按键。
///
/// 自由函数、纯的,和 `crate::providers::key` 一样,这样有分支的那一半可以只拿一个
/// view 和一个 panel 去测,不要屏幕。
pub fn key(view: &PluginsView, panel: &mut Panel, press: KeyPress) -> Step {
    // 有活在跑:只认 Esc。别的键一律吞掉——不是没接,是此刻按下去的每一个都会
    // 派出第二件活,而第一件还在克隆。
    if let Some(busy) = panel.busy.clone() {
        return match (press.key, press.mods) {
            (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => {
                panel.busy = None;
                Step::Cancel { job: busy.job }
            }
            _ => Step::Stay,
        };
    }
    match panel.form.clone() {
        Some(Form::Scope(form)) => scope_key(panel, form, press),
        Some(Form::Plugin(form)) => plugin_key(panel, form, press),
        Some(Form::Market(form)) => market_key(panel, form, press),
        Some(Form::AddMarket(form)) => add_market_key(panel, form, press),
        None => list_key(view, panel, press),
    }
}

/// 粘贴落进正在打字的那个字段。
///
/// 只有加市场那张表单有字段,而地址正是人从浏览器里拷来的那种东西——没有这一条,
/// 粘贴会落到面板背后的输入框里:一个看不见的草稿,而面板一关它就成了一句发给
/// 模型的话。
///
/// **只取一行。** 字段是一行,而从网页上拷来的地址后面常跟着一个换行。
///
/// 变了才返回 true。没有表单时返回 false,让调用方照旧走:列表上的粘贴是往过滤框里
/// 粘,那是搜索。
pub fn paste(panel: &mut Panel, text: &str) -> bool {
    if panel.busy.is_some() {
        return false;
    }
    match panel.form.as_mut() {
        Some(Form::AddMarket(form)) => {
            let clean: String = text
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .chars()
                .filter(|c| !c.is_control())
                .collect();
            if clean.is_empty() {
                return false;
            }
            let at = snap(&form.url, form.caret);
            form.url.insert_str(at, &clean);
            form.caret = at + clean.len();
            true
        }
        Some(_) => false,
        None => {
            let clean: String = text
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .chars()
                .filter(|c| !c.is_control())
                .collect();
            if clean.is_empty() {
                return false;
            }
            panel.query.push_str(&clean);
            panel.cursor = 0;
            true
        }
    }
}

fn list_key(view: &PluginsView, panel: &mut Panel, press: KeyPress) -> Step {
    let listed = view.listed(panel);
    let at = listed.get(panel.cursor).copied();
    let armed = panel.pending_delete.take();
    match (press.key, press.mods) {
        (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => Step::Close,
        (Key::Tab, _) => {
            panel.show(panel.tab.step(1));
            Step::Stay
        }
        (Key::BackTab, _) => {
            panel.show(panel.tab.step(-1));
            Step::Stay
        }
        (Key::Left, _) => {
            panel.show(panel.tab.step(-1));
            Step::Stay
        }
        (Key::Right, _) => {
            panel.show(panel.tab.step(1));
            Step::Stay
        }
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
        (Key::PageUp, _) => {
            panel.cursor = panel.cursor.saturating_sub(10);
            Step::Stay
        }
        (Key::PageDown, _) => {
            panel.cursor = (panel.cursor + 10).min(listed.len().saturating_sub(1));
            Step::Stay
        }
        // 加一个市场。ctrl-a 而不是一个字母:字母都是过滤框的。
        (Key::Char('a'), Mods::CTRL) => {
            panel.form = Some(Form::AddMarket(AddMarketForm::default()));
            Step::Stay
        }
        (Key::Char('d'), Mods::CTRL) => delete_key(view, panel, at, armed),
        (Key::Enter, _) => enter(view, panel, at),
        (Key::Backspace, _) => {
            let at = snap(&panel.query, panel.query.len());
            let _ = at;
            panel.query.pop();
            panel.cursor = 0;
            Step::Stay
        }
        (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
            panel.query.push(c);
            panel.cursor = 0;
            Step::Stay
        }
        _ => Step::Stay,
    }
}

/// 在列表上按回车。
fn enter(view: &PluginsView, panel: &mut Panel, at: Option<Listed>) -> Step {
    match at {
        Some(Listed::AddMarket) => {
            panel.form = Some(Form::AddMarket(AddMarketForm::default()));
            Step::Stay
        }
        Some(Listed::Market(i)) => {
            let Some(row) = view.markets().get(i) else {
                return Step::Stay;
            };
            panel.form = Some(Form::Market(MarketForm {
                name: row.name.clone(),
                official: row.official,
                at: 0,
            }));
            Step::Stay
        }
        Some(Listed::Plugin(i)) => {
            let Some(row) = view.plugins().get(i) else {
                return Step::Stay;
            };
            panel.form = Some(match row.installed {
                // 已经装上的:能更新、能卸。
                Some(scope) => Form::Plugin(PluginForm {
                    plugin: row.name.clone(),
                    marketplace: row.marketplace.clone(),
                    scope,
                    at: 0,
                }),
                // 还没装的:先问装到哪儿去。
                None => Form::Scope(ScopeForm {
                    plugin: row.name.clone(),
                    marketplace: row.marketplace.clone(),
                    at: 0,
                }),
            });
            Step::Stay
        }
        None => Step::Stay,
    }
}

/// 两下的删除。第一下把那一行架起来,第二下才真去问。
fn delete_key(
    view: &PluginsView,
    panel: &mut Panel,
    at: Option<Listed>,
    armed: Option<String>,
) -> Step {
    match at {
        Some(Listed::Plugin(i)) => {
            let Some(row) = view.plugins().get(i) else {
                return Step::Stay;
            };
            // 没装的没什么可卸。
            let Some(scope) = row.installed else {
                return Step::Stay;
            };
            let id = row.id();
            if armed.as_deref() != Some(id.as_str()) {
                panel.pending_delete = Some(id);
                return Step::Stay;
            }
            Step::Uninstall {
                plugin: row.name.clone(),
                marketplace: row.marketplace.clone(),
                scope,
            }
        }
        Some(Listed::Market(i)) => {
            let Some(row) = view.markets().get(i) else {
                return Step::Stay;
            };
            if row.official {
                return Step::Stay;
            }
            let id = format!("market:{}", row.name);
            if armed.as_deref() != Some(id.as_str()) {
                panel.pending_delete = Some(id);
                return Step::Stay;
            }
            Step::RemoveMarket {
                name: row.name.clone(),
            }
        }
        Some(Listed::AddMarket) | None => Step::Stay,
    }
}

fn scope_key(panel: &mut Panel, form: ScopeForm, press: KeyPress) -> Step {
    match (press.key, press.mods) {
        (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => {
            panel.form = None;
            Step::Stay
        }
        (Key::Up, _) => {
            let at = form.at.saturating_sub(1);
            panel.form = Some(Form::Scope(ScopeForm { at, ..form }));
            Step::Stay
        }
        (Key::Down, _) => {
            let at = (form.at + 1).min(Scope::ALL.len() - 1);
            panel.form = Some(Form::Scope(ScopeForm { at, ..form }));
            Step::Stay
        }
        (Key::Enter, _) => {
            let scope = Scope::ALL[form.at.min(Scope::ALL.len() - 1)];
            panel.form = None;
            Step::Install {
                plugin: form.plugin,
                marketplace: form.marketplace,
                scope,
            }
        }
        _ => Step::Stay,
    }
}

fn plugin_key(panel: &mut Panel, form: PluginForm, press: KeyPress) -> Step {
    match (press.key, press.mods) {
        (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => {
            panel.form = None;
            Step::Stay
        }
        (Key::Up, _) => {
            let at = form.at.saturating_sub(1);
            panel.form = Some(Form::Plugin(PluginForm { at, ..form }));
            Step::Stay
        }
        (Key::Down, _) => {
            let at = (form.at + 1).min(PluginAction::ALL.len() - 1);
            panel.form = Some(Form::Plugin(PluginForm { at, ..form }));
            Step::Stay
        }
        (Key::Enter, _) => {
            let action = PluginAction::ALL[form.at.min(PluginAction::ALL.len() - 1)];
            panel.form = None;
            match action {
                PluginAction::Update => Step::Update {
                    plugin: form.plugin,
                    marketplace: form.marketplace,
                    scope: form.scope,
                },
                PluginAction::Uninstall => Step::Uninstall {
                    plugin: form.plugin,
                    marketplace: form.marketplace,
                    scope: form.scope,
                },
            }
        }
        _ => Step::Stay,
    }
}

fn market_key(panel: &mut Panel, form: MarketForm, press: KeyPress) -> Step {
    let actions = form.actions();
    match (press.key, press.mods) {
        (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => {
            panel.form = None;
            Step::Stay
        }
        (Key::Up, _) => {
            let at = form.at.saturating_sub(1);
            panel.form = Some(Form::Market(MarketForm { at, ..form }));
            Step::Stay
        }
        (Key::Down, _) => {
            let at = (form.at + 1).min(actions.len() - 1);
            panel.form = Some(Form::Market(MarketForm { at, ..form }));
            Step::Stay
        }
        (Key::Enter, _) => {
            let action = actions[form.at.min(actions.len() - 1)];
            panel.form = None;
            match action {
                MarketAction::Browse => {
                    panel.only(&form.name);
                    Step::Stay
                }
                MarketAction::Update => Step::UpdateMarket { name: form.name },
                MarketAction::Remove => Step::RemoveMarket { name: form.name },
            }
        }
        _ => Step::Stay,
    }
}

fn add_market_key(panel: &mut Panel, mut form: AddMarketForm, press: KeyPress) -> Step {
    match (press.key, press.mods) {
        (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => {
            panel.form = None;
            Step::Stay
        }
        (Key::Enter, _) => {
            let url = form.url.trim().to_string();
            if url.is_empty() {
                // 空地址不派活,表单也不关:人正打算打字,而一个因为按早了而消失的
                // 表单要重开一次。
                panel.form = Some(Form::AddMarket(form));
                return Step::Stay;
            }
            panel.form = None;
            Step::AddMarket { url }
        }
        (Key::Left, _) => {
            form.caret = back(&form.url, form.caret);
            panel.form = Some(Form::AddMarket(form));
            Step::Stay
        }
        (Key::Right, _) => {
            form.caret = forward(&form.url, form.caret);
            panel.form = Some(Form::AddMarket(form));
            Step::Stay
        }
        (Key::Home, _) => {
            form.caret = 0;
            panel.form = Some(Form::AddMarket(form));
            Step::Stay
        }
        (Key::End, _) => {
            form.caret = form.url.len();
            panel.form = Some(Form::AddMarket(form));
            Step::Stay
        }
        (Key::Backspace, _) => {
            take_before(&mut form.url, &mut form.caret);
            panel.form = Some(Form::AddMarket(form));
            Step::Stay
        }
        (Key::Delete, _) => {
            take_at(&mut form.url, form.caret);
            panel.form = Some(Form::AddMarket(form));
            Step::Stay
        }
        (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
            let at = snap(&form.url, form.caret);
            form.url.insert(at, c);
            form.caret = at + c.len_utf8();
            panel.form = Some(Form::AddMarket(form));
            Step::Stay
        }
        _ => Step::Stay,
    }
}

/// `at`,退回字符边界上。
///
/// 每条移动光标的路都把它留在边界上;这是给切片那几个函数系的第二道保险——一个落在
/// 多字节字符中间的字节偏移会当场 panic。
fn snap(s: &str, at: usize) -> usize {
    let at = at.min(s.len());
    if s.is_char_boundary(at) {
        return at;
    }
    (0..=at).rev().find(|i| s.is_char_boundary(*i)).unwrap_or(0)
}

/// The boundary before `at`.
///
/// Walked over `char_indices` rather than sliced: a byte slice here would be
/// one more place that has to prove it lands on a boundary, and this file's job
/// is to never make that mistake (`gates/tui-string-slice.sh`).
fn back(s: &str, at: usize) -> usize {
    let at = snap(s, at);
    s.char_indices()
        .map(|(i, _)| i)
        .filter(|i| *i < at)
        .next_back()
        .unwrap_or(0)
}

/// The boundary after `at`.
fn forward(s: &str, at: usize) -> usize {
    let at = snap(s, at);
    s.char_indices()
        .map(|(i, _)| i)
        .find(|i| *i > at)
        .unwrap_or(s.len())
}

fn take_before(s: &mut String, at: &mut usize) {
    let here = snap(s, *at);
    let prev = back(s, here);
    if prev < here {
        s.replace_range(prev..here, "");
        *at = prev;
    }
}

fn take_at(s: &mut String, at: usize) {
    let here = snap(s, at);
    let next = forward(s, here);
    if next > here {
        s.replace_range(here..next, "");
    }
}

/// 读插件,和改插件。
///
/// 由启动屏幕的那一方填,和 `crate::providers::Providers` 同一个形状、同一条
/// 理由:屏幕知道的产品信息是从缝里过来的,不是从产品自己的服务里拿的
/// （`docs/adr/0022` §3）。
///
/// **每一次写都是 async 的**,因为每一次写都是一次 `git`。回来的 `Ok` 是一句给人
/// 看的话,`Err` 是启动器的拒绝,原样画出来——屏幕不知道一个市场地址长什么样才算
/// 对,重新措辞就是瞎编。
#[async_trait::async_trait]
pub trait Plugins: Send + Sync {
    /// 此刻磁盘上是什么样。读本地几个小文件,同步。
    fn rows(&self) -> PluginsView;

    async fn install(
        &self,
        plugin: &str,
        marketplace: &str,
        scope: Scope,
    ) -> Result<String, String>;

    /// 先卸后装。分开一条而不是让调用方连着发两次,因为「更新」是一件事:中途失败
    /// 了该说的是「更新没成」,而不是「卸好了」加一句「装不上」。
    async fn update(&self, plugin: &str, marketplace: &str, scope: Scope)
        -> Result<String, String>;

    async fn uninstall(
        &self,
        plugin: &str,
        marketplace: &str,
        scope: Scope,
    ) -> Result<String, String>;

    async fn add_market(&self, url: &str) -> Result<String, String>;

    async fn update_market(&self, name: &str) -> Result<String, String>;

    /// 连同从它装的插件一起拿掉。
    async fn remove_market(&self, name: &str) -> Result<String, String>;

    /// 人不等这件活了。活还在跑,落地时端口自己收拾——否则磁盘上会留下一个人已经
    /// 放弃、却装好了的插件。
    fn cancel(&self, _job: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::{Key, Mods};

    fn press(key: Key) -> KeyPress {
        KeyPress::plain(key)
    }

    fn ctrl(c: char) -> KeyPress {
        KeyPress::new(Key::Char(c), Mods::CTRL)
    }

    fn typed(c: char) -> KeyPress {
        KeyPress::new(Key::Char(c), Mods::NONE)
    }

    fn plugin(name: &str, market: &str, about: &str, installed: Option<Scope>) -> PluginRow {
        PluginRow {
            name: name.into(),
            marketplace: market.into(),
            description: about.into(),
            installed,
        }
    }

    fn market(name: &str, plugins: usize, installed: usize, official: bool) -> MarketRow {
        MarketRow {
            name: name.into(),
            source: format!("https://example.com/{name}.git"),
            plugins,
            installed,
            updated: "今天更新".into(),
            official,
        }
    }

    fn view() -> PluginsView {
        PluginsView::new(
            vec![
                plugin("lens", "official", "看一眼改了什么", Some(Scope::User)),
                plugin("tidy", "official", "把代码排整齐", None),
                plugin("lens", "mine", "另一个同名的", None),
            ],
            vec![market("official", 2, 1, true), market("mine", 1, 0, false)],
        )
    }

    /// What is listed, in words — the rows are sorted by name, so asserting on
    /// indices would be asserting on the sort rather than on the filter.
    fn shown(view: &PluginsView, panel: &Panel) -> Vec<String> {
        view.listed(panel)
            .into_iter()
            .map(|what| match what {
                Listed::Plugin(i) => view.plugins()[i].id(),
                Listed::Market(i) => format!("market:{}", view.markets()[i].name),
                Listed::AddMarket => "+market".to_string(),
            })
            .collect()
    }

    /// Move the cursor onto the row with this id, the way a person walks to it.
    fn walk_to(view: &PluginsView, panel: &mut Panel, id: &str) {
        let at = shown(view, panel)
            .iter()
            .position(|row| row == id)
            .unwrap_or_else(|| panic!("`{id}` is listed"));
        panel.cursor = at;
    }

    fn run(view: &PluginsView, panel: &mut Panel, keys: &[KeyPress]) -> Step {
        let mut last = Step::Stay;
        for key in keys {
            last = super::key(view, panel, *key);
        }
        last
    }

    /// The filter reads everything a person can see on the row, which is why a
    /// marketplace name and a description are matched as well as the name: the
    /// row says `tidy · @official · 把代码排整齐`, and every word of it is
    /// something somebody will type looking for it.
    #[test]
    fn the_filter_matches_what_a_person_can_see() {
        let view = view();
        let mut panel = Panel::new();
        panel.query = "mine".into();
        assert_eq!(shown(&view, &panel), ["lens@mine"], "by market");
        panel.query = "排整齐".into();
        assert_eq!(shown(&view, &panel), ["tidy@official"], "by what it does");
        panel.query = "lens".into();
        assert_eq!(
            shown(&view, &panel),
            ["lens@mine", "lens@official"],
            "two marketplaces carrying one name are two rows, not one"
        );
    }

    /// The installed page lists only what is installed, whatever is typed.
    #[test]
    fn the_installed_page_is_only_what_is_installed() {
        let view = view();
        let mut panel = Panel::new();
        panel.tab = Tab::Installed;
        assert_eq!(shown(&view, &panel), ["lens@official"]);
        panel.query = "tidy".into();
        assert!(
            view.listed(&panel).is_empty(),
            "a plugin that is not installed does not appear here because it was searched for"
        );
    }

    /// The way out of the marketplaces page never filters away.
    ///
    /// Written because it is the one row that has to survive a query matching
    /// nothing: a person whose filter emptied the page would otherwise have no
    /// key but Esc, and the thing they came to do is add the marketplace that
    /// would have matched.
    #[test]
    fn adding_a_marketplace_is_always_reachable() {
        let view = view();
        let mut panel = Panel::new();
        panel.tab = Tab::Markets;
        assert_eq!(
            shown(&view, &panel),
            ["+market", "market:official", "market:mine"]
        );
        panel.query = "nothing matches this".into();
        assert_eq!(shown(&view, &panel), ["+market"]);
    }

    /// Enter on a plugin asks where to put it; Enter on an installed one offers
    /// the two things that can still be done to it.
    #[test]
    fn enter_asks_where_to_install_and_what_to_do_with_an_installed_one() {
        let view = view();
        let mut panel = Panel::new();
        walk_to(&view, &mut panel, "tidy@official");
        run(&view, &mut panel, &[press(Key::Enter)]);
        let Some(Form::Scope(form)) = panel.form.clone() else {
            panic!("a plugin that is not installed asks where it should go");
        };
        assert_eq!(
            (form.plugin.as_str(), form.marketplace.as_str()),
            ("tidy", "official")
        );
        // And picking the second of the three sends the install with that scope.
        let step = run(&view, &mut panel, &[press(Key::Down), press(Key::Enter)]);
        assert_eq!(
            step,
            Step::Install {
                plugin: "tidy".into(),
                marketplace: "official".into(),
                scope: Scope::Project,
            }
        );
        assert!(
            panel.form.is_none(),
            "the form is gone before the job goes out"
        );

        // And the one that is installed offers the two things left to do.
        let mut panel = Panel::new();
        walk_to(&view, &mut panel, "lens@official");
        run(&view, &mut panel, &[press(Key::Enter)]);
        let Some(Form::Plugin(form)) = panel.form.clone() else {
            panic!("an installed plugin offers update and uninstall");
        };
        assert_eq!(form.scope, Scope::User);
        let step = run(&view, &mut panel, &[press(Key::Enter)]);
        assert_eq!(
            step,
            Step::Update {
                plugin: "lens".into(),
                marketplace: "official".into(),
                scope: Scope::User,
            }
        );
    }

    /// Throwing something away takes two presses, and the first one only arms
    /// the row it is on.
    #[test]
    fn uninstalling_takes_two_presses() {
        let view = view();
        let mut panel = Panel::new();
        panel.tab = Tab::Installed;
        walk_to(&view, &mut panel, "lens@official");
        assert_eq!(run(&view, &mut panel, &[ctrl('d')]), Step::Stay);
        assert_eq!(panel.pending_delete.as_deref(), Some("lens@official"));
        assert_eq!(
            run(&view, &mut panel, &[ctrl('d')]),
            Step::Uninstall {
                plugin: "lens".into(),
                marketplace: "official".into(),
                scope: Scope::User,
            }
        );

        // And anything else disarms it — including moving off the row.
        let mut panel = Panel::new();
        panel.tab = Tab::Installed;
        walk_to(&view, &mut panel, "lens@official");
        run(&view, &mut panel, &[ctrl('d'), press(Key::Up)]);
        assert!(panel.pending_delete.is_none());
        assert_eq!(run(&view, &mut panel, &[ctrl('d')]), Step::Stay);
    }

    /// A plugin that is not installed has nothing to uninstall, and the
    /// marketplace this build ships cannot be removed at all.
    #[test]
    fn what_cannot_be_thrown_away_is_never_armed() {
        let view = view();
        let mut panel = Panel::new();
        walk_to(&view, &mut panel, "tidy@official");
        assert_eq!(run(&view, &mut panel, &[ctrl('d'), ctrl('d')]), Step::Stay);
        assert!(panel.pending_delete.is_none());

        // The official marketplace.
        let mut panel = Panel::new();
        panel.tab = Tab::Markets;
        walk_to(&view, &mut panel, "market:official");
        assert_eq!(run(&view, &mut panel, &[ctrl('d'), ctrl('d')]), Step::Stay);
        assert!(panel.pending_delete.is_none());

        // The other one can.
        walk_to(&view, &mut panel, "market:mine");
        assert_eq!(run(&view, &mut panel, &[ctrl('d')]), Step::Stay);
        assert_eq!(
            run(&view, &mut panel, &[ctrl('d')]),
            Step::RemoveMarket {
                name: "mine".into()
            }
        );
    }

    /// While a job is out there, the panel takes no key but the one that gives
    /// up on it.
    ///
    /// The point is not tidiness: every one of these keys would start a second
    /// `git` while the first is still cloning, and the two would be writing the
    /// same directory.
    #[test]
    fn a_running_job_swallows_every_key_but_the_one_that_gives_up() {
        let view = view();
        let mut panel = Panel::new();
        panel.busy = Some(Busy {
            what: "正在装 tidy@official …".into(),
            job: "tidy@official".into(),
        });
        for key in [
            press(Key::Enter),
            press(Key::Down),
            press(Key::Tab),
            ctrl('d'),
            typed('x'),
        ] {
            assert_eq!(super::key(&view, &mut panel, key), Step::Stay);
        }
        assert_eq!(panel.cursor, 0, "not even the cursor moved");
        assert!(
            panel.query.is_empty(),
            "and nothing was typed into the filter"
        );
        assert!(panel.busy.is_some(), "the job is still out there");

        assert_eq!(
            super::key(&view, &mut panel, press(Key::Esc)),
            Step::Cancel {
                job: "tidy@official".into()
            }
        );
        assert!(
            panel.busy.is_none(),
            "the panel stops saying it is working the moment the person stops waiting"
        );
    }

    /// A marketplace address is typed, or pasted, and only its first line.
    ///
    /// A URL copied off a web page comes with a newline on the end, and that
    /// newline would be written into a `git clone` argument.
    #[test]
    fn an_address_can_be_typed_or_pasted_and_never_carries_a_newline() {
        let view = view();
        let mut panel = Panel::new();
        panel.tab = Tab::Markets;
        run(&view, &mut panel, &[press(Key::Enter)]);
        assert!(matches!(panel.form, Some(Form::AddMarket(_))));

        run(&view, &mut panel, &[typed('g'), typed('i'), typed('t')]);
        assert!(paste(&mut panel, "hub.example/x.git\nsecond line"));
        let Some(Form::AddMarket(form)) = panel.form.clone() else {
            panic!("the form is up");
        };
        assert_eq!(form.url, "githdub.example/x.git".replace("hd", "h"));
        assert_eq!(
            form.caret,
            form.url.len(),
            "the caret follows what was pasted"
        );

        assert_eq!(
            run(&view, &mut panel, &[press(Key::Enter)]),
            Step::AddMarket {
                url: "github.example/x.git".into()
            }
        );
    }

    /// Enter on an empty address does nothing and leaves the form up.
    ///
    /// The form closing here is the annoying bug: the person is mid-thought,
    /// reaching for the clipboard, and a form that vanished has to be opened
    /// again.
    #[test]
    fn an_empty_address_is_not_a_job() {
        let view = view();
        let mut panel = Panel::new();
        panel.tab = Tab::Markets;
        run(&view, &mut panel, &[press(Key::Enter)]);
        assert_eq!(run(&view, &mut panel, &[press(Key::Enter)]), Step::Stay);
        assert!(matches!(panel.form, Some(Form::AddMarket(_))));
    }

    /// The caret walks over characters, not bytes.
    ///
    /// A URL is ASCII, but what gets pasted into this field is whatever was on
    /// the clipboard — and a byte offset landing inside a multi-byte character
    /// panics the process rather than misdrawing a line.
    #[test]
    fn the_caret_stays_on_character_boundaries() {
        let view = view();
        let mut panel = Panel::new();
        panel.form = Some(Form::AddMarket(AddMarketForm::default()));
        run(&view, &mut panel, &[typed('中'), typed('文')]);
        run(
            &view,
            &mut panel,
            &[press(Key::Left), press(Key::Backspace)],
        );
        let Some(Form::AddMarket(form)) = panel.form.clone() else {
            panic!("the form is up");
        };
        assert_eq!(form.url, "文");
        assert_eq!(form.caret, 0);
    }

    /// Walking into a marketplace shows what it carries, as a filter on the
    /// page that already lists plugins — not a fourth page that would have to
    /// learn everything the first one knows.
    #[test]
    fn a_marketplace_shows_what_it_carries() {
        let view = view();
        let mut panel = Panel::new();
        panel.tab = Tab::Markets;
        walk_to(&view, &mut panel, "market:mine");
        run(&view, &mut panel, &[press(Key::Enter)]);
        let Some(Form::Market(form)) = panel.form.clone() else {
            panic!("a marketplace opens with what can be done to it");
        };
        assert_eq!(form.actions().len(), 3, "and this one can be removed");
        assert_eq!(run(&view, &mut panel, &[press(Key::Enter)]), Step::Stay);
        assert_eq!(panel.tab, Tab::All);
        assert_eq!(panel.query, "mine");
        assert_eq!(shown(&view, &panel), ["lens@mine"]);
    }

    /// The marketplace this build ships offers two actions, not three.
    #[test]
    fn the_shipped_marketplace_offers_no_way_to_remove_it() {
        let view = view();
        let mut panel = Panel::new();
        panel.tab = Tab::Markets;
        walk_to(&view, &mut panel, "market:official");
        run(&view, &mut panel, &[press(Key::Enter)]);
        let Some(Form::Market(form)) = panel.form.clone() else {
            panic!("a marketplace opens with what can be done to it");
        };
        assert!(form.official);
        assert_eq!(
            form.actions(),
            vec![MarketAction::Browse, MarketAction::Update],
            "a row that cannot do anything is a row that should not be drawn"
        );
    }

    /// Changing page drops what was typed on the last one.
    #[test]
    fn changing_page_drops_the_filter() {
        let view = view();
        let mut panel = Panel::new();
        run(&view, &mut panel, &[typed('l'), typed('e')]);
        assert_eq!(panel.query, "le");
        run(&view, &mut panel, &[press(Key::Tab)]);
        assert_eq!(panel.tab, Tab::Installed);
        assert!(
            panel.query.is_empty(),
            "a list emptied by something typed on another page looks broken"
        );
    }

    /// Esc closes the panel, and closes a form back to the list first.
    #[test]
    fn esc_backs_out_one_step_at_a_time() {
        let view = view();
        let mut panel = Panel::new();
        run(&view, &mut panel, &[press(Key::Enter)]);
        assert!(panel.form.is_some());
        assert_eq!(run(&view, &mut panel, &[press(Key::Esc)]), Step::Stay);
        assert!(panel.form.is_none(), "the form goes first");
        assert_eq!(run(&view, &mut panel, &[press(Key::Esc)]), Step::Close);
    }

    /// With no form up, a paste is a search — the box under the cursor is the
    /// filter, same as the providers panel.
    #[test]
    fn a_paste_into_the_list_is_a_search() {
        let mut panel = Panel::new();
        assert!(paste(&mut panel, "lens"));
        assert_eq!(panel.query, "lens");
    }
}
