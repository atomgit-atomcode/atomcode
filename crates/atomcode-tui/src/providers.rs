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
        }
    }

    pub fn accounts(&self) -> &[AccountRow] {
        &self.accounts
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
    pub fn with_current(&self, id: Option<&str>) -> Self {
        let models: Vec<ModelRow> = self
            .models
            .iter()
            .map(|row| ModelRow {
                current: id == Some(row.id.as_str()),
                ..row.clone()
            })
            .collect();
        Self {
            accounts: self.accounts.clone(),
            models: Arc::new(models),
            protocols: self.protocols.clone(),
            efforts: self.efforts.clone(),
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

    /// The rows of one tab, filtered and drilled as the panel asks.
    ///
    /// One function, walked by the keys, the drawing and the hit test, so a
    /// click that landed on one row and a highlight drawn on another is ruled
    /// out by construction rather than by keeping three formulas in step — the
    /// shape `crate::modules::settings` uses for the same reason.
    pub fn listed(&self, panel: &Panel) -> Vec<Listed> {
        let q = panel.query.trim().to_lowercase();
        let hit = |haystack: &[&str]| -> bool {
            q.is_empty()
                || haystack
                    .iter()
                    .any(|text| text.to_lowercase().contains(q.as_str()))
        };
        let mut out: Vec<Listed> = Vec::new();
        match panel.tab {
            Tab::Accounts => {
                for (i, a) in self.accounts.iter().enumerate() {
                    if hit(&[&a.id, &a.label, &a.protocol]) {
                        out.push(Listed::Account(i));
                    }
                }
                out.push(Listed::Add);
            }
            Tab::Models => {
                // 按账号分组:一个账号一条不可选的小标题,它的模型跟在下面。
                // 平铺过的那版在多账号下读不出「这个模型是谁家的」——行里写着
                // 账号,但十几行里找同一个账号要靠眼睛扫。下钻到某个账号时不画
                // 标题:那时整张列表都是它的,再写一遍是废话。
                let mut groups: Vec<&str> = Vec::new();
                for m in self.models.iter() {
                    if !groups.contains(&m.account.as_str()) {
                        groups.push(&m.account);
                    }
                }
                for account in groups {
                    let mut under: Vec<usize> = Vec::new();
                    for (i, m) in self.models.iter().enumerate() {
                        if m.account != account {
                            continue;
                        }
                        if panel.drill.as_deref().is_some_and(|only| only != m.account) {
                            continue;
                        }
                        if hit(&[&m.id, &m.model, &m.account]) {
                            under.push(i);
                        }
                    }
                    // 筛空了的组不留标题:一个空标题说的是「这里什么都没有」。
                    if under.is_empty() {
                        continue;
                    }
                    if panel.drill.is_none() {
                        out.push(Listed::Group(under[0]));
                    }
                    out.extend(under.into_iter().map(Listed::Model));
                }
                // Nowhere to put a new model is a row that would open a form
                // with no account to hang it on.
                if !self.open_accounts().is_empty() {
                    out.push(Listed::Add);
                }
            }
        }
        out
    }
}

impl ProvidersView {
    /// 把光标落到一行能停的行上。
    ///
    /// 换页、下钻、改筛选词之后都要走一趟:模型页的第一行是账号的小标题,
    /// 而亮着的那一行必须是人真能对它做点什么的行——否则屏幕上亮着一条分界线,
    /// 回车没反应。
    pub fn settle_cursor(&self, panel: &mut Panel) {
        let listed = self.listed(panel);
        panel.cursor = settle(&listed, panel.cursor);
    }
}

/// One row of the list, as everything that walks the list sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listed {
    Account(usize),
    Model(usize),
    /// An account's name, over the models that belong to it — carried as the
    /// index of the first of them, so this stays `Copy` and the name is read
    /// where it is drawn. Not selectable: the cursor walks past it, because
    /// there is nothing to do to a heading.
    Group(usize),
    /// The virtual last row that opens the add form.
    Add,
}

impl Listed {
    /// Whether the cursor may rest here.
    pub fn selectable(&self) -> bool {
        !matches!(self, Listed::Group(_))
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

/// Which list is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Accounts,
    Models,
}

impl Tab {
    pub const ALL: [Tab; 2] = [Tab::Accounts, Tab::Models];

    /// The word on the tab. The other front end has a providers panel too, and
    /// it is the same two words — so this reaches for its entry rather than
    /// opening a second one that could drift.
    pub fn label(self) -> String {
        use crate::i18n::product::{t, Msg};
        match self {
            Tab::Accounts => t(Msg::ProviderPanelTabAccounts).into_owned(),
            Tab::Models => t(Msg::ProviderPanelTabModels).into_owned(),
        }
    }

    fn other(self) -> Tab {
        match self {
            Tab::Accounts => Tab::Models,
            Tab::Models => Tab::Accounts,
        }
    }
}

/// Which field of the account form has the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountField {
    Name,
    Protocol,
    Endpoint,
    Key,
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
        }
    }

    /// The fields this form actually has, in the order Tab walks them.
    ///
    /// Asked rather than matched on at each of the places that care, because
    /// "which fields does this form have" is one fact and three copies of it
    /// would disagree the first time a field grew a condition.
    pub fn fields(&self, view: &ProvidersView) -> Vec<AccountField> {
        let mut out = Vec::new();
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
            AccountField::Protocol | AccountField::Key => 0,
        }
    }

    /// Move to the next protocol, carrying its endpoint with it.
    ///
    /// The endpoint follows the protocol only while it is still the one the
    /// protocol suggested: a person who typed their own endpoint and then
    /// cycled the protocol to see what else is on offer must not find it gone.
    fn cycle_protocol(&mut self, view: &ProvidersView, forward: bool) {
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
            AccountField::Protocol | AccountField::Key => None,
        }
    }
}

