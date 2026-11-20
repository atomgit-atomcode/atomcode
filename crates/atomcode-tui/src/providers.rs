//! The providers a person may add, change, delete and switch between, and the
//! port that writes them.
//!
//! The same two halves [`crate::settings`] is in, for the same reason
//! (`docs/adr/0022` §3): **what a provider is** — which accounts the
//! configuration has, which models hang off them, which protocols this product
//! speaks — arrives as plain data from whoever reads the configuration; **how it
//! is drawn and worked** is this crate's. A screen that knew `[provider_accounts]`
//! from `[providers]`, or that a CodingPlan account is spelled `AtomGit`, would
//! be a screen that has to be edited every time the configuration grows a table.
//!
//! One thing here is unlike the settings panel: **a credential**. Adding an
//! account means typing an API key, and the rule is kept honest in the shape of
//! the types rather than by remembering it. [`AccountForm`] and [`ModelForm`]
//! carry how many characters have been typed and *never* the characters — the
//! panel lives in [`crate::moment::Moment`], which is cloned once a frame and
//! readable by every module, and which is exactly why `Moment::secret` carries a
//! count too. The characters live in one `String` the host owns and leave it
//! once: into the [`Step`] the port writes from, which redacts them in `Debug`.
//!
//! The list is a filter, not a mode. `crate::settings::Panel::query_caret` has
//! the story of the `searching` flag this panel deliberately does not have:
//! tuix's `search_focused` decides whether the arrows walk the tabs or the
//! query, and the key that turned it on ate the character that turned it on.
//! Here the arrows are always the tabs and typing is always the filter.

use std::sync::Arc;

use crate::surface::{Key, KeyPress, Mods};
// The one set of caret moves every single-line field on this screen shares.
use crate::text::{backspace_at, delete_at, insert_at, snap, step_caret};

/// A protocol a provider speaks, as the launcher offers it.
///
/// Not an enum: which protocols exist is the product's answer, and a screen with
/// a fixed list would refuse a build that added one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Protocol {
    /// What the port gets back. Opaque here — a preset id, or whatever the
    /// launcher calls it.
    pub id: String,
    /// The word to draw.
    pub label: String,
    /// What to prefill the endpoint with, when this protocol has a usual one.
    pub endpoint: Option<String>,
    /// Whether an account on this protocol is asked for a key at all. A local
    /// Ollama is not, and a form that asked would be a form with a field nobody
    /// can fill.
    pub needs_key: bool,
}

/// Host-provided connection choices for one vendor offer. Corresponding URL
/// indices preserve the same service/plan when switching the protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresetConnection {
    pub protocol: Protocol,
    pub base_urls: Vec<String>,
}

/// One account: a place to send requests, and a credential to send them with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountRow {
    /// The id the port writes under, and what the panel selects by.
    pub id: String,
    /// The name to draw, which may differ from the id.
    pub label: String,
    /// The protocol's label, already in the language to draw it in.
    pub protocol: String,
    /// Where it sends, as far as a person needs to see. Empty when the protocol
    /// has one usual endpoint and this account has not moved off it.
    pub endpoint: String,
    /// How many models hang off it.
    pub models: usize,
    /// Whether a credential is stored. **Never the credential** — this answer is
    /// drawn on a screen and kept in a log.
    pub has_key: bool,
    /// Whether this product owns the account and manual editing is refused —
    /// a gateway account signed in through `/login`, for instance.
    pub managed: bool,
    /// Whether it is in the configuration at all. `false` is an offer: a
    /// protocol this build knows about that nobody has set up yet, listed so
    /// adding one is a keystroke rather than a hunt.
    pub configured: bool,
}

impl AccountRow {
    /// Display only the endpoint's authority, never its path, query or userinfo.
    /// This is a label; transport URL validation remains the port's job.
    pub fn endpoint_host(&self) -> &str {
        self.endpoint
            .split_once("://")
            .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default())
            .map(|authority| authority.rsplit('@').next().unwrap_or_default())
            .unwrap_or_default()
    }
}

/// One model: what a person selects when they pick what to talk to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRow {
    /// The selection id — what `/model` takes.
    pub id: String,
    /// The account it is under, by [`AccountRow::id`].
    pub account: String,
    /// The name sent on the wire, which an alias may differ from.
    pub model: String,
    /// How much it can hold, in tokens.
    pub window: usize,
    /// Saved per-model output budget; `None` uses the runtime default.
    pub max_tokens: Option<usize>,
    /// `None` is "let it be decided"; the two `Some`s are a person's override.
    pub vision: Option<bool>,
    /// The reasoning effort this model runs at, when it takes one.
    pub effort: Option<String>,
    /// The effort levels it offers, when it offers fewer than all of them.
    pub levels: Vec<String>,
    /// The one the session is on.
    pub current: bool,
    /// Under a managed account: shown, never edited.
    pub managed: bool,
    /// Display alias advertised by the CodingPlan gateway (`models-v3`).
    /// Shown instead of the wire name when set; `None` keeps the raw name.
    pub alias: Option<String>,
    /// Human-readable description advertised by the CodingPlan gateway,
    /// shown under the model's row in the picker. `None` adds no line.
    pub desc: Option<String>,
    /// English alias / description advertised by the CodingPlan gateway.
    /// Shown when the interface language is English; `None` falls back to
    /// [`Self::alias`].
    pub alias_en: Option<String>,
    pub desc_en: Option<String>,
}

impl ModelRow {
    /// The alias to show for the interface language in force: the English one
    /// the server advertised when the UI is in English, the Chinese one
    /// otherwise. Neither has a fallback — when the language's own alias is
    /// absent the raw model name stands, so an English UI never shows a
    /// Chinese alias it was not given.
    pub fn localized_alias(&self) -> Option<&str> {
        match crate::i18n::current_locale() {
            crate::i18n::Locale::En => self.alias_en.as_deref(),
            _ => self.alias.as_deref(),
        }
    }

    /// The description to show, selected the same way as
    /// [`Self::localized_alias`].
    pub fn localized_desc(&self) -> Option<&str> {
        match crate::i18n::current_locale() {
            crate::i18n::Locale::En => self.desc_en.as_deref(),
            _ => self.desc.as_deref(),
        }
    }

    /// Only explicitly declared, known levels are offered when switching models.
    /// A stored default or a single configured value is not a level catalog.
    pub fn effort_pick(&self) -> Option<Vec<String>> {
        let mut levels: Vec<String> = atomcode_harness::REASONING_EFFORT_LEVELS
            .iter()
            .filter(|level| {
                self.levels
                    .iter()
                    .any(|value| value.trim().eq_ignore_ascii_case(level))
            })
            .map(|level| level.to_string())
            .collect();
        if levels.is_empty() {
            return None;
        }
        levels.push("default".to_string());
        Some(levels)
    }
}

/// The level `/effort` lands on when stepping from `current` across `levels` —
/// the pick list exactly as [`ModelRow::effort_pick`] returns it (declared
/// levels in wire order, `default` last). `None` when nothing is declared:
/// there is nothing to step through, and `/effort` reports that rather than
/// the key pretending.
pub(crate) fn effort_step(
    current: &str,
    levels: Option<Vec<String>>,
    forward: bool,
) -> Option<String> {
    let cycle = levels?;
    if cycle.is_empty() {
        return None;
    }
    let len = cycle.len();
    Some(match cycle.iter().position(|level| level == current) {
        Some(at) => cycle[(at + if forward { 1 } else { len - 1 }) % len].clone(),
        // The level in force is not one this model declares (the config
        // changed since the session started): enter the list from the near
        // end instead of guessing a middle rung.
        None if forward => cycle[0].clone(),
        None => cycle[len - 1].clone(),
    })
}

/// The providers, as of one frame.
///
/// Immutable and cheap to clone, so a `Moment` can carry it and two renders
/// against that moment see the same list — the promise
/// [`crate::settings::SettingsView`] keeps for settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProvidersView {
    accounts: Arc<Vec<AccountRow>>,
    models: Arc<Vec<ModelRow>>,
    protocols: Arc<Vec<Protocol>>,
    efforts: Arc<Vec<String>>,
    connections: Arc<Vec<(String, Vec<PresetConnection>)>>,
    /// The row [`Self::with_current`] resolved the session to, when it could.
    ///
    /// Not the same as the row marked `current`: a name no row or two rows
    /// answer to leaves the file's own mark standing (so `model_after` has
    /// somewhere to step from), and that mark says what the *next* session
    /// starts on — nothing about which account this one is on.
    live: Option<usize>,
}

impl ProvidersView {
    pub fn new(
        accounts: Vec<AccountRow>,
        models: Vec<ModelRow>,
        protocols: Vec<Protocol>,
        efforts: Vec<String>,
    ) -> Self {
        Self {
            accounts: Arc::new(accounts),
            models: Arc::new(models),
            protocols: Arc::new(protocols),
            efforts: Arc::new(efforts),
            connections: Arc::new(Vec::new()),
            live: None,
        }
    }

    pub fn accounts(&self) -> &[AccountRow] {
        &self.accounts
    }

    pub fn with_connections(mut self, connections: Vec<(String, Vec<PresetConnection>)>) -> Self {
        self.connections = Arc::new(connections);
        self
    }

    pub fn connections(&self, preset: &str) -> &[PresetConnection] {
        self.connections
            .iter()
            .find(|(id, _)| id == preset)
            .map(|(_, options)| options.as_slice())
            .unwrap_or_default()
    }

    /// The model one step on from the one in use, or one step back.
    ///
    /// Its own function, and pure, so "which model is next" can be judged
    /// without a panel, a port or a running session — the key that asks for it
    /// cannot be.
    ///
    /// `None` when there is nowhere to go: no models configured, or only the
    /// one already in use. Wraps around, so the key always does something on a
    /// list of two and never dead-ends on a list of many. With nothing marked
    /// current — a session on a model that is no longer in the list — it starts
    /// at the first, which is the only honest answer: the current one is not
    /// somewhere this list can step from.
    pub fn model_after(&self, forward: bool) -> Option<&ModelRow> {
        let models = self.models();
        if models.len() < 2 {
            return None;
        }
        let at = models.iter().position(|m| m.current);
        let Some(at) = at else {
            return models.first();
        };
        let next = match forward {
            true => (at + 1) % models.len(),
            false => (at + models.len() - 1) % models.len(),
        };
        models.get(next)
    }

    pub fn models(&self) -> &[ModelRow] {
        &self.models
    }

    /// The protocols an account may be put on. Empty is a launcher that offers
    /// none, and then nothing can be added — which the panel says rather than
    /// opening a form with an empty toggle.
    pub fn protocols(&self) -> &[Protocol] {
        &self.protocols
    }

    /// Every reasoning-effort level this build has a word for, in its own order.
    pub fn efforts(&self) -> &[String] {
        &self.efforts
    }

    /// The same list, with `current` marking the model the *session* is on.
    ///
    /// The port reads a file, and a file says which model the next session would
    /// start on — not which one this one was switched to with `/model`. The two
    /// differ exactly as often as a person switches without saving, which is
    /// most of the time, so the screen marks the row from what it already knows
    /// about its own session rather than asking the file to know something it
    /// cannot.
    ///
    /// `live` is that name, and it is **not always a selection id**: the
    /// description reports the model sent on the wire ([`ModelRow::model`]),
    /// which an aliased row differs from — `AtomGit-deepseek-flash` sends
    /// `deepseek-flash`. Both names are answered, the id first. And a name no row
    /// answers to (the first frames of a session, a session on a model the file
    /// no longer lists) leaves the list exactly as the file had it: marking
    /// nothing is not a neutral act here, because `model_after` steps *from* the
    /// marked row and an unmarked list steps off the first one every time.
    pub fn with_current(&self, live: Option<&str>) -> Self {
        let Some(at) = live.and_then(|live| self.marked_row(live)) else {
            return self.clone();
        };
        let models: Vec<ModelRow> = self
            .models
            .iter()
            .enumerate()
            .map(|(i, row)| ModelRow {
                current: i == at,
                ..row.clone()
            })
            .collect();
        Self {
            accounts: self.accounts.clone(),
            models: Arc::new(models),
            protocols: self.protocols.clone(),
            efforts: self.efforts.clone(),
            connections: self.connections.clone(),
            live: Some(at),
        }
    }

    /// The row the session is known to be on — resolved from its own model by
    /// [`Self::with_current`], never read off the file's mark. `None` when the
    /// list was never marked from a session, or the name could not pick one row.
    pub fn live_row(&self) -> Option<&ModelRow> {
        self.live.and_then(|at| self.models.get(at))
    }

    /// Which row `live` names: by selection id when that is what it is, else the
    /// one row that sends it on the wire.
    ///
    /// Two accounts running the same model is answered `None` — the file's own
    /// row stays marked rather than one of them being picked at random. Telling
    /// those apart needs the description to report the *selection* rather than
    /// the wire name, which is a change to what an agent says about itself, not
    /// to this list.
    fn marked_row(&self, live: &str) -> Option<usize> {
        if let Some(at) = self.models.iter().position(|row| row.id == live) {
            return Some(at);
        }
        let mut named = self
            .models
            .iter()
            .enumerate()
            .filter(|(_, row)| row.model == live)
            .map(|(at, _)| at);
        match (named.next(), named.next()) {
            (Some(at), None) => Some(at),
            _ => None,
        }
    }

    pub fn account(&self, id: &str) -> Option<&AccountRow> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn model(&self, id: &str) -> Option<&ModelRow> {
        self.models.iter().find(|m| m.id == id)
    }

    /// The accounts a model may be added to: the ones this product does not own.
    pub fn open_accounts(&self) -> Vec<String> {
        self.accounts
            .iter()
            .filter(|a| !a.managed)
            .map(|a| a.id.clone())
            .collect()
    }

    /// The account `/model`'s add row opens the provider list on: the one the
    /// session is on, else the first one a model may be added to.
    pub fn home_account(&self) -> Option<&str> {
        self.live_row()
            .or_else(|| self.models.iter().find(|m| m.current))
            .map(|m| m.account.as_str())
            .or_else(|| {
                self.accounts
                    .iter()
                    .find(|a| a.configured && !a.managed)
                    .map(|a| a.id.as_str())
            })
    }

    /// The listed server model behind an [`Listed::Available`] row.
    pub fn available<'a>(&self, panel: &'a Panel, at: usize) -> Option<&'a Discovered> {
        let Screen::Detail(account) = &panel.screen else {
            return None;
        };
        match &panel.discovery.get(account)?.state {
            DiscoveryState::Ready(items) => items.get(at),
            _ => None,
        }
    }

    /// The rows of the screen that is showing, filtered as the panel asks.
    ///
    /// One function, walked by the keys, the drawing and the hit test, so a
    /// click that landed on one row and a highlight drawn on another is ruled
    /// out by construction rather than by keeping three formulas in step — the
    /// shape `crate::modules::settings` uses for the same reason.
    ///
    /// Each screen that can add something ends on its add row, so the row is
    /// always the last one: the drawing pins it under the scrolling part, and
    /// `End` reaches it.
    pub fn listed(&self, panel: &Panel) -> Vec<Listed> {
        let q = panel.query.trim().to_lowercase();
        let hit = |haystack: &[&str]| -> bool {
            q.is_empty()
                || haystack
                    .iter()
                    .any(|text| text.to_lowercase().contains(q.as_str()))
        };
        let mut out: Vec<Listed> = Vec::new();
        // 挑强度:这一层列表就是他声明的档位,画面的行先让开。筛选用的是
        // 同一把尺子,所以打到一半「high」也能把其它档收掉。
        if let Some(pick) = &panel.efforts {
            return (0..pick.levels.len())
                .filter(|at| hit(&[&pick.levels[*at]]))
                .map(Listed::Effort)
                .collect();
        }
        match &panel.screen {
            Screen::Models => {
                // 按账号分组:一个账号一条不可选的小标题,它的模型跟在下面。
                //
                // 正在用的那一个打头:它的账号整组排最前,组里它自己排第一,其余
                // 照文件顺序。`/model` 打开时光标停在第一个能停的行上,于是落下
                // 就在它身上。
                let in_use = self.models.iter().position(|m| m.current);
                let mut order: Vec<usize> = in_use.into_iter().collect();
                order.extend((0..self.models.len()).filter(|i| Some(*i) != in_use));
                let mut groups: Vec<&str> = Vec::new();
                for &i in &order {
                    let account = self.models[i].account.as_str();
                    if !groups.contains(&account) {
                        groups.push(account);
                    }
                }
                for account in groups {
                    let under: Vec<usize> = order
                        .iter()
                        .copied()
                        .filter(|&i| {
                            let m = &self.models[i];
                            m.account == account
                                && hit(&[
                                    &m.id,
                                    &m.model,
                                    &m.account,
                                    m.localized_alias().unwrap_or_default(),
                                ])
                        })
                        .collect();
                    // 筛空了的组不留标题:一个空标题说的是「这里什么都没有」。
                    if under.is_empty() {
                        continue;
                    }
                    out.push(Listed::Group(under[0]));
                    out.extend(under.into_iter().map(Listed::Model));
                }
                out.push(Listed::AddModel);
            }
            Screen::Providers => {
                for (i, a) in self.accounts.iter().enumerate() {
                    if hit(&[&a.id, &a.label, &a.protocol, a.endpoint_host()]) {
                        out.push(Listed::Account(i));
                    }
                }
                out.push(Listed::AddAccount);
            }
            Screen::Detail(account) => {
                let Some(row) = self.account(account) else {
                    return out;
                };
                out.push(Listed::Connection);
                let added: Vec<usize> = self
                    .models
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| {
                        m.account == *account
                            && hit(&[&m.id, &m.model, m.localized_alias().unwrap_or_default()])
                    })
                    .map(|(i, _)| i)
                    .collect();
                if !added.is_empty() {
                    out.push(Listed::Section(Section::Added));
                    out.extend(added.into_iter().map(Listed::Configured));
                }
                // A managed account's models come from the gateway that owns
                // it: nothing to list from a server, nothing to add by hand.
                if row.managed {
                    return out;
                }
                match panel.discovery.get(account).map(|d| &d.state) {
                    None | Some(DiscoveryState::Loading) => {
                        out.push(Listed::Section(Section::Server));
                        out.push(Listed::Loading);
                    }
                    Some(DiscoveryState::Failed(_)) => {
                        out.push(Listed::Section(Section::Server));
                        out.push(Listed::Failed);
                    }
                    Some(DiscoveryState::Ready(items)) => {
                        // 「服务端还有」 = 服务端列出的减去已经配了的(按线上名)。
                        // 正在添加的那几行在写入成功之前仍留在这里。
                        let have: Vec<&str> = self
                            .models
                            .iter()
                            .filter(|m| m.account == *account)
                            .map(|m| m.model.as_str())
                            .collect();
                        let available: Vec<usize> = items
                            .iter()
                            .enumerate()
                            .filter(|(_, item)| {
                                !have.contains(&item.id.as_str())
                                    && hit(&[&item.id, item.name.as_deref().unwrap_or_default()])
                            })
                            .map(|(j, _)| j)
                            .collect();
                        if !available.is_empty() {
                            // Listed out of a catalog rather than by the endpoint:
                            // the heading says the account may not reach them.
                            let section = match items.iter().any(|item| item.catalog_only) {
                                true => Section::Catalog,
                                false => Section::Server,
                            };
                            out.push(Listed::Section(section));
                            out.extend(available.into_iter().map(Listed::Available));
                        }
                    }
                }
                out.push(Listed::Manual);
            }
        }
        out
    }

    /// Whether a typed filter names a server model exactly — then the manual
    /// row has nothing to offer that the list does not.
    pub fn exact_server_match(&self, panel: &Panel) -> bool {
        let q = panel.query.trim();
        let Screen::Detail(account) = &panel.screen else {
            return false;
        };
        !q.is_empty()
            && panel
                .discovery
                .get(account)
                .is_some_and(|d| match &d.state {
                    DiscoveryState::Ready(items) => items.iter().any(|item| item.id == q),
                    _ => false,
                })
    }
}

impl ProvidersView {
    /// The row the cursor is on, as it settles: a cursor left on a heading
    /// answers for the first row under it that it could rest on. What the
    /// drawing lights and the footer speaks for, so neither depends on every
    /// caller having settled the cursor first.
    pub fn pointed(&self, panel: &Panel) -> Option<(usize, Listed)> {
        let listed = self.listed(panel);
        let at = settle(&listed, panel.cursor);
        listed.get(at).map(|row| (at, *row))
    }

    /// 把光标落到一行能停的行上。
    ///
    /// 换画面、改筛选词之后都要走一趟:模型页的第一行是账号的小标题,
    /// 而亮着的那一行必须是人真能对它做点什么的行——否则屏幕上亮着一条分界线,
    /// 回车没反应。
    pub fn settle_cursor(&self, panel: &mut Panel) {
        let listed = self.listed(panel);
        panel.cursor = settle(&listed, panel.cursor);
    }
}

/// What a listed row is about, for finding it again in another list.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowId {
    Model(String),
    Account(String),
    Available(String),
    Connection,
    Manual,
    AddModel,
    AddAccount,
}

fn row_id(view: &ProvidersView, panel: &Panel, row: Listed) -> Option<RowId> {
    match row {
        Listed::Model(i) | Listed::Configured(i) => {
            view.models().get(i).map(|m| RowId::Model(m.id.clone()))
        }
        Listed::Account(i) => view.accounts().get(i).map(|a| RowId::Account(a.id.clone())),
        Listed::Available(j) => view
            .available(panel, j)
            .map(|item| RowId::Available(item.id.clone())),
        Listed::Connection => Some(RowId::Connection),
        Listed::Manual => Some(RowId::Manual),
        Listed::AddModel => Some(RowId::AddModel),
        Listed::AddAccount => Some(RowId::AddAccount),
        Listed::Effort(_)
        | Listed::Group(_)
        | Listed::Section(_)
        | Listed::Loading
        | Listed::Failed => None,
    }
}

/// 列表从 `before` 换成 `after` 时,把光标挪回它原来指着的那一行。
///
/// 光标存的是行号,而模型页按「正在用的那一个」排序、详情页里加好的模型会从
/// 「服务端还有」挪到「已添加」——按行号不动,光标就指到了别的东西上,回车
/// 换过去或加上的是一个人没挑的那个。认的是行背后的东西,找不到就落到最近的
/// 能停的行。
pub fn follow_cursor(before: &ProvidersView, after: &ProvidersView, panel: &mut Panel) {
    let pointed = before
        .listed(panel)
        .get(panel.cursor)
        .and_then(|row| row_id(before, panel, *row));
    point_at_row(after, panel, pointed);
}

/// The same, across a change to the panel itself (a listing arriving).
fn follow_panel(view: &ProvidersView, before: &Panel, panel: &mut Panel) {
    let pointed = view
        .listed(before)
        .get(before.cursor)
        .and_then(|row| row_id(view, before, *row));
    point_at_row(view, panel, pointed);
}

fn point_at_row(view: &ProvidersView, panel: &mut Panel, pointed: Option<RowId>) {
    let listed = view.listed(panel);
    let found = pointed.and_then(|want| {
        listed
            .iter()
            .position(|row| row_id(view, panel, *row).as_ref() == Some(&want))
    });
    panel.cursor = match found {
        Some(at) => at,
        None => settle(&listed, panel.cursor),
    };
}

/// A heading on the detail page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// The models the account already has.
    Added,
    /// The models its endpoint lists that it has not.
    Server,
    /// The same, read out of the public catalog because the endpoint lists
    /// nothing: the account may not reach them.
    Catalog,
}

/// One row of the list, as everything that walks the list sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listed {
    /// `/model`: an account's name over its models — carried as the index of
    /// the first of them, so this stays `Copy` and the name is read where it is
    /// drawn. Not selectable.
    Group(usize),
    /// `/model`: a model, by its index into [`ProvidersView::models`].
    Model(usize),
    /// `/model`: the pinned last row, which opens the provider list.
    AddModel,
    /// The provider list: an account, by its index into
    /// [`ProvidersView::accounts`].
    Account(usize),
    /// The provider list: the pinned last row, which opens the add form.
    AddAccount,
    /// A detail page's first row: the account's connection.
    Connection,
    /// A detail page's heading. Not selectable.
    Section(Section),
    /// A detail page: a model the account has, by its index into
    /// [`ProvidersView::models`].
    Configured(usize),
    /// A detail page: a model the endpoint lists that the account does not
    /// have, by its index into the listing (`Panel::discovery`).
    Available(usize),
    /// A detail page: the listing is on its way. Not selectable.
    Loading,
    /// A detail page: the listing failed. Enter asks again.
    Failed,
    /// A detail page: the pinned last row, which adds a model by hand.
    Manual,
    /// 挑思考强度时的一行,索引进 [`Panel::efforts`] 的档位表。最后一行是
    /// `default` —— 它和其它行一样是一档,只是一个把强度交回端点的选择。
    Effort(usize),
}

impl Listed {
    /// Whether the cursor may rest here.
    pub fn selectable(&self) -> bool {
        !matches!(
            self,
            Listed::Group(_) | Listed::Section(_) | Listed::Loading
        )
    }

    /// Whether this is a screen's pinned add row.
    pub fn pinned(&self) -> bool {
        matches!(self, Listed::AddModel | Listed::AddAccount | Listed::Manual)
    }
}

/// 从 `from` 往 `step` 方向找下一个能停的行;没有就停在原地。
fn walk(listed: &[Listed], from: usize, down: bool) -> usize {
    let mut at = from;
    loop {
        let next = match down {
            true => at + 1,
            false => match at.checked_sub(1) {
                Some(next) => next,
                None => return from,
            },
        };
        match listed.get(next) {
            None => return from,
            Some(row) if row.selectable() => return next,
            Some(_) => at = next,
        }
    }
}

/// 从 `from` 起(含)第一个能停的行;底下没有就往上找。
fn settle(listed: &[Listed], from: usize) -> usize {
    if listed.get(from).is_some_and(Listed::selectable) {
        return from;
    }
    let below = listed
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, row)| row.selectable())
        .map(|(at, _)| at);
    below
        .or_else(|| {
            listed
                .iter()
                .enumerate()
                .take(from)
                .filter(|(_, row)| row.selectable())
                .map(|(at, _)| at)
                .next_back()
        })
        .unwrap_or(from)
}

/// Which screen of the panel is showing.
///
/// Three, and no tabs: `/model` switches, `/provider` lists the accounts, and
/// one account's page is where everything about it is changed. The tabs this
/// replaced put a switcher and a manager in one panel, so the switcher carried
/// every management key and the manager every switching one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Screen {
    /// `/model`.
    Models,
    /// `/provider`.
    #[default]
    Providers,
    /// One account, by id.
    Detail(String),
}

/// The listing a detail page shows under 「服务端还有」.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailDiscovery {
    /// The request this answers; a listing for another one is dropped.
    pub id: u64,
    pub state: DiscoveryState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryState {
    Loading,
    Ready(Vec<Discovered>),
    Failed(String),
}

/// Which field of the account form has the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountField {
    Name,
    Protocol,
    Endpoint,
    Key,
    /// The last row of a short form: Enter shows every field. A row, not a
    /// key: what it opens is listed where the eye already is.
    More,
}

/// Adding an account, or changing one.
///
/// **No key lives here.** `key_len` is what the panel draws dots from; the
/// characters are the host's, and this type could not hold them without putting
/// them in the `Moment` — see this module's own doc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountForm {
    /// `Some` when an account that already exists is being changed. Its id is
    /// then fixed: the id is what everything else in the file points at.
    pub editing: Option<String>,
    pub name: String,
    /// Index into [`ProvidersView::protocols`].
    pub protocol: usize,
    pub endpoint: String,
    /// How many characters have been typed into the key field.
    pub key_len: usize,
    pub focus: AccountField,
    /// Where the next character goes in the focused text field, as a byte
    /// offset. Kept on a character boundary by [`step_caret`].
    pub caret: usize,
    /// Whether the protocol may be moved. A legacy entry's wire and a managed
    /// account's are not this panel's to change.
    pub locked: bool,
    pub preset: Option<String>,
    pub advanced: bool,
    pub discovery: Option<AccountDiscovery>,
}

/// A read-only listing for an unsaved Ollama connection. No credential lives here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountDiscovery {
    pub id: u64,
    pub result: Option<Result<Vec<Discovered>, String>>,
}

fn next_discovery_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl AccountForm {
    /// A fresh add form, starting on the first protocol the launcher offers.
    pub fn add(view: &ProvidersView) -> Self {
        let endpoint = view
            .protocols()
            .first()
            .and_then(|p| p.endpoint.clone())
            .unwrap_or_default();
        Self {
            editing: None,
            name: String::new(),
            protocol: 0,
            endpoint,
            key_len: 0,
            focus: AccountField::Name,
            caret: 0,
            locked: false,
            preset: None,
            advanced: true,
            discovery: None,
        }
    }

    /// An edit form filled from a row.
    ///
    /// The key field starts empty and empty means "leave it alone": the stored
    /// key is not readable from here, so there is nothing to prefill it with and
    /// nothing that could be shown.
    pub fn edit(view: &ProvidersView, row: &AccountRow) -> Self {
        let protocol = view
            .protocols()
            .iter()
            .position(|p| p.label == row.protocol)
            .unwrap_or(0);
        let endpoint = if row.endpoint.is_empty() {
            view.protocols()
                .get(protocol)
                .and_then(|p| p.endpoint.clone())
                .unwrap_or_default()
        } else {
            row.endpoint.clone()
        };
        let locked = !row.configured;
        let endpoint_len = endpoint.len();
        Self {
            editing: Some(row.id.clone()),
            name: row.label.clone(),
            protocol,
            endpoint,
            key_len: 0,
            focus: if locked {
                AccountField::Endpoint
            } else {
                AccountField::Protocol
            },
            caret: if locked { endpoint_len } else { 0 },
            locked,
            preset: None,
            advanced: true,
            discovery: None,
        }
    }

    /// Connecting a catalog offer is an add, with its vendor identity intact.
    pub fn connect(view: &ProvidersView, row: &AccountRow) -> Self {
        let mut form = Self::edit(view, row);
        form.editing = None;
        form.name = row.id.clone();
        form.preset = Some(row.id.clone());
        form.advanced = false;
        form.locked = false;
        form.focus = if form.needs_key(view) {
            AccountField::Key
        } else {
            AccountField::Endpoint
        };
        form.caret = 0;
        if view.connections(&row.id).len() > 1 {
            form.focus = AccountField::Protocol;
        }
        form
    }

    /// The fields this form actually has, in the order Tab walks them.
    ///
    /// Asked rather than matched on at each of the places that care, because
    /// "which fields does this form have" is one fact and three copies of it
    /// would disagree the first time a field grew a condition.
    pub fn fields(&self, view: &ProvidersView) -> Vec<AccountField> {
        let mut out = Vec::new();
        if !self.advanced {
            if self
                .preset
                .as_deref()
                .is_some_and(|id| view.connections(id).len() > 1)
            {
                let mut fields = vec![AccountField::Protocol, AccountField::Endpoint];
                if self.needs_key(view) {
                    fields.push(AccountField::Key);
                }
                fields.push(AccountField::More);
                return fields;
            }
            return if self.needs_key(view) {
                vec![AccountField::Key, AccountField::More]
            } else {
                vec![AccountField::Endpoint, AccountField::More]
            };
        }
        if self.editing.is_none() {
            out.push(AccountField::Name);
        }
        if !self.locked {
            out.push(AccountField::Protocol);
        }
        out.push(AccountField::Endpoint);
        if self.needs_key(view) {
            out.push(AccountField::Key);
        }
        out
    }

    pub fn is_ollama(&self, view: &ProvidersView) -> bool {
        view.protocols()
            .get(self.protocol)
            .is_some_and(|p| p.id == "ollama")
    }

    pub fn start_discovery(&mut self, view: &ProvidersView) -> Step {
        if !self.is_ollama(view) || self.endpoint.trim().is_empty() {
            return Step::Stay;
        }
        self.discovery = Some(AccountDiscovery {
            id: next_discovery_id(),
            result: None,
        });
        Step::DiscoverDraft {
            endpoint: self.endpoint.trim().into(),
        }
    }

    pub fn needs_key(&self, view: &ProvidersView) -> bool {
        view.protocols()
            .get(self.protocol)
            .is_some_and(|p| p.needs_key)
    }

    pub fn protocol_label(&self, view: &ProvidersView) -> String {
        view.protocols()
            .get(self.protocol)
            .map(|p| p.label.clone())
            .unwrap_or_default()
    }

    fn step_focus(&mut self, view: &ProvidersView, forward: bool) {
        let fields = self.fields(view);
        if fields.is_empty() {
            return;
        }
        let at = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (at + 1) % fields.len()
        } else {
            (at + fields.len() - 1) % fields.len()
        };
        self.focus = fields[next];
        self.caret = self.focused_len();
    }

    fn focused_len(&self) -> usize {
        match self.focus {
            AccountField::Name => self.name.len(),
            AccountField::Endpoint => self.endpoint.len(),
            AccountField::Protocol | AccountField::Key | AccountField::More => 0,
        }
    }

    /// Move to the next protocol, carrying its endpoint with it.
    ///
    /// The endpoint follows the protocol only while it is still the one the
    /// protocol suggested: a person who typed their own endpoint and then
    /// cycled the protocol to see what else is on offer must not find it gone.
    fn cycle_protocol(&mut self, view: &ProvidersView, forward: bool) {
        if let Some(id) = self.preset.as_deref() {
            let options = view.connections(id);
            if let Some(at) = options
                .iter()
                .position(|option| option.protocol.label == self.protocol_label(view))
            {
                let next = if forward {
                    (at + 1) % options.len()
                } else {
                    (at + options.len() - 1) % options.len()
                };
                if let Some(protocol) = view
                    .protocols()
                    .iter()
                    .position(|p| p.label == options[next].protocol.label)
                {
                    if self.endpoint.trim().is_empty() {
                        self.endpoint = options[next].protocol.endpoint.clone().unwrap_or_default();
                    } else if let Some(index) = options[at]
                        .base_urls
                        .iter()
                        .position(|url| url == self.endpoint.trim().trim_end_matches('/'))
                    {
                        if let Some(url) = options[next].base_urls.get(index) {
                            self.endpoint = url.clone();
                        }
                    }
                    self.protocol = protocol;
                    self.caret = self.caret.min(self.focused_len());
                }
                return;
            }
        }
        self.preset = None;
        let protocols = view.protocols();
        if protocols.is_empty() {
            return;
        }
        let was_suggested = protocols
            .get(self.protocol)
            .map(|p| p.endpoint.clone().unwrap_or_default())
            .is_some_and(|suggested| suggested == self.endpoint)
            || self.endpoint.is_empty();
        self.protocol = if forward {
            (self.protocol + 1) % protocols.len()
        } else {
            (self.protocol + protocols.len() - 1) % protocols.len()
        };
        if was_suggested {
            self.endpoint = protocols[self.protocol]
                .endpoint
                .clone()
                .unwrap_or_default();
        }
        // The field it was on may not exist on the new protocol — a keyless one
        // has no key field, and a focus left pointing at it would eat keys.
        let fields = self.fields(view);
        if !fields.contains(&self.focus) {
            self.focus = AccountField::Endpoint;
        }
        self.caret = self.caret.min(self.focused_len());
    }

    /// The field being typed into and its caret, when the focus is on one.
    ///
    /// Both at once, from one borrow, so nothing can edit the text of one field
    /// while moving the caret of another.
    fn text_and_caret(&mut self) -> Option<(&mut String, &mut usize)> {
        let caret = &mut self.caret;
        match self.focus {
            AccountField::Name => Some((&mut self.name, caret)),
            AccountField::Endpoint => Some((&mut self.endpoint, caret)),
            AccountField::Protocol | AccountField::Key | AccountField::More => None,
        }
    }
}

/// The context windows a model is offered, in tokens, largest first, after
/// "automatic" (see [`ModelForm::window`]).
///
/// Picked rather than typed: a window is one of a handful of sizes models
/// actually come in, and a typed `12k` was a typo the form had to send back.
/// Automatic comes first and is where a new model starts, because a window
/// saved larger than the model's real one is worse than a cautious one:
/// compaction is measured against it, so it never fires, and the provider
/// refuses the overlong request instead.
pub const WINDOW_PRESETS: [usize; 6] = [1_000_000, 512_000, 256_000, 128_000, 64_000, 32_000];

/// The smallest and largest window [`parse_window`] takes. Below a thousand
/// tokens is a typo for a larger number (`1.4` meant `1.4M`), and above a
/// hundred million no model exists — a held-down `0` key would otherwise
/// reach the configuration file, which stores the window as a signed integer.
pub const WINDOW_TYPED_RANGE: std::ops::RangeInclusive<usize> = 1_000..=100_000_000;