/// Which field of the model form has the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelField {
    Account,
    Key,
    Model,
    Vision,
    Effort,
    Levels,
    Window,
    Default,
}

/// Adding a model to an account, or changing one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelForm {
    /// `Some` when an existing model is being changed; its account is then
    /// fixed, because moving a model between accounts is a delete and an add.
    pub editing: Option<String>,
    /// The accounts it may hang off, by id.
    pub accounts: Vec<String>,
    pub account: usize,
    pub model: String,
    /// Typed, so an empty field can mean "whatever this protocol usually holds".
    pub window: String,
    pub vision: Option<bool>,
    /// `None` is a model that takes no effort setting at all.
    pub effort: Option<String>,
    /// Parallel to [`ProvidersView::efforts`]: which levels this model offers.
    pub levels: Vec<bool>,
    /// Which level Space toggles.
    pub level: usize,
    /// Whether saving also makes it the one the session talks to.
    pub default: bool,
    pub key_len: usize,
    pub focus: ModelField,
    /// Where the next character goes in the focused text field.
    pub caret: usize,
}

impl ModelForm {
    /// A fresh add form. `None` when there is no account to hang a model off.
    pub fn add(view: &ProvidersView, preferred: Option<&str>) -> Option<Self> {
        let accounts = view.open_accounts();
        if accounts.is_empty() {
            return None;
        }
        let account = preferred
            .and_then(|want| accounts.iter().position(|id| id == want))
            .unwrap_or(0);
        Some(Self {
            editing: None,
            accounts,
            account,
            model: String::new(),
            window: String::new(),
            vision: None,
            effort: None,
            levels: vec![true; view.efforts().len()],
            level: 0,
            default: false,
            key_len: 0,
            focus: ModelField::Account,
            caret: 0,
        })
    }

    /// An edit form filled from a row. `None` for a row whose account is gone.
    pub fn edit(view: &ProvidersView, row: &ModelRow) -> Option<Self> {
        let accounts = vec![row.account.clone()];
        let levels = view
            .efforts()
            .iter()
            .map(|level| row.levels.is_empty() || row.levels.contains(level))
            .collect();
        Some(Self {
            editing: Some(row.id.clone()),
            accounts,
            account: 0,
            model: row.model.clone(),
            window: row.window.to_string(),
            vision: row.vision,
            effort: row.effort.clone(),
            levels,
            level: 0,
            default: row.current,
            key_len: 0,
            caret: row.model.len(),
            focus: ModelField::Model,
        })
    }

    pub fn account_id(&self) -> &str {
        self.accounts
            .get(self.account)
            .map(String::as_str)
            .unwrap_or_default()
    }

    /// Whether this form also has to ask for a credential: a new model on an
    /// account that has none stored yet.
    pub fn needs_key(&self, view: &ProvidersView) -> bool {
        self.editing.is_none()
            && view
                .account(self.account_id())
                .is_some_and(|a| !a.has_key && !a.managed)
    }