/// A typed window, in tokens: `200000`, `200K`, `1.5M` (either case). `None`
/// for anything else, and for a size outside [`WINDOW_TYPED_RANGE`].
pub fn parse_window(text: &str) -> Option<usize> {
    let text = text.trim().to_ascii_lowercase();
    let (number, scale) = match text.strip_suffix('k') {
        Some(number) => (number, 1_000.0),
        None => match text.strip_suffix('m') {
            Some(number) => (number, 1_000_000.0),
            None => (text.as_str(), 1.0),
        },
    };
    // Digits and one dot only: `f64::from_str` would also take `1e9`, `inf`
    // and a sign, none of which is a way anyone writes a window.
    let number = number.trim();
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let value = number.parse::<f64>().ok()? * scale;
    if !value.is_finite() || value > *WINDOW_TYPED_RANGE.end() as f64 {
        return None;
    }
    Some(value.round() as usize).filter(|window| WINDOW_TYPED_RANGE.contains(window))
}

/// A window as it reads on the form: `1M`, `512K`, or the plain count when it
/// is neither a whole million nor a whole thousand.
pub fn window_label(window: usize) -> String {
    if window >= 1_000_000 && window.is_multiple_of(1_000_000) {
        format!("{}M", window / 1_000_000)
    } else if window >= 1000 && window.is_multiple_of(1000) {
        format!("{}K", window / 1000)
    } else {
        window.to_string()
    }
}

/// Exact wire budgets: labels do not round 4096 into decimal "4K".
pub const OUTPUT_PRESETS: &[usize] = &[4096, 8192, 16384, 32768, 65536, 131072];

/// Positive integer tokens, optionally in decimal K/M. Match the runtime's u32 range.
pub fn parse_output_tokens(text: &str) -> Option<usize> {
    let text = text.trim().to_ascii_lowercase();
    let (number, scale) = if let Some(number) = text.strip_suffix('k') {
        (number, 1000u32)
    } else if let Some(number) = text.strip_suffix('m') {
        (number, 1_000_000u32)
    } else {
        (text.as_str(), 1u32)
    };
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    number
        .parse::<u32>()
        .ok()?
        .checked_mul(scale)
        .filter(|n| *n > 0)
        .map(|n| n as usize)
}

/// Which field of the model form has the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelField {
    Model,
    Vision,
    Effort,
    Levels,
    Window,
    Output,
}

/// Adding a model to an account, or changing one.
///
/// The account is fixed: a model is added from its account's page, and moving
/// one between accounts is a delete and an add.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelForm {
    /// `Some` when an existing model is being changed.
    pub editing: Option<String>,
    /// The account it hangs off, by id.
    pub account: String,
    pub model: String,
    /// The context window, in tokens. `None` is "automatic": nothing is sent,
    /// and whoever saves decides — a new model gets its protocol's default
    /// (128k for most, 8k for ollama), an edited one keeps the window it has.
    /// Else one of [`WINDOW_PRESETS`], or the value an edited model already had
    /// when that is none of them.
    pub window: Option<usize>,
    /// `Some` while the window is the custom one, holding what is typed —
    /// `200000`, `200k`, `1.5m`. It is the last stop the arrows reach after
    /// the presets, and typing a digit on a preset lands here too. Read by
    /// [`parse_window`] when the form is saved; [`ModelForm::window`] is not
    /// used while this is set.
    pub window_typed: Option<String>,
    /// Custom output budget text. Empty/invalid text must not silently mean automatic.
    pub max_tokens_typed: Option<String>,
    pub vision: Option<bool>,
    /// `None` is a model that takes no effort setting at all.
    pub effort: Option<String>,
    /// Parallel to [`ProvidersView::efforts`]: which levels this model offers.
    pub levels: Vec<bool>,
    /// Which level Space toggles.
    pub level: usize,
    pub focus: ModelField,
    /// Where the next character goes in the focused text field.
    pub caret: usize,
    pub known_levels: Option<Vec<String>>,
    pub discovered_model: Option<String>,
    pub name: Option<String>,
    pub max_tokens: Option<usize>,
}

/// One model the account's endpoint lists, as its page shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Discovered {
    pub id: String,
    pub catalog_only: bool,
    pub name: Option<String>,
    pub max_tokens: Option<usize>,
    pub vision: Option<bool>,
    pub effort: Option<String>,
    pub catalog_default_effort: Option<String>,
    pub levels: Option<Vec<String>>,
    /// The context window the listing gave, when it gave one.
    pub window: Option<usize>,
}

impl ModelDraft {
    /// The draft one Enter on a server row writes: what the listing declared,
    /// and nothing it did not. Never the session's model, never the default.
    pub fn listed(account: &str, item: &Discovered) -> Self {
        ModelDraft {
            account: account.to_string(),
            model: item.id.clone(),
            name: item.name.clone(),
            max_tokens: item.max_tokens,
            window: item.window,
            vision: item.vision,
            effort: item.effort.clone(),
            levels: item.levels.clone(),
            ..ModelDraft::default()
        }
    }
}

impl ModelForm {
    /// A fresh add form on `account`. `None` when a model may not be added
    /// there: an account this product owns, or one that is gone.
    pub fn add(view: &ProvidersView, account: &str) -> Option<Self> {
        if !view.open_accounts().iter().any(|id| id == account) {
            return None;
        }
        Some(Self {
            editing: None,
            account: account.to_string(),
            model: String::new(),
            window: None,
            window_typed: None,
            max_tokens_typed: None,
            vision: None,
            effort: None,
            levels: vec![true; view.efforts().len()],
            level: 0,
            focus: ModelField::Model,
            caret: 0,
            known_levels: None,
            discovered_model: None,
            name: None,
            max_tokens: None,
        })
    }

    /// The add form a server row opens with Ctrl+E: what the listing declared,
    /// to change before it is written.
    pub fn listed(view: &ProvidersView, account: &str, item: &Discovered) -> Option<Self> {
        let mut form = Self::add(view, account)?;
        form.take_discovered(view, item);
        Some(form)
    }

    /// An edit form filled from a row.
    pub fn edit(view: &ProvidersView, row: &ModelRow) -> Option<Self> {
        let levels = view
            .efforts()
            .iter()
            .map(|level| row.levels.is_empty() || row.levels.contains(level))
            .collect();
        Some(Self {
            editing: Some(row.id.clone()),
            account: row.account.clone(),
            model: row.model.clone(),
            // A row the configuration gave no window stays automatic.
            window: (row.window > 0).then_some(row.window),
            window_typed: None,
            max_tokens_typed: None,
            vision: row.vision,
            effort: row.effort.clone(),
            levels,
            level: 0,
            caret: row.model.len(),
            focus: ModelField::Model,
            known_levels: None,
            discovered_model: None,
            name: None,
            max_tokens: row.max_tokens,
        })
    }

    /// Take a row the picker offered: its id in the model field, and the
    /// context and capability metadata it actually declared. Missing fields
    /// stay unknown; capabilities from a previous pick are never inherited.
    pub fn take_discovered(&mut self, view: &ProvidersView, item: &Discovered) {
        self.model = item.id.clone();
        self.discovered_model = Some(item.id.clone());
        self.name = item.name.clone();
        self.max_tokens = item.max_tokens;
        self.max_tokens_typed = None;
        self.vision = item.vision;
        self.effort = item.effort.clone();
        self.known_levels = item.levels.clone();
        self.levels = view
            .efforts()
            .iter()
            .map(|level| {
                item.levels
                    .as_ref()
                    .is_some_and(|levels| levels.contains(level))
            })
            .collect();
        self.window = None;
        self.window_typed = None;
        self.caret = self.model.len();
        self.focus = ModelField::Model;
        if let Some(window) = item.window {
            if WINDOW_PRESETS.contains(&window) {
                self.window = Some(window);
                self.window_typed = None;
            } else {
                self.window_typed = Some(window.to_string());
            }
        }
    }

    fn clear_discovered(&mut self) {
        if self.discovered_model.take().is_none() {
            return;
        }
        self.name = None;
        self.max_tokens = None;
        self.max_tokens_typed = None;
        self.vision = None;
        self.effort = None;
        self.known_levels = None;
        self.levels.fill(false);
        self.window = None;
        self.window_typed = None;
    }

    pub fn account_id(&self) -> &str {
        &self.account
    }

    /// The fields, in the order Tab walks them: all of them, always. A key is
    /// the connection's, changed on the connection row.
    pub fn fields(&self, view: &ProvidersView) -> Vec<ModelField> {
        let mut out = vec![ModelField::Model, ModelField::Vision, ModelField::Effort];
        if !view.efforts().is_empty() {
            out.push(ModelField::Levels);
        }
        out.push(ModelField::Window);
        out.push(ModelField::Output);
        out
    }

    fn step_focus(&mut self, view: &ProvidersView, forward: bool) {
        let fields = self.fields(view);
        if fields.is_empty() {
            return;
        }
        let at = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (at + 1) % fields.len()
        } else {
            (at + fields.len() - 1) % fields.len()
        };
        self.focus = fields[next];
        self.caret = self.focused_len();
    }

    fn focused_len(&self) -> usize {
        match self.focus {
            ModelField::Model => self.model.len(),
            ModelField::Window => self.window_typed.as_ref().map_or(0, String::len),
            ModelField::Output => self.max_tokens_typed.as_ref().map_or(0, String::len),
            _ => 0,
        }
    }

    /// Auto → on → off → auto. Three states because "decide for me" is not the
    /// same answer as "no", and a two-state toggle would make it one.
    fn cycle_vision(&mut self, forward: bool) {
        self.vision = match (self.vision, forward) {
            (None, true) => Some(true),
            (Some(true), true) => Some(false),
            (Some(false), true) => None,
            (None, false) => Some(false),
            (Some(false), false) => Some(true),
            (Some(true), false) => None,
        };
    }

    /// The windows the arrows step through: automatic, then the presets
    /// largest first, with the one this form holds added in its place when it
    /// is none of them — an edited model keeps the window it had until a
    /// person picks another.
    pub fn window_choices(&self) -> Vec<Option<usize>> {
        let mut presets = WINDOW_PRESETS.to_vec();
        if let Some(window) = self.window.filter(|w| !presets.contains(w)) {
            let at = presets
                .iter()
                .position(|preset| *preset < window)
                .unwrap_or(presets.len());
            presets.insert(at, window);
        }
        // Automatic is offered only where it does something. Saving an edited
        // model with no window keeps the one it has (`Providers::edit_model`),
        // so on an edit form it would be a stop that promises the protocol's
        // default and delivers nothing — a 1m window picked by mistake would
        // stay 1m under a label saying otherwise. A row that somehow has no
        // window yet keeps the stop it is on.
        let automatic = self.editing.is_none() || self.window.is_none();
        automatic
            .then_some(None)
            .into_iter()
            .chain(presets.into_iter().map(Some))
            .collect()
    }

    /// Through [`window_choices`](Self::window_choices), then the custom
    /// stop, then round. Leaving the custom stop drops what was typed there.
    fn cycle_window(&mut self, forward: bool) {
        let choices = self.window_choices();
        // One more stop than there are choices: the custom one, last.
        let stops = choices.len() + 1;
        let at = match self.window_typed {
            Some(_) => choices.len(),
            None => choices.iter().position(|w| *w == self.window).unwrap_or(0),
        };
        let next = if forward {
            (at + 1) % stops
        } else {
            (at + stops - 1) % stops
        };
        match choices.get(next) {
            Some(window) => {
                self.window = *window;
                self.window_typed = None;
            }
            None => self.window_typed = Some(String::new()),
        }
        self.caret = self.focused_len();
    }

    /// A character typed on the window row: part of a custom window when it
    /// can be one — a digit on a preset starts one — and nothing otherwise.
    fn type_window(&mut self, c: char) {
        if !(c.is_ascii_digit() || matches!(c.to_ascii_lowercase(), 'k' | 'm' | '.')) {
            return;
        }
        if self.window_typed.is_none() {
            if !c.is_ascii_digit() {
                return;
            }
            self.window_typed = Some(String::new());
            self.caret = 0;
        }
        if let Some((text, caret)) = self.text_and_caret() {
            insert_at(text, caret, c);
        }
    }

    /// Automatic is always available: unlike the context field it clears the stored override.
    pub fn output_choices(&self) -> Vec<Option<usize>> {
        let mut presets = OUTPUT_PRESETS.to_vec();
        if let Some(value) = self.max_tokens.filter(|value| !presets.contains(value)) {
            presets.push(value);
            presets.sort_unstable();
        }
        std::iter::once(None)
            .chain(presets.into_iter().map(Some))
            .collect()
    }

    fn cycle_output(&mut self, forward: bool) {
        let choices = self.output_choices();
        let stops = choices.len() + 1;
        let at = if self.max_tokens_typed.is_some() {
            choices.len()
        } else {
            choices
                .iter()
                .position(|value| *value == self.max_tokens)
                .unwrap_or(0)
        };
        let next = if forward {
            (at + 1) % stops
        } else {
            (at + stops - 1) % stops
        };
        match choices.get(next) {
            Some(value) => {
                self.max_tokens = *value;
                self.max_tokens_typed = None;
            }
            None => self.max_tokens_typed = Some(String::new()),
        }
        self.caret = self.focused_len();
    }

    fn type_output(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        if self.max_tokens_typed.is_none() {
            self.max_tokens_typed = Some(String::new());
            self.caret = 0;
        }
        if let Some((text, caret)) = self.text_and_caret() {
            insert_at(text, caret, c);
        }
    }

    /// Off, then each level this build offers, then round.
    fn cycle_effort(&mut self, view: &ProvidersView, forward: bool) {
        let mut ring: Vec<Option<String>> = vec![None];
        ring.extend(
            view.efforts()
                .iter()
                .enumerate()
                .filter(|(i, _)| self.known_levels.is_none() || self.levels.get(*i) == Some(&true))
                .map(|(_, level)| Some(level.clone())),
        );
        if ring.len() < 2 {
            return;
        }
        let at = ring.iter().position(|e| *e == self.effort).unwrap_or(0);
        let next = if forward {
            (at + 1) % ring.len()
        } else {
            (at + ring.len() - 1) % ring.len()
        };
        self.effort = ring[next].clone();
    }

    fn text_and_caret(&mut self) -> Option<(&mut String, &mut usize)> {
        let caret = &mut self.caret;
        match self.focus {
            ModelField::Model => Some((&mut self.model, caret)),
            ModelField::Window => self.window_typed.as_mut().map(|typed| (typed, caret)),
            ModelField::Output => self.max_tokens_typed.as_mut().map(|typed| (typed, caret)),
            _ => None,
        }
    }

    pub fn declared_efforts(&self, view: &ProvidersView) -> Vec<String> {
        self.declared_levels(view).unwrap_or_default()
    }

    /// The levels to persist: `None` when every level is on, which is what
    /// "unrestricted" means and what a list of all of them would only pin.
    fn declared_levels(&self, view: &ProvidersView) -> Option<Vec<String>> {
        if let Some(known) = &self.known_levels {
            if view
                .efforts()
                .iter()
                .zip(&self.levels)
                .all(|(level, on)| *on == known.contains(level))
            {
                return Some(known.clone());
            }
        }
        if self.levels.iter().all(|on| *on) || self.levels.iter().all(|on| !*on) {
            return None;
        }
        Some(
            view.efforts()
                .iter()
                .zip(self.levels.iter())
                .filter(|(_, on)| **on)
                .map(|(level, _)| level.clone())
                .collect(),
        )
    }
}

/// The form on screen, when one is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Form {
    Account(AccountForm),
    Model(ModelForm),
}

/// The providers panel, while it is up.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Panel {
    pub card: Option<crate::model_cards::CardPanel>,
    pub screen: Screen,
    /// The provider list was opened from `/model`'s add row: Esc goes back
    /// there rather than closing the panel.
    pub from_models: bool,
    /// The row the arrows are on, as an index into the *listed* rows.
    pub cursor: usize,
    /// What is typed in the search box.
    pub query: String,
    /// The row one more Ctrl+D would delete.
    ///
    /// Two presses, not one: this is the gesture that throws something away, and
    /// a single key that does it is a key somebody hits on the way to something
    /// else. Cleared by everything else, including moving off the row.
    pub pending_delete: Option<String>,
    /// The form over the screen. Leaving it is back on the screen it opened on.
    pub form: Option<Form>,
    /// 正在为某个模型挑思考强度,`None` 就是平常的列表。
    ///
    /// 挑强度不是第二块面板:档位用面板自己那张列表、自己那套按键画出来
    /// (`Listed::Effort`),挑完这一层就收起来。
    pub efforts: Option<EffortPick>,
    /// What each account's endpoint lists, as far as this panel asked: kept
    /// until the panel closes, dropped when the account's connection changes.
    pub discovery: std::collections::BTreeMap<String, DetailDiscovery>,
    /// Server rows whose add is in flight, by (account, model): a second Enter
    /// on one is not a second write.
    pub adding: std::collections::BTreeSet<(String, String)>,
    /// Server rows whose add failed, with why — drawn on the row, which stays
    /// where it is to be tried again.
    pub add_errors: std::collections::BTreeMap<(String, String), String>,
    /// Enter on a server row also takes the session to that model: the person
    /// has no model to talk to (first run, or signed out of the one they had)
    /// and no other add holds that claim. Kept in step by the host, which owns
    /// both facts.
    pub auto_use: bool,
    /// Which opening of the panel this is: an answer meant for an earlier one
    /// must not close or move this one.
    pub instance: u64,
    /// The add-and-use this panel is waiting on, by (request, account, model):
    /// its row says it is switching, and only its answer may close the panel.
    pub awaiting_switch: Option<(u64, String, String)>,
    /// A switch that was refused after its model was saved, by (model id, why):
    /// said on the added row, where Enter tries the switch again.
    pub switch_error: Option<(String, String)>,
}

/// 在面板里为一个刚选中的模型挑思考强度。
///
/// `levels` 已经带上收尾的 `default`(把强度交回端点),所以选中哪一行、
/// 要说什么,是同一句话 —— 派发下去就是 `/effort <那一行>`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffortPick {
    /// 正在为哪个模型挑 —— `/model` 收的那个选择 id。
    pub model: String,
    /// 它声明(或全量)的档位,最后一个是 `default`。
    pub levels: Vec<String>,
}

impl Panel {
    pub fn new() -> Self {
        Self::default()
    }

    /// The panel `/model` opens.
    pub fn models() -> Self {
        Self {
            screen: Screen::Models,
            ..Self::default()
        }
    }