    pub fn fields(&self, view: &ProvidersView) -> Vec<ModelField> {
        let mut out = Vec::new();
        if self.editing.is_none() {
            out.push(ModelField::Account);
            if self.needs_key(view) {
                out.push(ModelField::Key);
            }
        }
        out.push(ModelField::Model);
        out.push(ModelField::Vision);
        out.push(ModelField::Effort);
        if !view.efforts().is_empty() {
            out.push(ModelField::Levels);
        }
        out.push(ModelField::Window);
        out.push(ModelField::Default);
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
            ModelField::Window => self.window.len(),
            _ => 0,
        }
    }

    fn cycle_account(&mut self, view: &ProvidersView, forward: bool) {
        if self.accounts.is_empty() {
            return;
        }
        self.account = if forward {
            (self.account + 1) % self.accounts.len()
        } else {
            (self.account + self.accounts.len() - 1) % self.accounts.len()
        };
        let fields = self.fields(view);
        if !fields.contains(&self.focus) {
            self.focus = ModelField::Model;
        }
        self.caret = self.caret.min(self.focused_len());
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

    /// Off, then each level this build offers, then round.
    fn cycle_effort(&mut self, view: &ProvidersView, forward: bool) {
        let mut ring: Vec<Option<String>> = vec![None];
        ring.extend(view.efforts().iter().cloned().map(Some));
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
            ModelField::Window => Some((&mut self.window, caret)),
            _ => None,
        }
    }

    /// The levels to persist: `None` when every level is on, which is what
    /// "unrestricted" means and what a list of all of them would only pin.
    fn declared_levels(&self, view: &ProvidersView) -> Option<Vec<String>> {
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
    pub tab: Tab,
    /// The row the arrows are on, as an index into the *listed* rows.
    pub cursor: usize,
    /// What is typed in the search box.
    pub query: String,
    /// Only this account's models are listed, when a person walked in through
    /// one. Cleared by switching tabs, the way tuix's drill-in is.
    pub drill: Option<String>,
    /// The row one more ctrl-d would delete.
    ///
    /// Two presses, not one: this is the gesture that throws something away, and
    /// a single key that does it is a key somebody hits on the way to something
    /// else. Cleared by everything else, including moving off the row.
    pub pending_delete: Option<String>,
    pub form: Option<Form>,
}

impl Panel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Show a tab, with its own list from the top and no filter.
    ///
    /// The query is dropped rather than kept — unlike the settings panel, whose
    /// two pages list the same kind of thing. Here they do not: a filter typed
    /// against account names hides every model, and a list that came up empty
    /// because of something typed on another tab is a list that looks broken.
    pub fn show(&mut self, tab: Tab) -> bool {
        if self.tab == tab && self.drill.is_none() && self.query.is_empty() {
            return false;
        }
        self.tab = tab;
        // 0 行未必停得住(模型页第一行是账号的小标题),真正的落点由
        // `point_at`/`walk` 夹住;这里先归零,列表一画就会被带到第一个能停的行。
        self.cursor = 0;
        self.query.clear();
        self.drill = None;
        self.pending_delete = None;
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
#[derive(Clone, PartialEq, Eq)]
pub struct ModelDraft {
    pub account: String,
    pub model: String,
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
    },
    /// Write an account. `id` is `None` for one that does not exist yet.
    SaveAccount {
        id: Option<String>,
        draft: AccountDraft,
    },
    SaveModel {
        id: Option<String>,
        draft: ModelDraft,
    },
    DeleteAccount {
        id: String,
    },
    DeleteModel {
        id: String,
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
    // 光标可能正停在一条不可选的小标题上:换页、改筛选词都会把它留在那儿。
    // 先挪到能停的行再看这一键——否则在模型页上一按回车什么也不发生,而屏幕上
    // 明明有一行亮着。
    if listed
        .get(panel.cursor)
        .is_some_and(|row| !row.selectable())
    {
        panel.cursor = settle(&listed, panel.cursor);
    }
    let at = listed.get(panel.cursor).copied();
    let armed = panel.pending_delete.take();
    match (press.key, press.mods) {
        (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => Step::Close,
        (Key::Tab, _) | (Key::BackTab, _) => {
            panel.show(panel.tab.other());
            view.settle_cursor(panel);
            Step::Stay
        }
        (Key::Left, _) => {
            panel.show(Tab::Accounts);
            view.settle_cursor(panel);
            Step::Stay
        }
        (Key::Right, _) => {
            panel.show(Tab::Models);
            view.settle_cursor(panel);
            Step::Stay
        }
        (Key::Up, _) => {
            panel.cursor = walk(&listed, panel.cursor, false);
            Step::Stay
        }
        (Key::Down, _) => {
            panel.cursor = walk(&listed, panel.cursor, true);
            Step::Stay
        }
        // Add, on the tab that is showing. ctrl-a rather than a letter, because
        // the letters are the filter.
        (Key::Char('a'), Mods::CTRL) => {
            begin_add(view, panel, secret);
            Step::Stay
        }
        (Key::Char('e'), Mods::CTRL) => {
            begin_edit(view, panel, secret, at);
            Step::Stay
        }
        (Key::Char('d'), Mods::CTRL) => delete_key(view, panel, at, armed),
        (Key::Enter, _) => match at {
            // 光标停不到小标题上,所以这一支只为穷尽而写。
            Some(Listed::Group(_)) => Step::Stay,
            Some(Listed::Add) => {
                begin_add(view, panel, secret);
                Step::Stay
            }
            // Walking into an account shows what is under it. The models tab
            // with one account's models on it is the same list a person would
            // have filtered to by hand.
            Some(Listed::Account(i)) => {
                let Some(row) = view.accounts().get(i) else {
                    return Step::Stay;
                };
                panel.tab = Tab::Models;
                panel.drill = Some(row.id.clone());
                panel.query.clear();
                panel.cursor = 0;
                Step::Stay
            }
            Some(Listed::Model(i)) => match view.models().get(i) {
                Some(row) => Step::Use { id: row.id.clone() },
                None => Step::Stay,
            },
            None => Step::Stay,
        },
        (Key::Backspace, _) => {
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

/// Open the add form for whichever tab is showing.
///
/// On the models tab with nowhere to put one, nothing opens: the row that would
/// have opened it is not listed either, so this is only reachable by ctrl-a.
fn begin_add(view: &ProvidersView, panel: &mut Panel, secret: &mut String) {
    secret.clear();
    panel.pending_delete = None;
    panel.form = match panel.tab {
        Tab::Accounts => Some(Form::Account(AccountForm::add(view))),
        // Nothing to hang a model off means no form at all — the row that
        // would have opened one is not listed either.
        Tab::Models => ModelForm::add(view, panel.drill.as_deref()).map(Form::Model),
    };
}

/// Open the edit form for the row the cursor is on, when it is one this panel
/// may change.
fn begin_edit(view: &ProvidersView, panel: &mut Panel, secret: &mut String, at: Option<Listed>) {
    secret.clear();
    panel.pending_delete = None;
    panel.form = match at {
        Some(Listed::Group(_)) => None,
        Some(Listed::Account(i)) => view
            .accounts()
            .get(i)
            .filter(|row| !row.managed)
            .map(|row| Form::Account(AccountForm::edit(view, row))),
        Some(Listed::Model(i)) => view
            .models()
            .get(i)
            .filter(|row| !row.managed)
            .and_then(|row| ModelForm::edit(view, row))
            .map(Form::Model),
        Some(Listed::Add) | None => None,
    };
}

/// The two-press delete. The first press arms the row; the second is the one
/// that asks for the write.
fn delete_key(
    view: &ProvidersView,
    panel: &mut Panel,
    at: Option<Listed>,
    armed: Option<String>,
) -> Step {
    let (id, account, managed) = match at {
        Some(Listed::Group(_)) => return Step::Stay,
        Some(Listed::Account(i)) => match view.accounts().get(i) {
            // An offer that was never configured has nothing to delete.
            Some(row) => (row.id.clone(), true, row.managed || !row.configured),
            None => return Step::Stay,
        },
        Some(Listed::Model(i)) => match view.models().get(i) {
            Some(row) => (row.id.clone(), false, row.managed),
            None => return Step::Stay,
        },
        Some(Listed::Add) | None => return Step::Stay,
    };
    if managed {
        return Step::Stay;
    }
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
    match (press.key, press.mods) {
        (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => {
            leave_form(panel, secret);
            return Step::Stay;
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
    panel.form = Some(Form::Account(form));
    Step::Stay
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
    let key = (!secret.trim().is_empty()).then(|| secret.trim().to_string());
    let draft = AccountDraft {
        name,
        protocol: protocol.id.clone(),
        endpoint,
        key,
    };
    let id = form.editing.clone();
    leave_form(panel, secret);
    Step::SaveAccount { id, draft }
}

fn model_key(
    view: &ProvidersView,
    panel: &mut Panel,
    mut form: ModelForm,
    secret: &mut String,
    press: KeyPress,
) -> Step {
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
                ModelField::Account => form.cycle_account(view, forward),
                ModelField::Vision => form.cycle_vision(forward),
                ModelField::Effort => form.cycle_effort(view, forward),
                ModelField::Levels => {
                    let len = form.levels.len().max(1);
                    form.level = match forward {
                        true => (form.level + 1) % len,
                        false => (form.level + len - 1) % len,
                    };
                }
                ModelField::Default => form.default = !form.default,
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
        // Space is the confirm for the fields that are toggles: it is what a
        // person reaches for on a checkbox, and none of those fields take text.
        (Key::Char(' '), Mods::NONE) => match form.focus {
            ModelField::Levels => {
                if let Some(on) = form.levels.get_mut(form.level) {
                    *on = !*on;
                }
            }
            ModelField::Default => form.default = !form.default,
            ModelField::Vision => form.cycle_vision(true),
            ModelField::Effort => form.cycle_effort(view, true),
            _ => {
                if let Some((text, caret)) = form.text_and_caret() {
                    insert_at(text, caret, ' ');
                }
            }
        },
        (Key::Backspace, _) => {
            if form.focus == ModelField::Key {
                secret.pop();
                form.key_len = secret.chars().count();
            } else if let Some((text, caret)) = form.text_and_caret() {
                backspace_at(text, caret);
            }
        }
        (Key::Delete, _) => {
            if form.focus == ModelField::Key {
                secret.clear();
                form.key_len = 0;
            } else if let Some((text, caret)) = form.text_and_caret() {
                delete_at(text, caret);
            }
        }
        (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
            if form.focus == ModelField::Key {
                secret.push(c);
                form.key_len = secret.chars().count();
            } else if let Some((text, caret)) = form.text_and_caret() {
                insert_at(text, caret, c);
            }
        }
        _ => {}
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
    // A window that will not parse is a typo, not a zero: an empty field is how
    // "let the protocol decide" is said, and `0` tokens is not a thing to save.
    let window = form
        .window
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|window| *window > 0);
    if !form.window.trim().is_empty() && window.is_none() {
        let mut form = form.clone();
        form.focus = ModelField::Window;
        panel.form = Some(Form::Model(form));
        return Step::Stay;
    }
    let draft = ModelDraft {
        account,
        model,
        window,
        vision: form.vision,
        effort: form.effort.clone(),
        levels: form.declared_levels(view),
        default: form.default,
        key: (form.needs_key(view) && !secret.trim().is_empty()).then(|| secret.trim().to_string()),
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
            true
        }
        Some(Form::Model(form)) => {
            if form.focus == ModelField::Key {
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
}

/// See [`Providers::probe`]: what to say, and whether all is well.
pub type ProbeFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = (String, bool)> + Send + 'static>>;

#[cfg(test)]
mod tests {
    use super::*;

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
            vision: None,
            effort: None,
            levels: Vec::new(),
            current: false,
            managed: false,
        }
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
        run(&view, &mut panel, &mut secret, &[ctrl('a')]);
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
        run(&view, &mut panel, &mut secret, &[ctrl('a')]);
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
    fn walking_into_an_account_shows_only_its_models() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        // The second account, `local`, has two models.
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Down), press(Key::Enter)],
        );
        assert_eq!(panel.tab, Tab::Models);
        assert_eq!(panel.drill.as_deref(), Some("local"));
        let listed = view.listed(&panel);
        assert_eq!(
            listed,
            vec![Listed::Model(1), Listed::Model(2), Listed::Add],
            "only `local`'s models, and the add row"
        );
    }

    #[test]
    fn a_tab_switch_clears_the_filter_and_the_drill() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Down), press(Key::Enter), press(Key::Char('a'))],
        );
        assert_eq!(panel.query, "a");
        run(&view, &mut panel, &mut secret, &[press(Key::Tab)]);
        assert_eq!(panel.tab, Tab::Accounts);
        assert!(panel.query.is_empty() && panel.drill.is_none());
    }

    #[test]
    fn choosing_a_model_is_the_same_gesture_as_typing_the_command() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        let step = run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Right), press(Key::Enter)],
        );
        assert_eq!(
            step,
            Step::Use {
                id: "deepseek/chat".into()
            }
        );
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
        run(&view, &mut panel, &mut secret, &[ctrl('a')]);
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
        let Step::SaveAccount { id, draft } = step else {
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
        run(&view, &mut panel, &mut secret, &[ctrl('a')]);
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
            &[press(Key::Right), ctrl('a')],
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
    fn a_model_saves_what_the_form_says() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Right), ctrl('a')],
        );
        // The account cycles, the model name is typed, and the window with it.
        let Some(Form::Model(mut form)) = panel.form.clone() else {
            panic!("the model form is up");
        };
        form.model = "vendor-x".into();
        form.window = "64000".into();
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

    #[test]
    fn a_window_that_will_not_parse_is_a_typo_not_a_zero() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        run(
            &view,
            &mut panel,
            &mut secret,
            &[press(Key::Right), ctrl('a')],
        );
        let Some(Form::Model(mut form)) = panel.form.clone() else {
            panic!("the model form is up");
        };
        form.model = "vendor-x".into();
        form.window = "12k".into();
        panel.form = Some(Form::Model(form));
        assert_eq!(
            run(&view, &mut panel, &mut secret, &[press(Key::Enter)]),
            Step::Stay
        );
        let Some(Form::Model(form)) = panel.form.clone() else {
            panic!("the form stays open");
        };
        assert_eq!(form.focus, ModelField::Window);
    }

    #[test]
    fn with_nowhere_to_put_a_model_the_add_row_is_not_offered() {
        let mut managed = account("gateway", 1);
        managed.managed = true;
        let view = ProvidersView::new(
            vec![managed],
            vec![model("gateway/one", "gateway")],
            Vec::new(),
            Vec::new(),
        );
        let mut panel = Panel::new();
        panel.tab = Tab::Models;
        assert_eq!(
            view.listed(&panel),
            vec![Listed::Group(0), Listed::Model(0)],
            "模型按账号分组:一条小标题,底下是它的模型"
        );
    }

    /// A pasted line goes into the field with the keyboard, and only its first
    /// line: a newline written into an id is a newline written into the file.
    #[test]
    fn a_paste_lands_in_the_field_and_never_carries_a_newline() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        run(&view, &mut panel, &mut secret, &[ctrl('a')]);
        assert!(paste(&mut panel, &mut secret, "two\nlines\n"));
        let Some(Form::Account(form)) = panel.form.clone() else {
            panic!("the form is up");
        };
        assert_eq!(form.name, "two");
        assert_eq!(form.caret, 3, "the caret follows what was pasted");

        // And into the key, where it is counted rather than kept.
        let mut panel2 = Panel::new();
        let mut secret2 = String::new();
        run(&view, &mut panel2, &mut secret2, &[ctrl('a')]);
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
        panel.tab = Tab::Models;
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

    /// 小标题停不住:上下键从它上面走过去,回车不会落在一条分界线上。
    #[test]
    fn the_cursor_walks_past_a_heading() {
        let view = view();
        let mut panel = Panel::new();
        let mut secret = String::new();
        run(&view, &mut panel, &mut secret, &[press(Key::Right)]);
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

    /// 下钻到一个账号之后不画小标题:那时整张列表都是它的,再写一遍是废话。
    #[test]
    fn drilling_into_one_account_drops_the_headings() {
        let view = view();
        let mut panel = Panel::new();
        panel.tab = Tab::Models;
        panel.drill = Some(view.models()[0].account.clone());
        let listed = view.listed(&panel);
        assert!(
            !listed.iter().any(|row| matches!(row, Listed::Group(_))),
            "{listed:?}"
        );
    }

    #[test]
    fn the_filter_matches_what_a_person_can_see() {
        let view = view();
        let mut panel = Panel::new();
        panel.query = "loc".into();
        assert_eq!(view.listed(&panel), vec![Listed::Account(1), Listed::Add]);
        panel.tab = Tab::Models;
        panel.query = "local/b".into();
        assert_eq!(
            view.listed(&panel),
            vec![Listed::Group(2), Listed::Model(2), Listed::Add],
            "筛剩一个模型时,它那一组的小标题还在;筛空了的组连标题一起不画"
        );
    }
}