    /// Show a screen, with its own list from the top and no filter.
    ///
    /// The query is dropped rather than kept: a filter typed against account
    /// names hides every model, and a list that came up empty because of
    /// something typed on another screen is a list that looks broken.
    pub fn show(&mut self, screen: Screen) -> bool {
        if self.screen == screen && self.query.is_empty() && self.efforts.is_none() {
            return false;
        }
        self.screen = screen;
        self.cursor = 0;
        self.query.clear();
        self.pending_delete = None;
        self.efforts = None;
        // Leaving the page is leaving the switch it was waiting on: its answer
        // is then only said.
        self.awaiting_switch = None;
        true
    }

    /// Point at a row by index, clamped to what is listed. True when it moved.
    pub fn point_at(&mut self, row: usize, rows: usize) -> bool {
        let want = row.min(rows.saturating_sub(1));
        if self.cursor == want {
            return false;
        }
        self.cursor = want;
        self.pending_delete = None;
        true
    }

    /// Ask for `account`'s listing afresh: a new request, whose answer is the
    /// only one [`Self::discovered`] takes.
    pub fn start_discovery(&mut self, account: &str) -> Step {
        self.discovery.insert(
            account.to_string(),
            DetailDiscovery {
                id: next_discovery_id(),
                state: DiscoveryState::Loading,
            },
        );
        Step::Discover {
            account: account.to_string(),
        }
    }

    /// The request a listing for `account` has to answer.
    pub fn discovery_request(&self, account: &str) -> Option<u64> {
        self.discovery.get(account).map(|d| d.id)
    }

    /// A listing came back. Taken only when it answers the request this panel
    /// is waiting on for that account: a listing for an endpoint since replaced
    /// must not fill the page of the new one. True when it was taken.
    pub fn discovered(
        &mut self,
        view: &ProvidersView,
        account: &str,
        request: u64,
        listed: Result<Vec<Discovered>, String>,
    ) -> bool {
        if self.discovery_request(account) != Some(request) {
            return false;
        }
        let before = self.clone();
        if let Some(entry) = self.discovery.get_mut(account) {
            entry.state = match listed {
                Ok(items) => DiscoveryState::Ready(items),
                Err(why) => DiscoveryState::Failed(why),
            };
        }
        follow_panel(view, &before, self);
        // A fresh page — an account with nothing added yet, the cursor still
        // where the page opened — rests on the first model to add: that is
        // the one thing left to do on it.
        let fresh = !view.models().iter().any(|m| m.account == account)
            && view.listed(&before).get(before.cursor) == Some(&Listed::Connection);
        if fresh && self.screen == Screen::Detail(account.to_string()) {
            if let Some(at) = view
                .listed(self)
                .iter()
                .position(|row| matches!(row, Listed::Available(_)))
            {
                self.cursor = at;
            }
        }
        true
    }

    /// The account's connection changed or it went away: what was listed for
    /// it, and the adds in flight against it, belong to the old one.
    pub fn forget_account(&mut self, account: &str) {
        self.discovery.remove(account);
        self.adding.retain(|(a, _)| a != account);
        self.add_errors.retain(|(a, _), _| a != account);
    }

    /// An add from a server row came back. Clears its in-flight mark; a
    /// failure stays on the row. True when the row was one this panel marked.
    pub fn add_settled(&mut self, account: &str, model: &str, outcome: Result<(), String>) -> bool {
        let key = (account.to_string(), model.to_string());
        if !self.adding.remove(&key) {
            return false;
        }
        match outcome {
            Ok(()) => {
                self.add_errors.remove(&key);
            }
            Err(why) => {
                self.add_errors.insert(key, why);
            }
        }
        true
    }
}

/// What a person typed into an account form, on its way to the port.
///
/// `Debug` redacts the key: this value is carried through a `Step`, and a
/// `Step` is the kind of thing that ends up in a panic message or a test
/// failure. The same rule `ResolvedModelConfig` follows in `atomcode-config`.
#[derive(Clone, PartialEq, Eq)]
pub struct AccountDraft {
    /// What the person called it. Empty on an edit, where the id is fixed.
    pub name: String,
    /// A [`Protocol::id`].
    pub protocol: String,
    pub endpoint: String,
    /// The key as typed. `None` is an empty field on an edit, which means
    /// "leave whatever is stored alone" — and is why this is an option rather
    /// than an empty string, which would mean "take it away".
    pub key: Option<String>,
}

impl std::fmt::Debug for AccountDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountDraft")
            .field("name", &self.name)
            .field("protocol", &self.protocol)
            .field("endpoint", &self.endpoint)
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// What a person typed into a model form, on its way to the port.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ModelDraft {
    pub account: String,
    pub model: String,
    pub name: Option<String>,
    /// `None` selects automatic and clears any saved output override on edit.
    pub max_tokens: Option<usize>,
    pub use_now: bool,
    /// `None` leaves it to whatever this protocol usually holds.
    pub window: Option<usize>,
    pub vision: Option<bool>,
    pub effort: Option<String>,
    /// `None` is unrestricted.
    pub levels: Option<Vec<String>>,
    /// Whether the session should end up on this model.
    pub default: bool,
    /// A key for the account this model is being added to, when it had none.
    pub key: Option<String>,
}

impl std::fmt::Debug for ModelDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelDraft")
            .field("account", &self.account)
            .field("model", &self.model)
            .field("name", &self.name)
            .field("max_tokens", &self.max_tokens)
            .field("use_now", &self.use_now)
            .field("window", &self.window)
            .field("vision", &self.vision)
            .field("effort", &self.effort)
            .field("levels", &self.levels)
            .field("default", &self.default)
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// What a key asked the panel's owner to do.
///
/// Everything that touches the world is here rather than in [`key`], for the
/// reason `crate::settings::Step` exists: the branching is what is worth testing
/// without a screen, and the writing is what only the caller can reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// The panel changed; nothing outside it did.
    Stay,
    /// Put the panel away.
    Close,
    /// Talk to this model from now on — the same gesture `/model <id>` is, and
    /// dispatched as exactly that so a panel and a typed command cannot come to
    /// mean different things.
    Use {
        id: String,
        /// 顺手要设的思考强度,正是 `/effort` 收的那个词(`default` 也在内);
        /// `None` 是不动它。挑强度那一层选完就落在这里。
        effort: Option<String>,
    },
    /// Write an account. `id` is `None` for one that does not exist yet.
    SaveAccount {
        id: Option<String>,
        draft: AccountDraft,
        /// Reuse the draft's successful listing after the explicit write.
        listed: Option<Vec<Discovered>>,
    },
    DiscoverDraft {
        endpoint: String,
    },
    SaveModel {
        id: Option<String>,
        draft: ModelDraft,
    },
    SaveModels {
        drafts: Vec<ModelDraft>,
    },
    DeleteAccount {
        id: String,
    },
    DeleteModel {
        id: String,
    },
    /// List the models `account`'s endpoint offers, for the picker over the
    /// add form. The answer comes back through [`discovered`].
    Discover {
        account: String,
    },
}

/// Run one key against the panel.
///
/// Free-standing and pure, like `crate::settings::key`, so the part with the
/// branching can be tested against a view and a panel with no screen.
///
/// `secret` is the key being typed: the host's `String`, lent for this press.
/// It is here rather than on the form so that the panel — which lives in the
/// `Moment` — cannot hold a credential even by accident; see this module's doc.
pub fn key(view: &ProvidersView, panel: &mut Panel, secret: &mut String, press: KeyPress) -> Step {
    if let Some(card) = &mut panel.card {
        if card.key(press) {
            panel.card = None;
        }
        return Step::Stay;
    }
    match panel.form.clone() {
        Some(Form::Account(form)) => account_key(view, panel, form, secret, press),
        Some(Form::Model(form)) => model_key(view, panel, form, secret, press),
        None => list_key(view, panel, secret, press),
    }
}

/// Leave whatever form is up, forgetting the key that was being typed.
///
/// One function because every way out of a form has to do it: a credential that
/// outlived the form it was typed into would be sent with the next save.
fn leave_form(panel: &mut Panel, secret: &mut String) {
    panel.form = None;
    secret.clear();
}

fn list_key(view: &ProvidersView, panel: &mut Panel, secret: &mut String, press: KeyPress) -> Step {
    let listed = view.listed(panel);
    // 光标可能正停在一条不可选的行上:换画面、改筛选词都会把它留在那儿。
    // 先挪到能停的行再看这一键——否则一按回车什么也不发生,而屏幕上明明有
    // 一行亮着。
    if listed
        .get(panel.cursor)
        .is_some_and(|row| !row.selectable())
    {
        panel.cursor = settle(&listed, panel.cursor);
    }
    let at = listed.get(panel.cursor).copied();
    let armed = panel.pending_delete.take();
    match (press.key, press.mods) {
        (Key::F(2), _) if panel.efforts.is_none() => {
            if let Some(Listed::Model(i) | Listed::Configured(i)) = at {
                if let Some(model) = view.models().get(i) {
                    panel.card = Some(crate::model_cards::CardPanel::new(model));
                }
            }
            Step::Stay
        }
        // 挑强度时 Esc 是「退一层」:回到刚选完模型的那张列表,面板留着。
        (Key::Esc, _) if panel.efforts.is_some() => {
            let chosen = panel.efforts.take().map(|pick| RowId::Model(pick.model));
            panel.query.clear();
            point_at_row(view, panel, chosen);
            Step::Stay
        }
        (Key::Esc, _) => back(view, panel),
        (Key::Char('c'), Mods::CTRL) => Step::Close,
        (Key::Up, _) => {
            panel.cursor = walk(&listed, panel.cursor, false);
            Step::Stay
        }
        (Key::Down, _) => {
            panel.cursor = walk(&listed, panel.cursor, true);
            Step::Stay
        }
        (Key::Home, _) => {
            panel.cursor = settle(&listed, 0);
            Step::Stay
        }
        // The pinned add row is always last: End is the way to it on a list
        // too long to walk.
        (Key::End, _) => {
            panel.cursor = settle(&listed, listed.len().saturating_sub(1));
            Step::Stay
        }
        (Key::Enter, _) => enter_key(view, panel, secret, at, &listed),
        (Key::Char('e'), Mods::CTRL) => {
            begin_edit(view, panel, secret, at);
            Step::Stay
        }
        (Key::Char('d'), Mods::CTRL) => delete_key(view, panel, at, armed),
        (Key::Backspace, _) => {
            panel.query.pop();
            panel.cursor = 0;
            view.settle_cursor(panel);
            Step::Stay
        }
        (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
            panel.query.push(c);
            panel.cursor = 0;
            view.settle_cursor(panel);
            Step::Stay
        }
        _ => Step::Stay,
    }
}

/// Esc on a list: one screen back, the way every panel steps back before it
/// closes.
fn back(view: &ProvidersView, panel: &mut Panel) -> Step {
    match panel.screen.clone() {
        Screen::Models => Step::Close,
        Screen::Providers if panel.from_models => {
            panel.from_models = false;
            panel.show(Screen::Models);
            view.settle_cursor(panel);
            Step::Stay
        }
        Screen::Providers => Step::Close,
        Screen::Detail(account) => {
            panel.show(Screen::Providers);
            point_at_row(view, panel, Some(RowId::Account(account)));
            Step::Stay
        }
    }
}

/// Walk into one account's page, asking its endpoint what it lists the first
/// time this panel goes there.
fn enter_detail(view: &ProvidersView, panel: &mut Panel, account: &str) -> Step {
    panel.show(Screen::Detail(account.to_string()));
    view.settle_cursor(panel);
    let listable = view
        .account(account)
        .is_some_and(|row| row.configured && !row.managed);
    if listable && !panel.discovery.contains_key(account) {
        return panel.start_discovery(account);
    }
    Step::Stay
}

/// Enter: the one thing this row is for.
fn enter_key(
    view: &ProvidersView,
    panel: &mut Panel,
    secret: &mut String,
    at: Option<Listed>,
    listed: &[Listed],
) -> Step {
    match at {
        // 光标停不到这几行上,所以这一支只为穷尽而写。
        Some(Listed::Group(_) | Listed::Section(_) | Listed::Loading) | None => Step::Stay,
        // `/model`'s add row: the provider list, on the account in use. Always
        // the list, even with one account — this may be the time a second is
        // connected.
        Some(Listed::AddModel) => {
            let home = view.home_account().map(str::to_string);
            panel.show(Screen::Providers);
            panel.from_models = true;
            point_at_row(view, panel, home.map(RowId::Account));
            Step::Stay
        }
        Some(Listed::Model(i) | Listed::Configured(i)) => match view.models().get(i) {
            // 这个模型声明了思考强度就接着往下挑:面板不收,把档位换成
            // 这一层的列表(`Panel::efforts`),挑完才一起派发。
            Some(row) => match row.effort_pick() {
                Some(levels) => {
                    panel.efforts = Some(EffortPick {
                        model: row.id.clone(),
                        levels,
                    });
                    panel.query.clear();
                    panel.cursor = 0;
                    Step::Stay
                }
                None => Step::Use {
                    id: row.id.clone(),
                    effort: None,
                },
            },
            None => Step::Stay,
        },
        Some(Listed::Effort(i)) => match panel.efforts.as_ref() {
            Some(pick) => match pick.levels.get(i) {
                Some(level) => Step::Use {
                    id: pick.model.clone(),
                    effort: Some(level.clone()),
                },
                None => Step::Stay,
            },
            None => Step::Stay,
        },
        Some(Listed::Account(i)) => {
            let Some(row) = view.accounts().get(i) else {
                return Step::Stay;
            };
            if !row.configured {
                return connect(view, panel, secret, row);
            }
            let id = row.id.clone();
            enter_detail(view, panel, &id)
        }
        Some(Listed::AddAccount) => {
            secret.clear();
            panel.pending_delete = None;
            panel.form = Some(Form::Account(AccountForm::add(view)));
            Step::Stay
        }
        Some(Listed::Connection) => {
            begin_edit(view, panel, secret, at);
            Step::Stay
        }
        Some(Listed::Failed) => match &panel.screen {
            Screen::Detail(account) => {
                let account = account.clone();
                panel.start_discovery(&account)
            }
            _ => Step::Stay,
        },
        Some(Listed::Available(j)) => {
            let Screen::Detail(account) = panel.screen.clone() else {
                return Step::Stay;
            };
            let Some(item) = view.available(panel, j).cloned() else {
                return Step::Stay;
            };
            let key = (account.clone(), item.id.clone());
            // Already on its way: a second Enter is not a second write — the
            // port refuses a model it already has, and the person would see one
            // add succeed and the next one fail.
            if panel.adding.contains(&key) {
                return Step::Stay;
            }
            panel.adding.insert(key.clone());
            panel.add_errors.remove(&key);
            // On to the next row at once, so a run of Enters adds a run of
            // models. The row being written stays where it is until the write
            // lands; past the last one the cursor is on the manual row, never on
            // an added model a further Enter would switch to.
            panel.cursor = walk(listed, panel.cursor, true);
            let mut draft = ModelDraft::listed(&account, &item);
            // The one add-and-use: this Enter takes the claim, so the next
            // server row says 「添加」 and only saves.
            draft.use_now = std::mem::take(&mut panel.auto_use);
            Step::SaveModel { id: None, draft }
        }
        Some(Listed::Manual) => {
            let Screen::Detail(account) = panel.screen.clone() else {
                return Step::Stay;
            };
            secret.clear();
            panel.pending_delete = None;
            if let Some(mut form) = ModelForm::add(view, &account) {
                // What the filter holds is what was being looked for, and the
                // listing did not have it: it is the id to add.
                let typed = panel.query.trim();
                if !typed.is_empty() {
                    form.model = typed.to_string();
                    form.caret = form.model.len();
                }
                panel.form = Some(Form::Model(form));
            }
            Step::Stay
        }
    }
}

/// Connecting an account the build knows about but nobody has set up.
fn connect(view: &ProvidersView, panel: &mut Panel, secret: &mut String, row: &AccountRow) -> Step {
    secret.clear();
    panel.pending_delete = None;
    let mut form = AccountForm::connect(view, row);
    let step = form.start_discovery(view);
    panel.form = Some(Form::Account(form));
    step
}

/// Ctrl+E: open the form that changes the thing under the cursor, when it is
/// one this panel may change.
fn begin_edit(view: &ProvidersView, panel: &mut Panel, secret: &mut String, at: Option<Listed>) {
    secret.clear();
    panel.pending_delete = None;
    let account_form = |row: &AccountRow| {
        Form::Account(if row.configured {
            AccountForm::edit(view, row)
        } else {
            AccountForm::connect(view, row)
        })
    };
    let form = match at {
        Some(Listed::Account(i)) => view
            .accounts()
            .get(i)
            .filter(|row| !row.managed)
            .map(account_form),
        Some(Listed::Connection) => match &panel.screen {
            Screen::Detail(account) => view
                .account(account)
                .filter(|row| !row.managed)
                .map(account_form),
            _ => None,
        },
        Some(Listed::Model(i) | Listed::Configured(i)) => view
            .models()
            .get(i)
            .filter(|row| !row.managed)
            .and_then(|row| ModelForm::edit(view, row))
            .map(Form::Model),
        Some(Listed::Available(j)) => match &panel.screen {
            Screen::Detail(account) => view
                .available(panel, j)
                .and_then(|item| ModelForm::listed(view, account, item))
                .map(Form::Model),
            _ => None,
        },
        _ => None,
    };
    if form.is_some() {
        panel.form = form;
    }
}

/// The two-press delete. The first press arms the row; the second is the one
/// that asks for the write.
fn delete_key(
    view: &ProvidersView,
    panel: &mut Panel,
    at: Option<Listed>,
    armed: Option<String>,
) -> Step {
    let (id, account) = match at {
        Some(Listed::Account(i)) => match view.accounts().get(i) {
            // An offer that was never configured has nothing to delete.
            Some(row) if !row.managed && row.configured => (row.id.clone(), true),
            _ => return Step::Stay,
        },
        Some(Listed::Model(i) | Listed::Configured(i)) => match view.models().get(i) {
            Some(row) if !row.managed => (row.id.clone(), false),
            _ => return Step::Stay,
        },
        _ => return Step::Stay,
    };
    if armed.as_deref() != Some(id.as_str()) {
        panel.pending_delete = Some(id);
        return Step::Stay;
    }
    match account {
        true => Step::DeleteAccount { id },
        false => Step::DeleteModel { id },
    }
}

fn account_key(
    view: &ProvidersView,
    panel: &mut Panel,
    mut form: AccountForm,
    secret: &mut String,
    press: KeyPress,
) -> Step {
    let previous_protocol = form.protocol;
    let previous_endpoint = form.endpoint.clone();
    let previous_focus = form.focus;
    match (press.key, press.mods) {
        (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => {
            leave_form(panel, secret);
            return Step::Stay;
        }
        // Enter is save — except on the 「更多设置 ▸」 row, where it shows the
        // rest of the form, landing on the first field that was not there.
        (Key::Enter, _) if form.focus == AccountField::More => {
            let short = form.fields(view);
            form.advanced = true;
            form.focus = form
                .fields(view)
                .into_iter()
                .find(|field| !short.contains(field))
                .unwrap_or(AccountField::Endpoint);
            form.caret = form.focused_len();
        }
        (Key::Enter, _) => return save_account(view, panel, &form, secret),
        (Key::Tab, _) | (Key::Down, _) => form.step_focus(view, true),
        (Key::BackTab, _) | (Key::Up, _) => form.step_focus(view, false),
        (Key::Left, _) if form.focus == AccountField::Protocol => form.cycle_protocol(view, false),
        (Key::Right, _) if form.focus == AccountField::Protocol => form.cycle_protocol(view, true),
        (Key::Left, _) | (Key::Right, _) => {
            let forward = press.key == Key::Right;
            if let Some((text, caret)) = form.text_and_caret() {
                *caret = step_caret(text, *caret, forward);
            }
        }
        (Key::Home, _) => {
            if form.text_and_caret().is_some() {
                form.caret = 0;
            }
        }
        (Key::End, _) => {
            if let Some((text, caret)) = form.text_and_caret() {
                *caret = text.len();
            }
        }
        (Key::Backspace, _) => {
            if form.focus == AccountField::Key {
                secret.pop();
                form.key_len = secret.chars().count();
            } else if let Some((text, caret)) = form.text_and_caret() {
                backspace_at(text, caret);
            }
        }
        (Key::Delete, _) => {
            if form.focus == AccountField::Key {
                secret.clear();
                form.key_len = 0;
            } else if let Some((text, caret)) = form.text_and_caret() {
                delete_at(text, caret);
            }
        }
        (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
            if form.focus == AccountField::Key {
                secret.push(c);
                form.key_len = secret.chars().count();
            } else if let Some((text, caret)) = form.text_and_caret() {
                insert_at(text, caret, c);
            }
        }
        _ => {}
    }
    if form.protocol != previous_protocol
        && !form
            .preset
            .as_deref()
            .is_some_and(|id| view.connections(id).len() > 1)
    {
        secret.clear();
        form.key_len = 0;
    }
    if form.endpoint != previous_endpoint || form.protocol != previous_protocol {
        // Editing immediately invalidates a request, before a replacement is sent.
        form.discovery = None;
    }
    // Leaving the address asks again whenever there is no good listing for
    // it — none yet, or one that failed (`ollama serve` not started, say):
    // that is how a person retries here, with no key of its own to learn.
    let discover = form.protocol != previous_protocol
        || (previous_focus == AccountField::Endpoint
            && form.focus != previous_focus
            && form
                .discovery
                .as_ref()
                .is_none_or(|d| matches!(d.result, Some(Err(_)))));
    let step = if discover {
        form.start_discovery(view)
    } else {
        Step::Stay
    };
    panel.form = Some(Form::Account(form));
    step
}

/// Hand the account form to the port, when it has enough to write.
///
/// A form that is not ready stays open on the field that is missing rather than
/// closing with a refusal: the person is already typing, and the thing to do
/// with a missing name is to ask for it where they are looking.
fn save_account(
    view: &ProvidersView,
    panel: &mut Panel,
    form: &AccountForm,
    secret: &mut String,
) -> Step {
    let name = form.name.trim().to_string();
    if form.editing.is_none() && name.is_empty() {
        let mut form = form.clone();
        form.focus = AccountField::Name;
        panel.form = Some(Form::Account(form));
        return Step::Stay;
    }
    let Some(protocol) = view.protocols().get(form.protocol) else {
        return Step::Stay;
    };
    let endpoint = form.endpoint.trim().to_string();
    if endpoint.is_empty() && protocol.endpoint.is_none() {
        let mut form = form.clone();
        form.focus = AccountField::Endpoint;
        panel.form = Some(Form::Account(form));
        return Step::Stay;
    }
    if form.editing.is_none() && form.needs_key(view) && secret.trim().is_empty() {
        let mut form = form.clone();
        form.focus = AccountField::Key;
        panel.form = Some(Form::Account(form));
        return Step::Stay;
    }
    let key =
        (form.needs_key(view) && !secret.trim().is_empty()).then(|| secret.trim().to_string());
    let draft = AccountDraft {
        name,
        protocol: form
            .preset
            .as_deref()
            .and_then(|id| {
                view.connections(id)
                    .iter()
                    .find(|option| option.protocol.label == protocol.label)
            })
            .map(|option| option.protocol.id.clone())
            .or_else(|| form.preset.clone())
            .unwrap_or_else(|| protocol.id.clone()),
        endpoint,
        key,
    };
    let id = form.editing.clone();
    let listed = form
        .discovery
        .as_ref()
        .and_then(|d| d.result.as_ref())
        .and_then(|result| result.as_ref().ok())
        .cloned();
    leave_form(panel, secret);
    Step::SaveAccount { id, draft, listed }
}

fn model_key(
    view: &ProvidersView,
    panel: &mut Panel,
    mut form: ModelForm,
    secret: &mut String,
    press: KeyPress,
) -> Step {
    let previous = (form.account_id().to_string(), form.model.clone());
    match (press.key, press.mods) {
        (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => {
            leave_form(panel, secret);
            return Step::Stay;
        }
        (Key::Enter, _) => return save_model(view, panel, &form, secret),
        (Key::Tab, _) | (Key::Down, _) => form.step_focus(view, true),
        (Key::BackTab, _) | (Key::Up, _) => form.step_focus(view, false),
        (Key::Left, _) | (Key::Right, _) => {
            let forward = press.key == Key::Right;
            match form.focus {
                ModelField::Vision => form.cycle_vision(forward),
                ModelField::Effort => form.cycle_effort(view, forward),
                ModelField::Window => form.cycle_window(forward),
                ModelField::Output => form.cycle_output(forward),
                ModelField::Levels => {
                    let len = form.levels.len().max(1);
                    form.level = match forward {
                        true => (form.level + 1) % len,
                        false => (form.level + len - 1) % len,
                    };
                }
                _ => {
                    if let Some((text, caret)) = form.text_and_caret() {
                        *caret = step_caret(text, *caret, forward);
                    }
                }
            }
        }
        (Key::Home, _) => {
            if form.text_and_caret().is_some() {
                form.caret = 0;
            }
        }
        (Key::End, _) => {
            if let Some((text, caret)) = form.text_and_caret() {
                *caret = text.len();
            }
        }
        // One meaning per key: ←→ change a value, Space turns the bracketed
        // reasoning level on or off — on that row only. Elsewhere it is a
        // space in a text field, or nothing: a second key that also changed
        // values was a second thing to learn about every row.
        (Key::Char(' '), Mods::NONE) => match form.focus {
            ModelField::Levels => {
                if let Some(on) = form.levels.get_mut(form.level) {
                    *on = !*on;
                }
            }
            _ => {
                if let Some((text, caret)) = form.text_and_caret() {
                    insert_at(text, caret, ' ');
                }
            }
        },
        (Key::Backspace, _) => {
            if let Some((text, caret)) = form.text_and_caret() {
                backspace_at(text, caret);
            }
        }
        (Key::Delete, _) => {
            if let Some((text, caret)) = form.text_and_caret() {
                delete_at(text, caret);
            }
        }
        (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
            if form.focus == ModelField::Window {
                form.type_window(c);
            } else if form.focus == ModelField::Output {
                form.type_output(c);
            } else if let Some((text, caret)) = form.text_and_caret() {
                insert_at(text, caret, c);
            }
        }
        _ => {}
    }
    if previous != (form.account_id().to_string(), form.model.clone()) {
        form.clear_discovered();
    }
    panel.form = Some(Form::Model(form));
    Step::Stay
}

fn save_model(
    view: &ProvidersView,
    panel: &mut Panel,
    form: &ModelForm,
    secret: &mut String,
) -> Step {
    let model = form.model.trim().to_string();
    if model.is_empty() {
        let mut form = form.clone();
        form.focus = ModelField::Model;
        panel.form = Some(Form::Model(form));
        return Step::Stay;
    }
    let account = form.account_id().to_string();
    if account.is_empty() {
        return Step::Stay;
    }
    // A custom window that will not parse is a typo, not a zero, and not
    // "automatic" either: the form goes back to it rather than saving a window
    // nobody chose.
    let window = match &form.window_typed {
        Some(typed) => match parse_window(typed) {
            Some(window) => Some(window),
            None => {
                let mut form = form.clone();
                form.focus = ModelField::Window;
                form.caret = typed.len();
                panel.form = Some(Form::Model(form));
                return Step::Stay;
            }
        },
        None => form.window,
    };
    let max_tokens = match &form.max_tokens_typed {
        Some(typed) => match parse_output_tokens(typed) {
            Some(value) => Some(value),
            None => {
                let mut form = form.clone();
                form.focus = ModelField::Output;
                form.caret = typed.len();
                panel.form = Some(Form::Model(form));
                return Step::Stay;
            }
        },
        None => form.max_tokens,
    };
    let draft = ModelDraft {
        account,
        model,
        name: form.name.clone(),
        max_tokens,
        // The session's model and the file's default are changed by switching,
        // which persists itself (`HostCommand::SwitchModel`) — never by saving
        // a model. A form that could set either was a second way to do it.
        use_now: false,
        window,
        vision: form.vision,
        effort: form.effort.clone(),
        levels: form.declared_levels(view),
        default: false,
        key: None,
    };
    let id = form.editing.clone();
    leave_form(panel, secret);
    Step::SaveModel { id, draft }
}

/// Put pasted text into the field that has the keyboard.
///
/// **One line of it.** Every field on these forms is one line — an id, a URL, a
/// key, a model name — and a newline that made it into one would be a newline
/// written into the configuration file. tuix normalises the same way, and for
/// the same reason: an accidental trailing newline off a web page must not end
/// up inside a TOML string.
///
/// True when something changed, so the caller knows whether a frame is owed.
/// False when no form is up, which is the case that must fall through: a paste
/// into the list is a paste into the search box, and that is a search.
pub fn paste(panel: &mut Panel, secret: &mut String, text: &str) -> bool {
    let line = text
        .replace("\r\n", "\n")
        .split('\n')
        .next()
        .unwrap_or_default()
        .to_string();
    if line.is_empty() {
        return false;
    }
    match panel.form.as_mut() {
        Some(Form::Account(form)) => {
            if form.focus == AccountField::Key {
                secret.push_str(&line);
                form.key_len = secret.chars().count();
                return true;
            }
            let Some((field, caret)) = form.text_and_caret() else {
                return false;
            };
            let at = snap(field, *caret);
            field.insert_str(at, &line);
            *caret = at + line.len();
            if form.focus == AccountField::Endpoint {
                form.discovery = None;
            }
            true
        }
        Some(Form::Model(form)) => {
            // The window row takes a paste the way it takes typing — one
            // character at a time through the same filter, which is also what
            // starts a custom window when a preset has the keyboard.
            if form.focus == ModelField::Window {
                let before = form.window_typed.clone();
                for c in line.trim().chars() {
                    form.type_window(c);
                }
                return form.window_typed != before;
            }
            if form.focus == ModelField::Output {
                let before = form.max_tokens_typed.clone();
                for c in line.trim().chars() {
                    form.type_output(c);
                }
                return form.max_tokens_typed != before;
            }
            let model_field = form.focus == ModelField::Model;
            let Some((field, caret)) = form.text_and_caret() else {
                return false;
            };
            let at = snap(field, *caret);
            field.insert_str(at, &line);
            *caret = at + line.len();
            if model_field {
                form.clear_discovered();
            }
            true
        }
        // No form: the list's own box is a filter, and pasting into a filter is
        // searching for what was pasted.
        None => {
            panel.query.push_str(&line);
            panel.cursor = 0;
            true
        }
    }
}

/// Reading the providers, and changing them.
///
/// Filled by whoever launches the screen, the same shape `crate::settings::Settings`
/// has and for the same reason: what the screen knows about the product came
/// over a seam, not out of a service of the product's own (`docs/adr/0022` §3).
///
/// Every write answers with `Result<_, String>` and the message is drawn as it
/// stands — a refusal belongs to whoever knows the configuration's rules, and
/// this side has no business rewording it.
pub trait Providers: Send + Sync {
    /// The accounts and models as they are now.
    ///
    /// Asked when the panel opens and after every write, never per frame: the
    /// port reads a file, and a `render` that did filesystem work would break
    /// the purity the whole crate rests on.
    fn rows(&self) -> ProvidersView;

    /// Write a new account, answering with the id it landed under so the panel
    /// can walk straight into its (empty) model list.
    fn add_account(&self, draft: &AccountDraft) -> Result<String, String>;

    /// Change one that exists. An absent `draft.key` leaves the stored
    /// credential alone.
    fn edit_account(&self, id: &str, draft: &AccountDraft) -> Result<(), String>;

    /// Take an account out, with the models that hang off it.
    fn delete_account(&self, id: &str) -> Result<(), String>;

    /// Add a model to an account, answering with its selection id.
    fn add_model(&self, draft: &ModelDraft) -> Result<String, String>;

    /// Save a selection in one transaction; ports must not partially write a batch.
    fn add_models(&self, _drafts: &[ModelDraft]) -> Result<Vec<String>, String> {
        Err("Batch model saving is unavailable".into())
    }

    /// Resolve public catalog metadata before the synchronous config write.
    fn enrich_models(&self, _drafts: &[ModelDraft]) -> Option<EnrichModelsFuture> {
        None
    }

    fn edit_model(&self, id: &str, draft: &ModelDraft) -> Result<(), String>;

    fn delete_model(&self, id: &str) -> Result<(), String>;

    /// Check that an account just saved answers — and, when a model was the
    /// thing saved, that the model exists — so a wrong base_url is caught where
    /// it was typed rather than as a failed turn later. `selection` is the
    /// model's selection id when a model was saved.
    ///
    /// The answer is what to tell the person, and whether it is fine. It is a
    /// future because it is a network round trip; the screen runs it off the
    /// frame and says the answer when it lands. `None` when there is nothing
    /// this port checks (a protocol it does not probe, no endpoint yet) — the
    /// default, so a port need not know probing exists.
    fn probe(&self, _account: &str, _selection: Option<&str>) -> Option<ProbeFuture> {
        None
    }

    /// The models `account`'s endpoint lists, for the picker over the add form
    /// — asked of its saved address and key, so nothing is typed twice. `Err`
    /// is what to tell the person. `None` when this port cannot list models at
    /// all; the default, so a port need not know listing exists.
    fn discover(&self, _account: &str) -> Option<DiscoverFuture> {
        None
    }

    /// Read-only Ollama discovery before the account has been saved.
    fn discover_ollama(&self, _endpoint: &str) -> Option<DiscoverFuture> {
        None
    }
}

/// See [`Providers::discover`].
pub type DiscoverFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Vec<Discovered>, String>> + Send + 'static>,
>;

pub type EnrichModelsFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Vec<ModelDraft>> + Send + 'static>>;

/// See [`Providers::probe`]: what to say, and whether all is well.
pub type ProbeFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = (String, bool)> + Send + 'static>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_steps_over_declared_levels_with_default_last() {
        let declared = Some(vec![
            "low".into(),
            "medium".into(),
            "high".into(),
            "default".into(),
        ]);

        assert_eq!(
            effort_step("medium", declared.clone(), true).as_deref(),
            Some("high")
        );
        assert_eq!(
            effort_step("medium", declared.clone(), false).as_deref(),
            Some("low")
        );
        // The ends wrap: the last declared level lands on the endpoint's
        // default, and stepping on from there re-enters the list.
        assert_eq!(
            effort_step("high", declared.clone(), true).as_deref(),
            Some("default")
        );
        assert_eq!(
            effort_step("default", declared.clone(), true).as_deref(),
            Some("low")
        );
        assert_eq!(
            effort_step("default", declared.clone(), false).as_deref(),
            Some("high")
        );
    }

    #[test]
    fn effort_steps_into_the_list_when_the_level_in_force_is_not_declared() {
        let declared = Some(vec!["low".into(), "high".into(), "default".into()]);

        // A config edit since the session started left `max` in force: step in
        // from the near end instead of guessing a middle rung.
        assert_eq!(
            effort_step("max", declared.clone(), true).as_deref(),
            Some("low")
        );
        assert_eq!(
            effort_step("max", declared, false).as_deref(),
            Some("default")
        );
    }

    #[test]
    fn effort_steps_nowhere_without_declared_levels() {
        assert_eq!(effort_step("high", None, true), None);
        assert_eq!(effort_step("default", Some(vec![]), false), None);
    }

    fn account(id: &str, models: usize) -> AccountRow {
        AccountRow {
            id: id.into(),
            label: id.into(),
            protocol: "OpenAI".into(),
            endpoint: "https://api.example.com/v1".into(),
            models,
            has_key: true,
            managed: false,
            configured: true,
        }
    }

    fn model(id: &str, account: &str) -> ModelRow {
        ModelRow {
            id: id.into(),
            account: account.into(),
            model: id.into(),
            window: 128_000,
            max_tokens: None,
            vision: None,
            effort: None,
            levels: Vec::new(),
            current: false,
            managed: false,
            alias: None,
            desc: None,
            alias_en: None,
            desc_en: None,
        }
    }

    #[test]
    fn defaults_and_fixed_values_do_not_invent_a_level_catalog() {
        for effort in [None, Some("auto"), Some("default"), Some("high")] {
            let mut row = model("custom", "a");
            row.effort = effort.map(str::to_string);
            assert!(row.effort_pick().is_none(), "{effort:?}");
        }
        let mut row = model("custom", "a");
        row.effort = Some("auto".into());
        row.levels = vec!["HIGH".into(), "unknown".into(), "low".into()];
        assert_eq!(row.effort_pick().unwrap(), ["low", "high", "default"]);
        row.levels = vec!["unknown".into()];
        assert!(row.effort_pick().is_none());
    }

    /// Stepping between models with one key, in both directions, wrapping.
    ///
    /// Judged on the list rather than through the key, because "which model is
    /// next" is the whole of the decision and the key cannot be run without a
    /// session, a port and a panel.
    #[test]
    fn stepping_through_the_models_goes_both_ways_and_comes_back_round() {
        let ids = |v: &ProvidersView, forward| v.model_after(forward).map(|m| m.id.clone());

        let mut rows = vec![model("a/1", "a"), model("b/1", "b"), model("b/2", "b")];
        rows[1].current = true;
        let v = ProvidersView::new(
            vec![account("a", 1), account("b", 2)],
            rows,
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(ids(&v, true), Some("b/2".into()), "forward is the next one");
        assert_eq!(ids(&v, false), Some("a/1".into()), "back is the one before");

        // From the last one, forward wraps — a key that dead-ends at the end of
        // the list is a key people press twice and then give up on.
        let mut rows = vec![model("a/1", "a"), model("b/1", "b")];
        rows[1].current = true;
        let v = ProvidersView::new(
            vec![account("a", 1), account("b", 1)],
            rows,
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(ids(&v, true), Some("a/1".into()), "it wraps round");

        // Nowhere to go: one model, or none. The key does nothing rather than
        // switching to the model already in use.
        let only = vec![model("a/1", "a")];
        let v = ProvidersView::new(vec![account("a", 1)], only, Vec::new(), Vec::new());
        assert_eq!(ids(&v, true), None, "one model is nowhere to step");
        let v = ProvidersView::new(Vec::new(), Vec::new(), Vec::new(), Vec::new());
        assert_eq!(ids(&v, true), None, "and neither is none");

        // Nothing marked current — a session on a model no longer in the list.
        // It starts at the first rather than refusing: the current one is not
        // somewhere this list can step from.
        let v = ProvidersView::new(
            vec![account("a", 1), account("b", 1)],
            vec![model("a/1", "a"), model("b/1", "b")],
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(ids(&v, true), Some("a/1".into()));
    }

    /// The reported `F2`: it named the model it started on, every press, and
    /// switched nothing.
    ///
    /// The screen knows what the session is on by the name the description
    /// reports — the model **sent on the wire** — while a row is keyed by its
    /// selection id. Those differ for every aliased model (`AtomGit-deepseek-flash`
    /// sends `deepseek-flash`, which is the name on the status line), so nothing
    /// matched, no row was marked, and `model_after` fell back to the first row
    /// every time.
    #[test]
    fn the_row_in_use_is_found_by_the_name_the_description_reports() {
        let wire = |id: &str, sent: &str, account: &str| ModelRow {
            model: sent.into(),
            ..model(id, account)
        };
        let view = ProvidersView::new(
            vec![account("AtomGit", 2), account("grok", 1)],
            vec![
                wire("AtomGit-deepseek-flash", "deepseek-flash", "AtomGit"),
                wire("AtomGit-glm5.3-flash", "glm5.3-flash", "AtomGit"),
                wire("grok/grok-4.6", "grok-4.6", "grok"),
            ],
            Vec::new(),
            Vec::new(),
        );

        let live = view.with_current(Some("deepseek-flash"));
        let marked: Vec<&str> = live
            .models()
            .iter()
            .filter(|m| m.current)
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(
            marked,
            ["AtomGit-deepseek-flash"],
            "one row, the one in use"
        );
        assert_eq!(
            live.model_after(true).map(|m| m.id.clone()),
            Some("AtomGit-glm5.3-flash".into()),
            "F2 steps off it rather than naming it again"
        );
        assert_eq!(
            live.model_after(false).map(|m| m.id.clone()),
            Some("grok/grok-4.6".into()),
            "and back wraps round the list"
        );

        // The selection id is the other name the screen could hand over, and it
        // is answered first: a row knows both.
        let by_id = view.with_current(Some("AtomGit-glm5.3-flash"));
        assert_eq!(
            by_id.model_after(true).map(|m| m.id.clone()),
            Some("grok/grok-4.6".into())
        );
    }

    /// No description yet — the first frames of a session — is not a reason to
    /// forget the row the *file* starts on. Marking nothing is what left `F2`
    /// stepping from nowhere and landing on the first row: the same reported
    /// symptom, reached another way.
    #[test]
    fn nothing_to_mark_leaves_the_row_the_file_starts_on_marked() {
        let mut rows = vec![model("a/1", "a"), model("b/1", "b")];
        rows[0].current = true;
        let view = ProvidersView::new(
            vec![account("a", 1), account("b", 1)],
            rows,
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(
            view.with_current(None)
                .model_after(true)
                .map(|m| m.id.clone()),
            Some("b/1".into()),
            "it still steps from the row the file named"
        );

        // And a name no row answers to is the same case: a session on a model
        // that is no longer in the list steps from where the list says it is.
        assert_eq!(
            view.with_current(Some("gone/away"))
                .model_after(true)
                .map(|m| m.id.clone()),
            Some("b/1".into())
        );
    }

    fn view() -> ProvidersView {
        ProvidersView::new(
            vec![account("deepseek", 1), account("local", 2)],
            vec![
                model("deepseek/chat", "deepseek"),
                model("local/a", "local"),
                model("local/b", "local"),
            ],
            vec![
                Protocol {
                    id: "openai-compatible".into(),
                    label: "OpenAI".into(),
                    endpoint: Some("https://api.openai.com/v1".into()),
                    needs_key: true,
                },
                Protocol {
                    id: "ollama".into(),
                    label: "Ollama".into(),
                    endpoint: Some("http://localhost:11434".into()),
                    needs_key: false,
                },
            ],
            vec!["low".into(), "medium".into(), "high".into()],
        )
    }

    fn press(key: Key) -> KeyPress {
        KeyPress::plain(key)
    }

    #[test]
    fn a_vendor_offer_exposes_protocol_and_preserves_plan_key_and_custom_address() {
        let original = view();
        let mut protocols = original.protocols().to_vec();
        protocols.push(Protocol {
            id: "anthropic-compatible".into(),
            label: "Anthropic".into(),
            endpoint: None,
            needs_key: true,
        });
        let view = ProvidersView::new(original.accounts().to_vec(), vec![], protocols, vec![])
            .with_connections(vec![(
                "deepseek".into(),
                vec![
                    PresetConnection {
                        protocol: Protocol {
                            id: "deepseek".into(),
                            label: "OpenAI".into(),
                            endpoint: Some("https://vendor.test/v1".into()),
                            needs_key: true,
                        },
                        base_urls: vec![
                            "https://vendor.test/v1".into(),
                            "https://vendor.test/plan/v1".into(),
                        ],
                    },
                    PresetConnection {
                        protocol: Protocol {
                            id: "anthropic-compatible".into(),
                            label: "Anthropic".into(),
                            endpoint: Some("https://vendor.test".into()),
                            needs_key: true,
                        },
                        base_urls: vec![
                            "https://vendor.test".into(),
                            "https://vendor.test/plan".into(),
                        ],
                    },
                ],
            )]);
        for (input, expected) in [
            ("https://vendor.test/plan/v1/", "https://vendor.test/plan"),
            ("https://custom.test/route", "https://custom.test/route"),
        ] {
            let mut form = AccountForm::connect(&view, view.account("deepseek").unwrap());
            form.endpoint = input.into();
            form.key_len = 8;
            assert!(!form.advanced);
            assert_eq!(
                form.fields(&view),
                vec![
                    AccountField::Protocol,
                    AccountField::Endpoint,
                    AccountField::Key,
                    AccountField::More
                ]
            );
            let mut panel = Panel {
                form: Some(Form::Account(form)),
                ..Panel::new()
            };
            let mut secret = "test-key".to_string();
            key(&view, &mut panel, &mut secret, press(Key::Right));
            assert_eq!(secret, "test-key");
            let Step::SaveAccount { draft, .. } =
                key(&view, &mut panel, &mut secret, press(Key::Enter))
            else {
                panic!("save account")
            };
            assert_eq!(draft.name, "deepseek");
            assert_eq!(draft.protocol, "anthropic-compatible");
            assert_eq!(draft.endpoint, expected);
            assert_eq!(draft.key.as_deref(), Some("test-key"));
        }
    }

    fn ctrl(c: char) -> KeyPress {
        KeyPress {
            key: Key::Char(c),
            mods: Mods::CTRL,
        }
    }

    fn run(
        view: &ProvidersView,
        panel: &mut Panel,
        secret: &mut String,
        keys: &[KeyPress],
    ) -> Step {
        let mut last = Step::Stay;
        for k in keys {
            last = key(view, panel, secret, *k);
        }
        last
    }

    /// The rule this whole module is shaped around: a key a person types is
    /// counted by the panel and held by the host, and the panel — which lives in
    /// the `Moment` — cannot be made to show it. Delete the two `key_len` arms
    /// in `account_key` and put the characters on the form, and this fails.
    #[test]
    fn a_typed_key_never_enters_the_panel() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        // Walk to the add row, open the form, type a name, tab past the
        // protocol and the endpoint, and type a key.
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::End), press(Key::Enter)],
        );
        run(
            &view,
            &mut panel,
            &mut secret,
            &[
                press(Key::Char('m')),
                press(Key::Tab),
                press(Key::Tab),
                press(Key::Tab),
            ],
        );
        let Some(Form::Account(form)) = panel.form.clone() else {
            panic!("the add form is up: {:?}", panel.form);
        };
        assert_eq!(form.focus, AccountField::Key, "on the key field");
        run(
            &view,
            &mut panel,
            &mut secret,
            &[
                press(Key::Char('s')),
                press(Key::Char('k')),
                press(Key::Char('-')),
                press(Key::Char('9')),
            ],
        );
        assert_eq!(secret, "sk-9", "the host holds the characters");
        let drawn = format!("{panel:?}");
        assert!(
            !drawn.contains("sk-9") && !drawn.contains("sk-"),
            "the panel must not carry the key: {drawn}"
        );
        let Some(Form::Account(form)) = panel.form.clone() else {
            panic!("still in the form");
        };
        assert_eq!(form.key_len, 4, "it counts what it will not hold");
    }

    /// And a `Step` carrying one says nothing either — a step is what ends up in
    /// a panic message or a failed assertion.
    #[test]
    fn a_draft_redacts_the_key_it_carries() {
        let draft = AccountDraft {
            name: "mine".into(),
            protocol: "openai-compatible".into(),
            endpoint: "https://api.example.com/v1".into(),
            key: Some("sk-secret".into()),
        };
        let drawn = format!("{draft:?}");
        assert!(!drawn.contains("sk-secret"), "{drawn}");
        assert!(drawn.contains("redacted"), "{drawn}");
    }

    #[test]
    fn leaving_a_form_forgets_the_key() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::End), press(Key::Enter)],
        );
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Tab), press(Key::Tab), press(Key::Tab)],
        );
        run(&view, &mut panel, &mut secret, &[press(Key::Char('x'))]);
        assert_eq!(secret, "x");
        run(&view, &mut panel, &mut secret, &[press(Key::Esc)]);
        assert!(panel.form.is_none(), "the form is gone");
        assert!(secret.is_empty(), "and so is what was typed into it");
    }

    #[test]
    fn choosing_a_model_is_the_same_gesture_as_typing_the_command() {
        let view = view();
        let mut panel = Panel::models();
        let mut secret = String::new();
        let step = run(&view, &mut panel, &mut secret, &[press(Key::Enter)]);
        assert_eq!(
            step,
            Step::Use {
                id: "deepseek/chat".into(),
                effort: None,
            }
        );
    }

    #[test]
    fn a_model_with_levels_asks_for_one_in_the_same_list() {
        let view = ProvidersView::new(
            vec![account("deepseek", 1)],
            vec![ModelRow {
                levels: vec!["low".into(), "high".into()],
                ..model("deepseek/chat", "deepseek")
            }],
            Vec::new(),
            vec!["low".into(), "high".into()],
        );
        let mut panel = Panel::models();
        let mut secret = String::new();

        // 在模型行上按回车:面板不收,列表换成它声明的档位,`default` 收尾 ——
        // 挑强度是同一张列表的下一层,不是一个浮起来的弹窗。
        assert_eq!(
            run(&view, &mut panel, &mut secret, &[press(Key::Enter)]),
            Step::Stay
        );
        assert_eq!(
            view.listed(&panel),
            vec![Listed::Effort(0), Listed::Effort(1), Listed::Effort(2)]
        );

        // 挑一档:模型和强度一起出去,人不用再打第二条命令。
        assert_eq!(
            run(
                &view,
                &mut panel,
                &mut secret,
                &[press(Key::Down), press(Key::Enter)]
            ),
            Step::Use {
                id: "deepseek/chat".into(),
                effort: Some("high".into()),
            }
        );

        // Esc 是「退一层」:回到刚选完模型的那张列表,面板还开着。
        assert_eq!(
            run(&view, &mut panel, &mut secret, &[press(Key::Esc)]),
            Step::Stay
        );
        assert!(panel.efforts.is_none());
        assert!(
            view.listed(&panel)
                .iter()
                .any(|row| matches!(row, Listed::Model(_))),
            "退回到模型列表"
        );
    }

    #[test]
    fn backing_out_of_the_effort_pick_lands_on_the_model_just_chosen() {
        let view = ProvidersView::new(
            vec![account("deepseek", 2)],
            vec![
                model("deepseek/chat", "deepseek"),
                ModelRow {
                    levels: vec!["low".into(), "high".into()],
                    ..model("deepseek/reasoner", "deepseek")
                },
            ],
            Vec::new(),
            vec!["low".into(), "high".into()],
        );
        let mut panel = Panel::models();
        let mut secret = String::new();

        // 挑第二个模型,进挑强度那一层,再 Esc 退回来。
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Down), press(Key::Enter)],
        );
        assert!(panel.efforts.is_some(), "第二个模型声明了档位");
        run(&view, &mut panel, &mut secret, &[press(Key::Esc)]);

        // 光标得在刚挑的那个模型上:再回车进的是它的档位,不是第一个模型。
        assert_eq!(
            view.listed(&panel).get(panel.cursor),
            Some(&Listed::Model(1))
        );
    }

    #[test]
    fn a_fixed_effort_does_not_open_the_level_picker() {
        // 面板和 `/model` 对「要不要问强度」必须一个判法:这里不问而命令问,
        // 面板就会先关、再被 `PickEffort` 弹回来。
        let view = ProvidersView::new(
            vec![account("deepseek", 1)],
            vec![ModelRow {
                effort: Some("high".into()),
                ..model("deepseek/reasoner", "deepseek")
            }],
            Vec::new(),
            Vec::new(),
        );
        let mut panel = Panel::models();
        let mut secret = String::new();

        assert_eq!(
            run(&view, &mut panel, &mut secret, &[press(Key::Enter)]),
            Step::Use {
                id: "deepseek/reasoner".into(),
                effort: None
            }
        );
        assert!(panel.efforts.is_none());
    }

    #[test]
    fn deleting_takes_two_presses_on_the_same_row() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        assert_eq!(
            run(&view, &mut panel, &mut secret, &[ctrl('d')]),
            Step::Stay
        );
        assert_eq!(panel.pending_delete.as_deref(), Some("deepseek"));
        // Moving off the row disarms it: a confirmation that outlived what it
        // was about would delete whatever the cursor landed on.
        run(&view, &mut panel, &mut secret, &[press(Key::Down)]);
        assert!(panel.pending_delete.is_none());
        assert_eq!(
            run(&view, &mut panel, &mut secret, &[ctrl('d')]),
            Step::Stay
        );
        assert_eq!(
            run(&view, &mut panel, &mut secret, &[ctrl('d')]),
            Step::DeleteAccount { id: "local".into() }
        );
    }

    #[test]
    fn a_managed_account_is_shown_and_never_written() {
        let mut managed = account("gateway", 3);
        managed.managed = true;
        let view = ProvidersView::new(vec![managed], Vec::new(), Vec::new(), Vec::new());
        let mut panel = Panel::new();
        let mut secret = String::new();
        assert_eq!(
            run(&view, &mut panel, &mut secret, &[ctrl('e')]),
            Step::Stay
        );
        assert!(panel.form.is_none(), "no edit form for a managed account");
        run(&view, &mut panel, &mut secret, &[ctrl('d')]);
        assert!(panel.pending_delete.is_none(), "not even armed");
        assert_eq!(
            run(&view, &mut panel, &mut secret, &[ctrl('d')]),
            Step::Stay
        );
    }

    #[test]
    fn an_offer_that_was_never_configured_has_nothing_to_delete() {
        let mut offer = account("openai", 0);
        offer.configured = false;
        let view = ProvidersView::new(vec![offer], Vec::new(), Vec::new(), Vec::new());
        let mut panel = Panel::new();
        let mut secret = String::new();
        run(&view, &mut panel, &mut secret, &[ctrl('d')]);
        assert!(panel.pending_delete.is_none());
    }

    #[test]
    fn saving_an_account_with_no_name_asks_for_one_where_the_eye_is() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::End), press(Key::Enter)],
        );
        let step = run(&view, &mut panel, &mut secret, &[press(Key::Enter)]);
        assert_eq!(step, Step::Stay, "nothing is written");
        let Some(Form::Account(form)) = panel.form.clone() else {
            panic!("the form stays open");
        };
        assert_eq!(form.focus, AccountField::Name);
    }

    #[test]
    fn an_empty_key_field_leaves_the_stored_credential_alone() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        run(&view, &mut panel, &mut secret, &[ctrl('e')]);
        let step = run(&view, &mut panel, &mut secret, &[press(Key::Enter)]);
        let Step::SaveAccount { id, draft, .. } = step else {
            panic!("it saves: {step:?}");
        };
        assert_eq!(id.as_deref(), Some("deepseek"));
        assert!(draft.key.is_none(), "nothing typed, nothing changed");
    }

    #[test]
    fn cycling_the_protocol_keeps_an_endpoint_the_person_typed() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::End), press(Key::Enter)],
        );
        // Name, then the endpoint field, typed by hand.
        run(
            &view,
            &mut panel,
            &mut secret,
            &[
                press(Key::Char('m')),
                press(Key::Tab),
                press(Key::Tab),
                press(Key::Char('h')),
                press(Key::Char('i')),
            ],
        );
        // Back to the protocol, and cycle it.
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::BackTab), press(Key::Right)],
        );
        let Some(Form::Account(form)) = panel.form.clone() else {
            panic!("the form is up");
        };
        assert_eq!(form.protocol_label(&view), "Ollama");
        assert!(
            form.endpoint.ends_with("hi"),
            "what was typed survives: {:?}",
            form.endpoint
        );
        assert!(
            !form.fields(&view).contains(&AccountField::Key),
            "a keyless protocol has no key field"
        );
    }

    #[test]
    fn every_level_on_is_saved_as_no_restriction_at_all() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        // Models tab, the add row.
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Enter), press(Key::End), press(Key::Enter)],
        );
        let Some(Form::Model(form)) = panel.form.clone() else {
            panic!("the model form is up: {:?}", panel.form);
        };
        assert!(form.levels.iter().all(|on| *on));
        assert_eq!(form.declared_levels(&view), None);
        let mut narrowed = form.clone();
        narrowed.levels = vec![true, false, true];
        assert_eq!(
            narrowed.declared_levels(&view),
            Some(vec!["low".to_string(), "high".to_string()])
        );
    }

    #[test]
    fn output_budget_presets_and_automatic_are_saved() {
        let view = view();
        let mut form = ModelForm::add(&view, &view.open_accounts()[0]).unwrap();
        assert!(
            form.fields(&view).contains(&ModelField::Output),
            "every field, always"
        );
        form.model = "vendor-x".into();
        form.focus = ModelField::Output;
        let mut panel = Panel::new();
        let mut secret = String::new();
        panel.form = Some(Form::Model(form));
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Right), press(Key::Right)],
        );
        let Step::SaveModel { draft, .. } =
            run(&view, &mut panel, &mut secret, &[press(Key::Enter)])
        else {
            panic!("saves a preset");
        };
        assert_eq!(draft.max_tokens, Some(8192));

        let mut row = model("vendor-x", "deepseek");
        row.max_tokens = Some(4096);
        let form = ModelForm::edit(&view, &row).unwrap();
        assert_eq!(form.max_tokens, row.max_tokens);
        panel.form = Some(Form::Model(form.clone()));
        let Step::SaveModel { draft, .. } =
            run(&view, &mut panel, &mut secret, &[press(Key::Enter)])
        else {
            panic!("saves the existing budget even with advanced settings hidden");
        };
        assert_eq!(draft.max_tokens, Some(4096));
        let mut form = form;
        form.focus = ModelField::Output;
        panel.form = Some(Form::Model(form));
        let Step::SaveModel { draft, .. } = run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Left), press(Key::Enter)],
        ) else {
            panic!("saves automatic");
        };
        assert_eq!(draft.max_tokens, None);
    }

    #[test]
    fn custom_output_budget_rejects_invalid_input_without_losing_it() {
        let view = view();
        for (input, expected) in [
            ("4096", Some(4096)),
            ("32k", Some(32000)),
            ("1M", Some(1_000_000)),
            ("4294967295", Some(u32::MAX as usize)),
            ("0", None),
            ("-4096", None),
            ("1.5", None),
            ("4294967296", None),
            ("4294968k", None),
            ("abc", None),
            ("", None),
        ] {
            let mut form = ModelForm::add(&view, &view.open_accounts()[0]).unwrap();
            form.model = "vendor-x".into();
            form.focus = ModelField::Output;
            form.max_tokens = Some(393216);
            let mut panel = Panel::new();
            let mut secret = String::new();
            panel.form = Some(Form::Model(form));
            if input.is_empty() {
                // Enter the custom stop after the saved non-preset value.
                run(&view, &mut panel, &mut secret, &[press(Key::Right)]);
            } else {
                assert!(paste(&mut panel, &mut secret, input));
            }
            let step = run(&view, &mut panel, &mut secret, &[press(Key::Enter)]);
            if let Some(expected) = expected {
                let Step::SaveModel { draft, .. } = step else {
                    panic!("{input}: {step:?}")
                };
                assert_eq!(draft.max_tokens, Some(expected), "{input}");
            } else {
                assert_eq!(step, Step::Stay, "{input}");
                let Some(Form::Model(form)) = &panel.form else {
                    panic!("keeps the form")
                };
                assert_eq!(form.focus, ModelField::Output);
                assert_eq!(form.max_tokens_typed.as_deref(), Some(input));
                // A deliberate choice of automatic recovers from invalid input.
                run(&view, &mut panel, &mut secret, &[press(Key::Right)]);
                let Step::SaveModel { draft, .. } =
                    run(&view, &mut panel, &mut secret, &[press(Key::Enter)])
                else {
                    panic!("automatic saves")
                };
                assert_eq!(draft.max_tokens, None);
            }
        }
    }

    #[test]
    fn a_model_saves_what_the_form_says() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Enter), press(Key::End), press(Key::Enter)],
        );
        // The account cycles, the model name is typed, and the window is picked.
        let Some(Form::Model(mut form)) = panel.form.clone() else {
            panic!("the model form is up");
        };
        form.model = "vendor-x".into();
        form.window = Some(64_000);
        form.vision = Some(true);
        panel.form = Some(Form::Model(form));
        let step = run(&view, &mut panel, &mut secret, &[press(Key::Enter)]);
        let Step::SaveModel { id, draft } = step else {
            panic!("it saves: {step:?}");
        };
        assert!(id.is_none(), "a new model has no id yet");
        assert_eq!(draft.model, "vendor-x");
        assert_eq!(draft.window, Some(64_000));
        assert_eq!(draft.vision, Some(true));
        assert_eq!(draft.account, "deepseek");
    }

    /// A window is picked, not typed: automatic first — where a new model
    /// starts, and what saves as "the protocol decides" — then the sizes models
    /// come in. The arrows and Space step through them.
    #[test]
    fn a_window_is_picked_and_a_new_model_starts_automatic() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Enter), press(Key::End), press(Key::Enter)],
        );
        let Some(Form::Model(mut form)) = panel.form.clone() else {
            panic!("the model form is up");
        };
        assert_eq!(form.window, None, "a new model starts automatic");
        assert_eq!(
            form.window_choices(),
            vec![
                None,
                Some(1_000_000),
                Some(512_000),
                Some(256_000),
                Some(128_000),
                Some(64_000),
                Some(32_000)
            ]
        );
        form.focus = ModelField::Window;
        panel.form = Some(Form::Model(form));
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Right), press(Key::Right)],
        );
        let Some(Form::Model(form)) = panel.form.clone() else {
            panic!("the form stays open");
        };
        assert_eq!(form.window, Some(512_000), "two steps");
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Left), press(Key::Left)],
        );
        let Some(Form::Model(mut form)) = panel.form.clone() else {
            panic!("the form stays open");
        };
        assert_eq!(form.window, None, "and back round to automatic");

        // Automatic saves as no window at all, for the protocol to fill in.
        form.model = "vendor-x".into();
        panel.form = Some(Form::Model(form));
        let Step::SaveModel { draft, .. } =
            run(&view, &mut panel, &mut secret, &[press(Key::Enter)])
        else {
            panic!("it saves");
        };
        assert_eq!(draft.window, None);

        assert_eq!(window_label(1_000_000), "1M");
        assert_eq!(window_label(512_000), "512K");
        assert_eq!(window_label(131_072), "131072");
    }

    /// The last stop is a custom window, typed: reached by the arrows after
    /// the smallest preset, or by typing a digit on any preset. `200k` and
    /// `1.5m` are read the way they are written; what will not parse sends the
    /// form back to the field instead of saving.
    #[test]
    fn a_custom_window_is_typed_after_the_presets() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Enter), press(Key::End), press(Key::Enter)],
        );
        let Some(Form::Model(mut form)) = panel.form.clone() else {
            panic!("the model form is up");
        };
        form.model = "vendor-x".into();
        form.focus = ModelField::Window;
        panel.form = Some(Form::Model(form));

        // Left from automatic wraps round to the custom stop.
        run(&view, &mut panel, &mut secret, &[press(Key::Left)]);
        let Some(Form::Model(form)) = panel.form.clone() else {
            panic!("the form stays open");
        };
        assert_eq!(form.window_typed.as_deref(), Some(""), "the custom stop");

        let keys: Vec<KeyPress> = "200k".chars().map(|c| press(Key::Char(c))).collect();
        run(&view, &mut panel, &mut secret, &keys);
        let Step::SaveModel { draft, .. } =
            run(&view, &mut panel, &mut secret, &[press(Key::Enter)])
        else {
            panic!("it saves");
        };
        assert_eq!(draft.window, Some(200_000));

        // A digit on a preset starts a custom window with that digit.
        let mut form = ModelForm::add(&view, &view.open_accounts()[0]).expect("an account");
        form.model = "vendor-x".into();
        form.focus = ModelField::Window;
        form.window = Some(128_000);
        panel.form = Some(Form::Model(form));
        let keys: Vec<KeyPress> = "8x000".chars().map(|c| press(Key::Char(c))).collect();
        run(&view, &mut panel, &mut secret, &keys);
        let Some(Form::Model(form)) = panel.form.clone() else {
            panic!("the form stays open");
        };
        assert_eq!(
            form.window_typed.as_deref(),
            Some("8000"),
            "what is not a number is not typed"
        );

        // And the arrows take it back to the presets, dropping what was typed.
        run(&view, &mut panel, &mut secret, &[press(Key::Right)]);
        let Some(Form::Model(form)) = panel.form.clone() else {
            panic!("the form stays open");
        };
        assert_eq!(form.window_typed, None);
        assert_eq!(form.window, None, "past the custom stop is automatic again");

        // What will not parse is sent back, not saved.
        let mut form = form;
        form.window_typed = Some("1.2.3k".into());
        panel.form = Some(Form::Model(form));
        assert_eq!(
            run(&view, &mut panel, &mut secret, &[press(Key::Enter)]),
            Step::Stay
        );
        let Some(Form::Model(form)) = panel.form.clone() else {
            panic!("the form stays open");
        };
        assert_eq!(form.focus, ModelField::Window);

        assert_eq!(parse_window("200000"), Some(200_000));
        assert_eq!(parse_window(" 1.5M "), Some(1_500_000));
        assert_eq!(parse_window("32K"), Some(32_000));
        assert_eq!(parse_window("0"), None);
        assert_eq!(parse_window("k"), None);
        assert_eq!(parse_window(""), None);
        // Outside the range a model can have: a typo, not a window.
        assert_eq!(parse_window("1.4"), None, "a thousand tokens at the least");
        assert_eq!(parse_window("99999999999999999999"), None);
        assert_eq!(parse_window("101m"), None);
        assert_eq!(parse_window("100m"), Some(100_000_000));
        // Nothing `f64` takes that nobody writes as a window.
        assert_eq!(parse_window("1e9"), None);
        assert_eq!(parse_window("-5k"), None);
        assert_eq!(parse_window("inf"), None);
    }

    #[test]
    fn selecting_ollama_probes_without_saving_and_address_edits_invalidate_the_probe() {
        let view = view();
        let mut form = AccountForm::add(&view);
        form.name = "my-local".into();
        form.focus = AccountField::Protocol;
        let mut panel = Panel {
            form: Some(Form::Account(form)),
            ..Panel::new()
        };
        let mut secret = String::new();
        assert_eq!(
            key(&view, &mut panel, &mut secret, press(Key::Right)),
            Step::DiscoverDraft {
                endpoint: "http://localhost:11434".into()
            }
        );
        let Some(Form::Account(form)) = panel.form.as_mut() else {
            panic!("account draft");
        };
        let first = form.discovery.as_ref().unwrap().id;
        form.discovery.as_mut().unwrap().result = Some(Err("Run ollama serve".into()));
        form.focus = AccountField::Endpoint;
        assert!(
            matches!(
                key(&view, &mut panel, &mut secret, press(Key::Tab)),
                Step::DiscoverDraft { .. }
            ),
            "leaving the address after a failure asks again"
        );
        let Some(Form::Account(form)) = panel.form.as_mut() else {
            panic!("account draft");
        };
        assert_ne!(first, form.discovery.as_ref().unwrap().id);
        form.focus = AccountField::Endpoint;
        form.caret = form.endpoint.len();
        key(&view, &mut panel, &mut secret, press(Key::Char('0')));
        let Some(Form::Account(form)) = panel.form.as_ref() else {
            panic!("account draft");
        };
        assert!(form.discovery.is_none(), "late answer must no longer match");
        assert!(form.endpoint.ends_with("114340"));
        assert!(matches!(
            key(&view, &mut panel, &mut secret, press(Key::Tab)),
            Step::DiscoverDraft { .. }
        ));
    }

    #[test]
    fn saving_an_ollama_draft_reuses_its_successful_inventory() {
        let view = view();
        let mut form = AccountForm::add(&view);
        form.name = "my-local".into();
        form.protocol = 1;
        form.endpoint = "http://localhost:11434".into();
        form.start_discovery(&view);
        let models = listed(&[("local-model", None)]);
        form.discovery.as_mut().unwrap().result = Some(Ok(models.clone()));
        let mut panel = Panel {
            form: Some(Form::Account(form)),
            ..Panel::new()
        };
        let Step::SaveAccount { listed, draft, .. } =
            key(&view, &mut panel, &mut String::new(), press(Key::Enter))
        else {
            panic!("explicit save");
        };
        assert_eq!(listed, Some(models));
        assert_eq!(draft.endpoint, "http://localhost:11434");
    }

    #[test]
    fn changing_a_target_never_reuses_the_previous_targets_typed_key() {
        let mut view = view();
        let mut form = AccountForm::add(&view);
        form.name = "local".into();
        form.focus = AccountField::Protocol;
        form.key_len = 10;
        let mut panel = Panel {
            form: Some(Form::Account(form)),
            ..Panel::new()
        };
        let mut secret = "sk-private".to_string();
        key(&view, &mut panel, &mut secret, press(Key::Right)); // switch to keyless Ollama
        assert!(secret.is_empty());
        let Step::SaveAccount { draft, .. } =
            key(&view, &mut panel, &mut secret, press(Key::Enter))
        else {
            panic!("save local");
        };
        assert_eq!(draft.protocol, "ollama");
        assert_eq!(draft.key, None);

        for account in Arc::make_mut(&mut view.accounts) {
            account.has_key = false;
        }
        // A model form's account is fixed by the page it opened on: there is no
        // other target to carry a typed key to.
        let form = ModelForm::add(&view, "local").unwrap();
        assert_eq!(form.account_id(), "local");
        assert_eq!(
            form.fields(&view).first(),
            Some(&ModelField::Model),
            "a key is the connection's: the model form never asks for one"
        );
    }

    #[test]
    fn connecting_a_preset_only_requires_the_key_and_preserves_the_vendor() {
        let view = view();
        let mut offer = account("deepseek", 0);
        offer.configured = false;
        offer.has_key = false;
        let form = AccountForm::connect(&view, &offer);
        assert_eq!(
            form.fields(&view),
            vec![AccountField::Key, AccountField::More]
        );
        assert_eq!(form.focus, AccountField::Key);
        assert_eq!(form.editing, None);
        let mut panel = Panel {
            form: Some(Form::Account(form)),
            ..Panel::new()
        };
        let mut secret = String::new();
        assert_eq!(
            key(&view, &mut panel, &mut secret, press(Key::Enter)),
            Step::Stay
        );
        assert!(paste(&mut panel, &mut secret, "sk-private"));
        assert!(!format!("{panel:?}").contains("sk-private"));
        let Step::SaveAccount { id, draft, .. } =
            key(&view, &mut panel, &mut secret, press(Key::Enter))
        else {
            panic!("connect");
        };
        assert_eq!(id, None);
        assert_eq!(draft.name, "deepseek");
        assert_eq!(draft.protocol, "deepseek");
        assert_eq!(draft.key.as_deref(), Some("sk-private"));
        assert!(secret.is_empty());
    }

    fn listed(ids: &[(&str, Option<usize>)]) -> Vec<Discovered> {
        ids.iter()
            .map(|(id, window)| Discovered {
                id: (*id).into(),
                window: *window,
                ..Default::default()
            })
            .collect()
    }

    /// A paste on the window row is typing: it starts a custom window from a
    /// preset, and only what a window can be written with gets in.
    #[test]
    fn a_paste_on_the_window_row_is_a_custom_window() {
        let view = view();
        let mut form = ModelForm::add(&view, &view.open_accounts()[0]).expect("an account");
        form.focus = ModelField::Window;
        form.window = Some(128_000);
        let mut panel = Panel {
            form: Some(Form::Model(form)),
            ..Panel::new()
        };
        let mut secret = String::new();
        assert!(paste(&mut panel, &mut secret, " 200k\n"));
        let Some(Form::Model(form)) = panel.form.clone() else {
            panic!("the form stays open");
        };
        assert_eq!(form.window_typed.as_deref(), Some("200k"));

        let mut form = form;
        form.window_typed = Some(String::new());
        panel.form = Some(Form::Model(form));
        assert!(paste(&mut panel, &mut secret, "1e9"));
        let Some(Form::Model(form)) = panel.form.clone() else {
            panic!("the form stays open");
        };
        assert_eq!(
            form.window_typed.as_deref(),
            Some("19"),
            "the `e` is not a window"
        );
    }

    /// An edited model keeps a window none of the presets name, in its place
    /// among them, until a person picks another — and one that had no window
    /// stays automatic rather than being given one on an unrelated save.
    #[test]
    fn an_edited_window_is_kept_as_it_was() {
        let view = view();
        let mut row = model("vendor-x", "deepseek");
        row.window = 200_000;
        let mut form = ModelForm::edit(&view, &row).expect("the account is there");
        assert_eq!(form.window, Some(200_000), "what it had is what it shows");
        // No automatic stop: saved from an edit it would keep the window it
        // has, which is not what "the protocol's default" says.
        assert_eq!(
            form.window_choices(),
            vec![
                Some(1_000_000),
                Some(512_000),
                Some(256_000),
                Some(200_000),
                Some(128_000),
                Some(64_000),
                Some(32_000)
            ]
        );
        form.cycle_window(true);
        assert_eq!(form.window, Some(128_000));
        row.window = 0;
        let form = ModelForm::edit(&view, &row).expect("the account is there");
        assert_eq!(form.window, None, "no window configured stays automatic");
    }

    /// A pasted line goes into the field with the keyboard, and only its first
    /// line: a newline written into an id is a newline written into the file.
    #[test]
    fn a_paste_lands_in_the_field_and_never_carries_a_newline() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::End), press(Key::Enter)],
        );
        assert!(paste(&mut panel, &mut secret, "two\nlines\n"));
        let Some(Form::Account(form)) = panel.form.clone() else {
            panic!("the form is up");
        };
        assert_eq!(form.name, "two");
        assert_eq!(form.caret, 3, "the caret follows what was pasted");

        // And into the key, where it is counted rather than kept.
        let mut panel2 = Panel::new();
        let mut secret2 = String::new();
        run(
            &view,
            &mut panel2,
            &mut secret2,
            &[press(Key::End), press(Key::Enter)],
        );
        run(
            &view,
            &mut panel2,
            &mut secret2,
            &[press(Key::Tab), press(Key::Tab), press(Key::Tab)],
        );
        assert!(paste(&mut panel2, &mut secret2, "sk-pasted\n"));
        assert_eq!(secret2, "sk-pasted");
        assert!(!format!("{panel2:?}").contains("sk-pasted"));

        // With no form up it is a search, because the box under the cursor is
        // the filter.
        let mut list = Panel::new();
        assert!(paste(&mut list, &mut String::new(), "loc"));
        assert_eq!(list.query, "loc");
    }

    /// 模型按账号分组,而不是平铺一张表:多账号时「这个模型是谁家的」要能一眼
    /// 看出来,而不是在十几行里扫同一个账号名。
    #[test]
    fn models_are_grouped_under_the_account_they_belong_to() {
        let view = view();
        let mut panel = Panel::new();
        panel.screen = Screen::Models;
        let listed = view.listed(&panel);
        let groups = listed
            .iter()
            .filter(|row| matches!(row, Listed::Group(_)))
            .count();
        assert!(groups >= 2, "一个账号一条小标题:{listed:?}");
        // 每条小标题底下紧跟的都是它自己账号的模型。
        let mut under: Option<String> = None;
        for row in &listed {
            match row {
                Listed::Group(first) => {
                    under = Some(view.models()[*first].account.clone());
                }
                Listed::Model(i) => {
                    if let Some(account) = under.as_deref() {
                        assert_eq!(
                            view.models()[*i].account,
                            account,
                            "模型跟在自己账号的小标题下:{listed:?}"
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// 正在用的模型那一组排最前,组里它自己排第一:`/model` 一打开,光标落下的
    /// 第一个能停的行就是它。按配置文件的顺序排时,用着排在后面账号的模型,
    /// 每次打开都要先翻过别人家的整组才找得到自己在哪(2026-10-09 反馈)。
    #[test]
    fn the_model_in_use_and_its_account_come_first() {
        let on_b = view().with_current(Some("local/b"));
        let mut panel = Panel::new();
        panel.screen = Screen::Models;
        assert_eq!(
            on_b.listed(&panel),
            vec![
                Listed::Group(2),
                Listed::Model(2),
                Listed::Model(1),
                Listed::Group(0),
                Listed::Model(0),
                Listed::AddModel,
            ],
            "当前账号整组提前,组内当前模型打头,其余保持文件里的顺序"
        );
        on_b.settle_cursor(&mut panel);
        assert_eq!(
            on_b.listed(&panel)[panel.cursor],
            Listed::Model(2),
            "打开就停在正在用的那一个上"
        );

        // 没标当前的列表照文件顺序,不凭空挑一个提前。
        assert_eq!(
            view().listed(&panel),
            vec![
                Listed::Group(0),
                Listed::Model(0),
                Listed::Group(1),
                Listed::Model(1),
                Listed::Model(2),
                Listed::AddModel,
            ]
        );
    }

    /// 面板开着时列表换了(agent 迟报了自己在用哪个、网页上切了模型),正在用的
    /// 那组跳到最前——光标得跟着它指着的那个模型走,不能留在原来的行号上。
    /// 留在行号上,回车换过去的就是一个人从没指过的模型。
    #[test]
    fn a_reordered_list_keeps_the_cursor_on_the_same_model() {
        let before = view();
        let mut panel = Panel::new();
        panel.screen = Screen::Models;
        // 文件顺序:[G0, M0, G1, M1, M2, AddModel],指着 local/a(第 3 行)。
        panel.cursor = 3;
        assert_eq!(before.listed(&panel)[panel.cursor], Listed::Model(1));

        // 报上来:在用的是 local/b,local 那组提到最前。
        let after = before.with_current(Some("local/b"));
        follow_cursor(&before, &after, &mut panel);
        assert_eq!(
            after.listed(&panel)[panel.cursor],
            Listed::Model(1),
            "还指着 local/a:{:?}",
            after.listed(&panel)
        );

        // 指着的那一个没了:落到能停的行上,不停在小标题上。
        let gone = ProvidersView::new(
            vec![account("deepseek", 1)],
            vec![model("deepseek/chat", "deepseek")],
            Vec::new(),
            Vec::new(),
        );
        follow_cursor(&after, &gone, &mut panel);
        assert!(gone.listed(&panel)[panel.cursor].selectable());
    }

    /// 小标题停不住:上下键从它上面走过去,回车不会落在一条分界线上。
    #[test]
    fn the_cursor_walks_past_a_heading() {
        let view = view();
        let mut panel = Panel::models();
        let mut secret = String::new();
        view.settle_cursor(&mut panel);
        let listed = view.listed(&panel);
        // 走一遍整张列表,光标一次都不该停在小标题上。
        for _ in 0..listed.len() {
            assert!(
                listed[panel.cursor].selectable(),
                "停在了小标题上:{:?} 第 {} 行",
                listed,
                panel.cursor
            );
            run(&view, &mut panel, &mut secret, &[press(Key::Down)]);
        }
        for _ in 0..listed.len() {
            assert!(listed[panel.cursor].selectable());
            run(&view, &mut panel, &mut secret, &[press(Key::Up)]);
        }
    }

    #[test]
    fn the_filter_matches_what_a_person_can_see() {
        let view = view();
        let mut panel = Panel::new();
        panel.query = "loc".into();
        assert_eq!(
            view.listed(&panel),
            vec![Listed::Account(1), Listed::AddAccount]
        );
        panel.screen = Screen::Models;
        panel.query = "local/b".into();
        assert_eq!(
            view.listed(&panel),
            vec![Listed::Group(2), Listed::Model(2), Listed::AddModel],
            "筛剩一个模型时,它那一组的小标题还在;筛空了的组连标题一起不画"
        );
    }

    // ── the list → detail panel (docs/plans/2026-11-20-provider-management-redesign-design.md) ──

    /// The view with a listing ready for `deepseek`, the panel on its page.
    fn on_page(items: &[&str]) -> (ProvidersView, Panel) {
        let view = view();
        let mut panel = Panel::new();
        let step = run(&view, &mut panel, &mut String::new(), &[press(Key::Enter)]);
        assert_eq!(
            step,
            Step::Discover {
                account: "deepseek".into()
            },
            "walking into an account asks what its endpoint lists"
        );
        assert_eq!(panel.screen, Screen::Detail("deepseek".into()));
        let request = panel.discovery_request("deepseek").unwrap();
        let listed = items
            .iter()
            .map(|id| Discovered {
                id: (*id).into(),
                window: Some(128_000),
                ..Default::default()
            })
            .collect();
        assert!(panel.discovered(&view, "deepseek", request, Ok(listed)));
        (view, panel)
    }

    fn at(view: &ProvidersView, panel: &Panel) -> Listed {
        view.listed(panel)[panel.cursor]
    }

    #[test]
    fn a_page_lists_the_connection_what_is_added_and_what_the_server_has() {
        let (view, panel) = on_page(&["deepseek/chat", "flash", "pro"]);
        assert_eq!(
            view.listed(&panel),
            vec![
                Listed::Connection,
                Listed::Section(Section::Added),
                Listed::Configured(0),
                Listed::Section(Section::Server),
                Listed::Available(1),
                Listed::Available(2),
                Listed::Manual,
            ],
            "the configured model is not offered again; the manual row is last"
        );
    }

    #[test]
    fn enter_switches_an_added_model_and_adds_a_server_one() {
        let (view, mut panel) = on_page(&["flash", "pro"]);
        let mut secret = String::new();
        // Down from the connection row: the added model.
        let step = run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Down), press(Key::Enter)],
        );
        assert_eq!(
            step,
            Step::Use {
                id: "deepseek/chat".into(),
                effort: None
            }
        );
        let (view, mut panel) = on_page(&["flash", "pro"]);
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Down), press(Key::Down)],
        );
        assert_eq!(at(&view, &panel), Listed::Available(0));
        let Step::SaveModel { id: None, draft } =
            run(&view, &mut panel, &mut secret, &[press(Key::Enter)])
        else {
            panic!("an add");
        };
        assert_eq!(
            (draft.account.as_str(), draft.model.as_str()),
            ("deepseek", "flash")
        );
        assert_eq!(draft.window, Some(128_000));
        assert!(!draft.use_now && !draft.default, "added, never switched to");
        assert!(panel.adding.contains(&("deepseek".into(), "flash".into())));
        assert_eq!(
            at(&view, &panel),
            Listed::Available(1),
            "on to the next one, so a run of Enters adds a run"
        );
    }

    #[test]
    fn a_row_being_added_is_not_sent_twice_and_a_failure_stays_on_it() {
        let (view, mut panel) = on_page(&["flash"]);
        let mut secret = String::new();
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Down), press(Key::Down)],
        );
        assert!(matches!(
            run(&view, &mut panel, &mut secret, &[press(Key::Enter)]),
            Step::SaveModel { .. }
        ));
        assert_eq!(
            at(&view, &panel),
            Listed::Manual,
            "past the last candidate: the manual row, never an added model"
        );
        run(&view, &mut panel, &mut secret, &[press(Key::Up)]);
        assert_eq!(at(&view, &panel), Listed::Available(0));
        assert_eq!(
            run(&view, &mut panel, &mut secret, &[press(Key::Enter)]),
            Step::Stay,
            "still on its way"
        );
        assert!(panel.add_settled("deepseek", "flash", Err("HTTP 500".into())));
        assert!(panel.adding.is_empty());
        assert_eq!(
            panel
                .add_errors
                .get(&("deepseek".into(), "flash".into()))
                .map(String::as_str),
            Some("HTTP 500")
        );
        assert!(
            matches!(
                run(&view, &mut panel, &mut secret, &[press(Key::Enter)]),
                Step::SaveModel { .. }
            ),
            "and it can be tried again"
        );
    }

    #[test]
    fn ctrl_e_edits_what_the_cursor_is_on() {
        let (view, mut panel) = on_page(&["flash"]);
        let mut secret = String::new();
        // The connection row: the account's form.
        run(&view, &mut panel, &mut secret, &[ctrl('e')]);
        assert!(
            matches!(&panel.form, Some(Form::Account(f)) if f.editing.as_deref() == Some("deepseek"))
        );
        run(&view, &mut panel, &mut secret, &[press(Key::Esc)]);
        // An added model: its form.
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Down), ctrl('e')],
        );
        assert!(
            matches!(&panel.form, Some(Form::Model(f)) if f.editing.as_deref() == Some("deepseek/chat"))
        );
        run(&view, &mut panel, &mut secret, &[press(Key::Esc)]);
        // A server model: an add form filled from what the listing declared.
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Down), ctrl('e')],
        );
        let Some(Form::Model(form)) = &panel.form else {
            panic!("an add form");
        };
        assert_eq!(
            (form.editing.as_deref(), form.model.as_str()),
            (None, "flash")
        );
        assert_eq!(form.window, Some(128_000));
        let Step::SaveModel { id: None, draft } =
            run(&view, &mut panel, &mut secret, &[press(Key::Enter)])
        else {
            panic!("saving it adds it");
        };
        assert_eq!(draft.model, "flash");
    }

    #[test]
    fn the_manual_row_takes_what_the_filter_holds() {
        let (view, mut panel) = on_page(&["flash"]);
        let mut secret = String::new();
        for c in "qwen3-x".chars() {
            run(&view, &mut panel, &mut secret, &[press(Key::Char(c))]);
        }
        assert_eq!(
            view.listed(&panel),
            vec![Listed::Connection, Listed::Manual],
            "nothing listed matches; the manual row is still there"
        );
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::End), press(Key::Enter)],
        );
        let Some(Form::Model(form)) = &panel.form else {
            panic!("the manual form");
        };
        assert_eq!(form.model, "qwen3-x");
        assert_eq!(form.account_id(), "deepseek");
    }

    #[test]
    fn end_reaches_the_pinned_row_and_esc_steps_back_one_screen() {
        let (view, mut panel) = on_page(&["a", "b", "c", "d"]);
        let mut secret = String::new();
        run(&view, &mut panel, &mut secret, &[press(Key::End)]);
        assert_eq!(at(&view, &panel), Listed::Manual);
        assert_eq!(
            run(&view, &mut panel, &mut secret, &[press(Key::Esc)]),
            Step::Stay
        );
        assert_eq!(panel.screen, Screen::Providers);
        assert_eq!(
            at(&view, &panel),
            Listed::Account(0),
            "back on the account it was"
        );
        assert_eq!(
            run(&view, &mut panel, &mut secret, &[press(Key::Esc)]),
            Step::Close
        );
    }

    #[test]
    fn model_add_row_opens_the_list_on_the_account_in_use_and_esc_returns() {
        let view = view().with_current(Some("local/a"));
        let mut panel = Panel::models();
        let mut secret = String::new();
        // A single account would not change it: the list, not its page.
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::End), press(Key::Enter)],
        );
        assert_eq!(panel.screen, Screen::Providers);
        assert!(panel.from_models);
        assert_eq!(at(&view, &panel), Listed::Account(1), "the account in use");
        assert!(view.listed(&panel).contains(&Listed::AddAccount));
        assert_eq!(
            run(&view, &mut panel, &mut secret, &[press(Key::Esc)]),
            Step::Stay
        );
        assert_eq!(panel.screen, Screen::Models);
        assert_eq!(
            at(&view, &panel),
            Listed::Model(1),
            "back on the model in use"
        );
    }

    #[test]
    fn a_managed_account_is_shown_read_only_on_its_page() {
        let mut view = view();
        Arc::make_mut(&mut view.accounts)[0].managed = true;
        for m in Arc::make_mut(&mut view.models) {
            if m.account == "deepseek" {
                m.managed = true;
            }
        }
        let mut panel = Panel::new();
        let mut secret = String::new();
        assert_eq!(
            run(&view, &mut panel, &mut secret, &[press(Key::Enter)]),
            Step::Stay,
            "nothing to ask a gateway that owns the account"
        );
        assert_eq!(
            view.listed(&panel),
            vec![
                Listed::Connection,
                Listed::Section(Section::Added),
                Listed::Configured(0)
            ]
        );
        for press_ in [ctrl('e'), ctrl('d'), ctrl('d')] {
            run(&view, &mut panel, &mut secret, &[press_]);
            assert!(panel.form.is_none() && panel.pending_delete.is_none());
        }
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Down), ctrl('d'), ctrl('d')],
        );
        assert!(
            panel.pending_delete.is_none(),
            "a managed model is not deleted"
        );
    }

    #[test]
    fn a_failed_listing_is_asked_again_with_enter() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        run(&view, &mut panel, &mut secret, &[press(Key::Enter)]);
        let request = panel.discovery_request("deepseek").unwrap();
        assert!(panel.discovered(&view, "deepseek", request, Err("HTTP 404".into())));
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::End), press(Key::Up)],
        );
        assert_eq!(at(&view, &panel), Listed::Failed);
        assert_eq!(
            run(&view, &mut panel, &mut secret, &[press(Key::Enter)]),
            Step::Discover {
                account: "deepseek".into()
            }
        );
        assert_ne!(panel.discovery_request("deepseek"), Some(request));
    }

    #[test]
    fn deleting_on_a_page_takes_two_presses_on_the_model() {
        let (view, mut panel) = on_page(&[]);
        let mut secret = String::new();
        run(&view, &mut panel, &mut secret, &[press(Key::Down)]);
        assert_eq!(
            run(&view, &mut panel, &mut secret, &[ctrl('d')]),
            Step::Stay
        );
        assert_eq!(panel.pending_delete.as_deref(), Some("deepseek/chat"));
        assert_eq!(
            run(&view, &mut panel, &mut secret, &[ctrl('d')]),
            Step::DeleteModel {
                id: "deepseek/chat".into()
            }
        );
    }

    #[test]
    fn with_no_model_the_first_enter_adds_and_uses_and_the_next_only_adds() {
        let (view, mut panel) = on_page(&["flash", "pro"]);
        panel.auto_use = true;
        let mut secret = String::new();
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Down), press(Key::Down)],
        );
        let Step::SaveModel { draft, .. } =
            run(&view, &mut panel, &mut secret, &[press(Key::Enter)])
        else {
            panic!("an add");
        };
        assert!(draft.use_now, "the add-and-use");
        assert!(
            !draft.default,
            "the default follows the switch, not the draft"
        );
        assert!(!panel.auto_use, "taken");
        let Step::SaveModel { draft, .. } =
            run(&view, &mut panel, &mut secret, &[press(Key::Enter)])
        else {
            panic!("an add");
        };
        assert!(!draft.use_now, "the next one only adds");
    }

    #[test]
    fn a_form_key_means_one_thing_on_every_row() {
        let view = view();
        let mut secret = String::new();
        // A short connection form ends on 「更多设置 ▸」: Enter there opens the
        // rest, landing on the first field that was not shown; elsewhere it saves.
        let mut offer = account("deepseek", 0);
        offer.configured = false;
        let mut panel = Panel {
            form: Some(Form::Account(AccountForm::connect(&view, &offer))),
            ..Panel::new()
        };
        assert_eq!(
            run(
                &view,
                &mut panel,
                &mut secret,
                &[press(Key::Down), press(Key::Enter)]
            ),
            Step::Stay
        );
        let Some(Form::Account(form)) = &panel.form else {
            panic!("still the form");
        };
        assert!(form.advanced);
        assert!(!form.fields(&view).contains(&AccountField::More));
        assert_eq!(
            form.focus,
            AccountField::Name,
            "on the first field it had not shown"
        );
        // Ctrl+O and Ctrl+F do nothing any more.
        let before = panel.clone();
        run(&view, &mut panel, &mut secret, &[ctrl('o'), ctrl('f')]);
        assert_eq!(panel, before);

        // On a model form Space changes nothing but a reasoning level.
        let mut form = ModelForm::add(&view, "deepseek").unwrap();
        form.focus = ModelField::Vision;
        let mut panel = Panel {
            form: Some(Form::Model(form)),
            ..Panel::new()
        };
        run(&view, &mut panel, &mut secret, &[press(Key::Char(' '))]);
        let Some(Form::Model(form)) = &panel.form else {
            panic!("still the form");
        };
        assert_eq!(
            form.vision, None,
            "Space is not a second way to change a value"
        );
        run(&view, &mut panel, &mut secret, &[press(Key::Right)]);
        let Some(Form::Model(form)) = &panel.form else {
            panic!("still the form");
        };
        assert_eq!(form.vision, Some(true), "←→ is");
    }
}
