// crates/atomcode-tuix/src/modals/plugin_manager.rs
//
// `/plugin` (no subcommand) modal — a full interactive plugin manager.
//
// One modal, an internal `Screen` state machine. From `Home` you can:
//   • Browse marketplaces → list a marketplace's plugins → Enter toggles
//     install/uninstall.
//   • Add marketplace…  → type/paste a git URL → Enter clones (async).
//   • Remove marketplace… → pick a marketplace → Enter removes (sync).
//   • Installed → pick an installed plugin → Enter uninstalls (sync).
//
// Sub-screens return to `Home` on Esc; `Home`'s Esc closes the modal.
// Slow ops that clone (add marketplace, install) go through the existing
// async `plugin_job_tx` pipeline; the event loop calls `on_plugin_event`
// when they finish so the modal can refresh its lists. Fast ops (uninstall,
// remove marketplace) run inline and refresh immediately.

use std::collections::HashSet;

use anyhow::Result;
use atomcode_core::plugin::installer::InstalledPluginInfo;
use atomcode_core::plugin::marketplace::MarketplaceInfo;
use atomcode_core::plugin::InstallScope;
use atomcode_core::plugin::PluginJobEvent;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::{build_status, reload_plugins, Buffer, LoopCtx};
use crate::i18n::{t, Msg};
use crate::render::{MenuKind, MenuPayload, Renderer, UiLine};
use crate::state::UiState;

/// Which screen the manager is currently showing.
enum Screen {
    Home,
    Browse,
    Plugins { mp: String },
    AddUrl,
    RemoveMarketplace,
    Installed,
    /// Scope selection — shown before installing a plugin.
    ScopeSelect { plugin: String, mp: String },
    /// Installing in progress — shown after scope is selected.
    /// Waits for async install to complete; Esc cancels.
    Installing { plugin: String, mp: String },
}

pub struct PluginManager {
    screen: Screen,
    selected: usize,
    marketplaces: Vec<MarketplaceInfo>,
    installed: Vec<InstalledPluginInfo>,
    /// Buffer for the Add-marketplace URL entry screen.
    url_input: String,
    /// Set while an async clone/install is in flight; shown as a status row
    /// and cleared by `on_plugin_event` when the job result arrives.
    pending: Option<String>,
    /// The plugin name currently being installed (used to show ⏳ in the
    /// list). Cleared together with `pending` in `on_plugin_event`.
    installing_plugin: Option<String>,
    /// Canonical plugin ids (`plugin@marketplace`) for installs that were
    /// cancelled by the user pressing Esc while on the `Installing` screen.
    /// When the background install job completes, `on_plugin_event` checks
    /// this set: if the plugin id is present, the install is rolled back
    /// (uninstalled) automatically so the filesystem is not left in an
    /// inconsistent state.
    cancelled_installs: HashSet<String>,
    /// Set by a successful uninstall / marketplace-removal so `handle_key`
    /// closes the manager afterwards. Without this the modal lingered on a
    /// now-empty list whose render is indistinguishable from the idle prompt,
    /// silently swallowing keystrokes until the user discovered Esc.
    close_requested: bool,
}

/// Path-safe segment, mirroring `marketplace::sanitize_name` (which is crate
/// private to atomcode-core). Used only to match a marketplace's raw plugin
/// name against the sanitized name recorded in `installed_plugins.json`.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

impl PluginManager {
    /// Open the manager, loading the current marketplace + installed state.
    /// Infallible: on a read error the lists are simply empty and the screens
    /// show their empty-state hints.
    pub fn open() -> Self {
        let mut m = Self {
            screen: Screen::Home,
            selected: 0,
            marketplaces: Vec::new(),
            installed: Vec::new(),
            url_input: String::new(),
            pending: None,
            installing_plugin: None,
            cancelled_installs: HashSet::new(),
            close_requested: false,
        };
        m.reload();
        m
    }

    fn reload(&mut self) {
        self.marketplaces =
            atomcode_core::plugin::marketplace::list_marketplaces().unwrap_or_default();
        self.installed = atomcode_core::plugin::installer::list_installed().unwrap_or_default();
        let n = self.current_len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    /// Number of selectable rows on the current screen (for nav clamping).
    fn current_len(&self) -> usize {
        match &self.screen {
            Screen::Home => 4,
            Screen::Browse | Screen::RemoveMarketplace => self.marketplaces.len(),
            Screen::Plugins { mp } => self.plugins_of(mp).map(|p| p.len()).unwrap_or(0),
            Screen::Installed => self.installed.len(),
            Screen::AddUrl => 0,
            Screen::ScopeSelect { .. } => 3, // user / project / local
            Screen::Installing { .. } => 0, // No selectable rows — just status text
        }
    }

    fn plugins_of(&self, mp: &str) -> Option<&Vec<String>> {
        self.marketplaces
            .iter()
            .find(|m| m.name == mp)
            .map(|m| &m.plugins)
    }

    fn is_installed(&self, plugin: &str, mp: &str) -> bool {
        let key = sanitize(plugin);
        self.installed
            .iter()
            .any(|i| i.marketplace == mp && (i.plugin == plugin || i.plugin == key))
    }

    fn goto(&mut self, screen: Screen) {
        self.screen = screen;
        self.selected = 0;
    }

    // ─── Async dispatch (clone-heavy ops) ───

    fn dispatch_install(&mut self, plugin: String, mp: String, scope: InstallScope, ctx: &LoopCtx) {
        let tx = ctx.plugin_job_tx.clone();
        self.installing_plugin = Some(plugin.clone());
        self.pending = Some(t(Msg::PluginMgrInstalling { plugin: &plugin }).into_owned());
        tokio::task::spawn_blocking(move || {
            let ev = match atomcode_core::plugin::installer::install(&plugin, &mp, scope) {
                Ok(info) => PluginJobEvent::PluginInstalled(info),
                Err(e) => {
                    if let Some(aie) = e
                        .downcast_ref::<atomcode_core::plugin::installer::AlreadyInstalledError>(
                    ) {
                        PluginJobEvent::PluginAlreadyInstalled { id: aie.id.clone() }
                    } else {
                        PluginJobEvent::Failed { op: "install".into(), msg: format!("{:#}", e) }
                    }
                }
            };
            let _ = tx.send(ev);
        });
    }

    fn dispatch_add(&mut self, url: String, ctx: &LoopCtx) {
        let tx = ctx.plugin_job_tx.clone();
        self.pending = Some(t(Msg::PluginMgrCloning).into_owned());
        tokio::task::spawn_blocking(move || {
            let ev = match atomcode_core::plugin::marketplace::add_marketplace(&url) {
                Ok(info) => PluginJobEvent::MarketplaceAdded(info),
                Err(e) => PluginJobEvent::Failed { op: "add".into(), msg: format!("{:#}", e) },
            };
            let _ = tx.send(ev);
        });
    }

    // ─── Per-screen Enter handlers ───

    fn enter_home(&mut self) {
        match self.selected {
            0 => self.goto(Screen::Browse),
            1 => {
                self.url_input.clear();
                self.goto(Screen::AddUrl);
            }
            2 => self.goto(Screen::RemoveMarketplace),
            3 => self.goto(Screen::Installed),
            _ => {}
        }
    }

    fn enter_browse(&mut self) {
        if let Some(m) = self.marketplaces.get(self.selected) {
            let mp = m.name.clone();
            self.goto(Screen::Plugins { mp });
        }
    }

    fn enter_plugins(&mut self, ctx: &mut LoopCtx, renderer: &mut dyn Renderer) {
        let Screen::Plugins { mp } = &self.screen else { return };
        let mp = mp.clone();
        let Some(plugin) = self.plugins_of(&mp).and_then(|p| p.get(self.selected)).cloned() else {
            return;
        };
        if self.is_installed(&plugin, &mp) {
            // Uninstall is fast — run inline and refresh.
            // Look up the actual scope of the installed plugin so we
            // uninstall from the right location (User / Project / Local).
            let key = sanitize(&plugin);
            let scope = self
                .installed
                .iter()
                .find(|i| {
                    i.marketplace == mp && (i.plugin == plugin || i.plugin == key)
                })
                .map(|i| i.scope.clone())
                .unwrap_or(InstallScope::User);
            match atomcode_core::plugin::installer::uninstall(&plugin, &mp, scope) {
                Ok(()) => {
                    reload_plugins(ctx);
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::PluginUninstalled { plugin: &plugin, marketplace: &mp })
                            .into_owned(),
                    ));
                    self.reload();
                    // Stay in the manager after a successful uninstall so the
                    // user can keep uninstalling. `reload` already refreshes
                    // the lists and clamps `selected`; empty screens now show
                    // an "esc back" hint, so lingering no longer mimics the
                    // idle prompt.
                }
                Err(e) => renderer.render(UiLine::Error(
                    t(Msg::PluginUninstallFailed { error: &format!("{:#}", e) }).into_owned(),
                )),
            }
            renderer.flush();
        } else {
            // Show scope selection instead of installing directly.
            self.goto(Screen::ScopeSelect { plugin, mp });
        }
    }

    fn enter_scope_select(&mut self, ctx: &LoopCtx) {
        let Screen::ScopeSelect { plugin, mp } = &self.screen else { return };
        let (plugin, mp) = (plugin.clone(), mp.clone());
        let scope = match self.selected {
            0 => InstallScope::User,
            1 => InstallScope::Project,
            2 => InstallScope::Local,
            _ => return,
        };
        self.dispatch_install(plugin.clone(), mp.clone(), scope, ctx);
        // Stay on an Installing screen so the user sees the progress state,
        // mirroring Claude Code's UX.  on_plugin_event will navigate back
        // to the Plugins list once the job completes.
        self.screen = Screen::Installing { plugin, mp };
        self.selected = 0;
    }

    fn enter_remove(&mut self, ctx: &mut LoopCtx, renderer: &mut dyn Renderer) {
        let Some(m) = self.marketplaces.get(self.selected) else { return };
        let name = m.name.clone();
        match atomcode_core::plugin::marketplace::remove_marketplace(&name) {
            Ok(()) => {
                reload_plugins(ctx);
                renderer.render(UiLine::CommandOutput(
                    t(Msg::PluginMarketplaceRemoved { name: &name }).into_owned(),
                ));
                self.reload();
                self.close_requested = true;
            }
            Err(e) => renderer.render(UiLine::Error(format!("{:#}", e))),
        }
        renderer.flush();
    }

    fn enter_installed(&mut self, ctx: &mut LoopCtx, renderer: &mut dyn Renderer) {
        let Some(i) = self.installed.get(self.selected) else { return };
        let (plugin, mp, scope) = (i.plugin.clone(), i.marketplace.clone(), i.scope.clone());
        match atomcode_core::plugin::installer::uninstall(&plugin, &mp, scope) {
            Ok(()) => {
                reload_plugins(ctx);
                renderer.render(UiLine::CommandOutput(
                    t(Msg::PluginUninstalled { plugin: &plugin, marketplace: &mp }).into_owned(),
                ));
                self.reload();
                // Stay in the manager so the user can keep uninstalling (see
                // the Plugins-screen uninstall path for the rationale).
            }
            Err(e) => renderer.render(UiLine::Error(
                t(Msg::PluginUninstallFailed { error: &format!("{:#}", e) }).into_owned(),
            )),
        }
        renderer.flush();
    }

    // ─── Rendering helpers ───

    /// Build (rows, hint) for the current screen.
    fn rows(&self) -> (Vec<(String, String)>, String) {
        match &self.screen {
            Screen::Home => {
                let rows = vec![
                    (t(Msg::PluginMgrBrowse).into_owned(), String::new()),
                    (t(Msg::PluginMgrAdd).into_owned(), String::new()),
                    (t(Msg::PluginMgrRemove).into_owned(), String::new()),
                    (
                        t(Msg::PluginMgrInstalled { count: self.installed.len() }).into_owned(),
                        String::new(),
                    ),
                ];
                (rows, t(Msg::PluginMgrHintNav).into_owned())
            }
            Screen::Browse => {
                if self.marketplaces.is_empty() {
                    return (Vec::new(), t(Msg::PluginMgrEmptyMarketplaces).into_owned());
                }
                let rows = self
                    .marketplaces
                    .iter()
                    .map(|m| (m.name.clone(), format!("{} plugins", m.plugins.len())))
                    .collect();
                (rows, t(Msg::PluginMgrHintNav).into_owned())
            }
            Screen::Plugins { mp } => {
                let mark = t(Msg::PluginMgrInstalledMark).into_owned();
                let installing_label = t(Msg::PluginMgrInstallingLabel).into_owned();
                let rows: Vec<(String, String)> = self
                    .plugins_of(mp)
                    .map(|plugins| {
                        plugins
                            .iter()
                            .map(|p| {
                                let desc = if self.installing_plugin.as_deref() == Some(p.as_str()) {
                                    installing_label.clone()
                                } else if self.is_installed(p, mp) {
                                    mark.clone()
                                } else {
                                    String::new()
                                };
                                (p.clone(), desc)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if rows.is_empty() {
                    return (Vec::new(), t(Msg::PluginMgrEmptyPlugins).into_owned());
                }
                // Show installing hint when pending, otherwise normal toggle hint.
                let hint = if self.pending.is_some() {
                    t(Msg::PluginMgrHintPending).into_owned()
                } else {
                    t(Msg::PluginMgrHintToggle).into_owned()
                };
                (rows, hint)
            }
            Screen::RemoveMarketplace => {
                if self.marketplaces.is_empty() {
                    return (Vec::new(), t(Msg::PluginMgrEmptyMarketplaces).into_owned());
                }
                let rows = self
                    .marketplaces
                    .iter()
                    .map(|m| (m.name.clone(), m.source.clone()))
                    .collect();
                (rows, t(Msg::PluginMgrHintRemove).into_owned())
            }
            Screen::Installed => {
                if self.installed.is_empty() {
                    return (Vec::new(), t(Msg::PluginMgrEmptyInstalled).into_owned());
                }
                let rows = self
                    .installed
                    .iter()
                    .map(|i| {
                        let scope_label = match i.scope {
                            InstallScope::User => String::new(),
                            InstallScope::Project => " [project]".into(),
                            InstallScope::Local => " [local]".into(),
                        };
                        (format!("{}{}", i.plugin, scope_label), format!("@{}", i.marketplace))
                    })
                    .collect();
                (rows, t(Msg::PluginMgrHintUninstall).into_owned())
            }
            Screen::AddUrl => (Vec::new(), t(Msg::PluginMgrHintUrl).into_owned()),
            Screen::ScopeSelect { .. } => {
                let rows = vec![
                    (t(Msg::PluginScopeUser).into_owned(), t(Msg::PluginScopeUserDesc).into_owned()),
                    (t(Msg::PluginScopeProject).into_owned(), t(Msg::PluginScopeProjectDesc).into_owned()),
                    (t(Msg::PluginScopeLocal).into_owned(), t(Msg::PluginScopeLocalDesc).into_owned()),
                ];
                (rows, t(Msg::PluginScopeHint).into_owned())
            }
            Screen::Installing { plugin, .. } => {
                let hint = format!(
                    "{} ({})",
                    t(Msg::PluginMgrInstalling { plugin }),
                    t(Msg::PluginMgrEscToCancel)
                );
                (Vec::new(), hint)
            }
        }
    }
}

impl Modal for PluginManager {
    fn handle_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        // Text-entry screen: capture characters into url_input.
        if matches!(self.screen, Screen::AddUrl) {
            match code {
                KeyCode::Esc => {
                    self.url_input.clear();
                    self.goto(Screen::Home);
                }
                KeyCode::Enter => {
                    let url = self.url_input.trim().to_string();
                    if !url.is_empty() {
                        self.dispatch_add(url, ctx);
                        self.url_input.clear();
                        self.goto(Screen::Home);
                    }
                }
                KeyCode::Backspace => {
                    self.url_input.pop();
                }
                KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                    self.url_input.push(c);
                }
                _ => {}
            }
            self.draw(buf, state, ctx, renderer);
            return Ok(ModalAction::Continue);
        }

        match code {
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                let max = self.current_len().saturating_sub(1);
                if self.selected < max {
                    self.selected += 1;
                }
            }
            KeyCode::Esc => {
                if matches!(self.screen, Screen::Home) {
                    return Ok(ModalAction::Close);
                }
                match &self.screen {
                    Screen::ScopeSelect { plugin: _, mp } => {
                        // Scope select → go back to Plugins list.
                        let mp = mp.clone();
                        self.goto(Screen::Plugins { mp });
                    }
                    Screen::Installing { plugin, mp } => {
                        // Mark the install as cancelled so that when the
                        // background job completes, on_plugin_event rolls
                        // it back automatically.
                        let id = format!("{}@{}", sanitize(plugin), mp);
                        self.cancelled_installs.insert(id);
                        self.pending = None;
                        self.installing_plugin = None;
                        let mp = mp.clone();
                        self.goto(Screen::Plugins { mp });
                    }
                    _ => {
                        self.goto(Screen::Home);
                    }
                }
            }
            KeyCode::Enter => {
                // Block Enter while an async install/clone is in flight
                // to prevent duplicate triggers.
                if self.pending.is_some() {
                    // no-op: swallow the keystroke
                } else {
                    match &self.screen {
                        Screen::Home => self.enter_home(),
                        Screen::Browse => self.enter_browse(),
                        Screen::Plugins { .. } => self.enter_plugins(ctx, renderer),
                        Screen::ScopeSelect { .. } => self.enter_scope_select(ctx),
                        Screen::Installing { .. } => {
                            // No-op: already installing, Enter does nothing.
                        }
                        Screen::RemoveMarketplace => self.enter_remove(ctx, renderer),
                        Screen::Installed => self.enter_installed(ctx, renderer),
                        Screen::AddUrl => {}
                    }
                }
            }
            _ => {}
        }
        // Marketplace removal closes the manager (removing a marketplace is a
        // bigger, rarer action). Plugin uninstall deliberately does NOT — the
        // user usually wants to uninstall several in a row.
        if std::mem::take(&mut self.close_requested) {
            return Ok(ModalAction::Close);
        }
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
    }

    fn draw(&self, buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        let (items, hint) = self.rows();
        // Status row: show a pending spinner-ish note if a job is in flight,
        // otherwise the navigation hint for this screen.
        let hint = match &self.screen {
            Screen::Installing { .. } => hint,
            _ => match &self.pending {
                Some(p) => p.clone(),
                None => hint,
            },
        };
        // The hint rides as the last (non-selectable) menu row's label so it
        // is visible under the list without a dedicated widget.
        let mut items = items;
        items.push((format!("— {} —", hint), String::new()));
        let selectable = self.current_len();
        let selected = if selectable == 0 {
            // No items are selectable, so nothing should be highlighted.
            // Using an out-of-bounds index (like items.len()) ensures that.
            items.len()
        } else {
            self.selected.min(selectable.saturating_sub(1))
        };
        let payload = MenuPayload { items, selected, kind: MenuKind::SlashCommand };

        let (text, cursor) = if matches!(self.screen, Screen::AddUrl) {
            (self.url_input.clone(), self.url_input.len())
        } else {
            (buf.text.clone(), buf.cursor)
        };
        renderer.render(UiLine::InputPrompt {
            buf: text,
            cursor_byte: cursor,
            menu: Some(payload),
            status: build_status(state, ctx),
            attachments: Vec::new(),
        });
        renderer.flush();
    }

    fn handle_paste(
        &mut self,
        text: &str,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        // On the Add-marketplace screen, paste lands in the URL field (git
        // URLs are usually pasted). Elsewhere there is no text entry, so drop
        // the paste rather than disturbing the hidden composer buffer.
        if matches!(self.screen, Screen::AddUrl) {
            self.url_input
                .push_str(text.trim().lines().next().unwrap_or("").trim());
        }
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
    }

    fn on_plugin_event(&mut self, ev: &PluginJobEvent) {
        self.pending = None;
        self.installing_plugin = None;

        // If a cancelled install completed successfully, roll it back
        // (uninstall) so the filesystem is not left in an inconsistent
        // state.  This handles the race where the user presses Esc while
        // a git clone is still in flight: the clone finishes and updates
        // installed_plugins.json, but the user already expressed intent to
        // cancel.
        if let PluginJobEvent::PluginInstalled(info) = ev {
            let id = format!("{}@{}", info.plugin, info.marketplace);
            if self.cancelled_installs.take(&id).is_some() {
                // Best-effort rollback — if this fails the stale dir
                // cleanup logic in install_external will handle it on
                // the next install attempt.
                let _ = atomcode_core::plugin::installer::uninstall(
                    &info.plugin,
                    &info.marketplace,
                    info.scope.clone(),
                );
            }
        }

        self.reload();
        // If we were on the Installing screen, navigate back to the
        // Plugins list so the user sees the updated installed state.
        if let Screen::Installing { mp, .. } = &self.screen {
            let mp = mp.clone();
            self.goto(Screen::Plugins { mp });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_mp(name: &str, plugins: &[&str]) -> MarketplaceInfo {
        MarketplaceInfo {
            name: name.to_string(),
            source: format!("https://example/{}.git", name),
            git_commit: "abc1234".to_string(),
            plugins: plugins.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn mk_installed(plugin: &str, mp: &str) -> InstalledPluginInfo {
        InstalledPluginInfo {
            plugin: plugin.to_string(),
            marketplace: mp.to_string(),
            plugin_dir: format!("installed/{}/{}", mp, plugin),
            scope: InstallScope::User,
        }
    }

    fn manager(mps: Vec<MarketplaceInfo>, inst: Vec<InstalledPluginInfo>) -> PluginManager {
        PluginManager {
            screen: Screen::Home,
            selected: 0,
            marketplaces: mps,
            installed: inst,
            url_input: String::new(),
            pending: None,
            installing_plugin: None,
            cancelled_installs: HashSet::new(),
            close_requested: false,
        }
    }

    #[test]
    fn home_has_four_rows() {
        let m = manager(vec![], vec![]);
        assert_eq!(m.current_len(), 4);
    }

    #[test]
    fn browse_then_plugins_navigation() {
        let mut m = manager(vec![mk_mp("official", &["a", "b", "c"])], vec![]);
        // Home → Browse
        m.selected = 0;
        m.enter_home();
        assert!(matches!(m.screen, Screen::Browse));
        assert_eq!(m.selected, 0);
        assert_eq!(m.current_len(), 1);
        // Browse → Plugins{official}
        m.enter_browse();
        assert!(matches!(&m.screen, Screen::Plugins { mp } if mp == "official"));
        assert_eq!(m.current_len(), 3);
    }

    #[test]
    fn installed_mark_matches_sanitized_name() {
        // Marketplace lists a raw name; installed records the sanitized one.
        let m = manager(
            vec![mk_mp("official", &["my plugin"])],
            vec![mk_installed("my-plugin", "official")],
        );
        assert!(m.is_installed("my plugin", "official"));
        assert!(!m.is_installed("my plugin", "other"));
        assert!(!m.is_installed("absent", "official"));
    }

    #[test]
    fn goto_resets_selection() {
        let mut m = manager(vec![mk_mp("x", &["a", "b"])], vec![]);
        m.selected = 1;
        m.goto(Screen::Browse);
        assert_eq!(m.selected, 0);
    }

    #[test]
    fn installed_count_in_home_rows() {
        let m = manager(vec![], vec![mk_installed("a", "x"), mk_installed("b", "x")]);
        let (rows, _hint) = m.rows();
        // 4 home rows; the Installed row label should mention the count 2.
        assert_eq!(rows.len(), 4);
        assert!(rows[3].0.contains('2'));
    }

    #[test]
    fn addurl_text_entry_accumulates() {
        let mut m = manager(vec![], vec![]);
        m.goto(Screen::AddUrl);
        for c in "git".chars() {
            m.url_input.push(c);
        }
        assert_eq!(m.url_input, "git");
        m.url_input.pop();
        assert_eq!(m.url_input, "gi");
    }

    #[test]
    fn scope_select_has_three_rows() {
        let m = manager(vec![], vec![]);
        let mut m2 = m;
        m2.goto(Screen::ScopeSelect { plugin: "test".into(), mp: "mp".into() });
        assert_eq!(m2.current_len(), 3);
    }

    #[test]
    fn scope_select_rows_have_labels() {
        let mut m = manager(vec![], vec![]);
        m.goto(Screen::ScopeSelect { plugin: "test".into(), mp: "mp".into() });
        let (rows, _hint) = m.rows();
        assert_eq!(rows.len(), 3);
        // Each row should have a non-empty label and description.
        for (label, desc) in &rows {
            assert!(!label.is_empty());
            assert!(!desc.is_empty());
        }
    }

    #[test]
    fn installing_screen_has_no_rows_and_carries_hint() {
        let mut m = manager(vec![], vec![]);
        m.goto(Screen::Installing { plugin: "discrawl".into(), mp: "mp".into() });
        let (rows, hint) = m.rows();
        assert!(rows.is_empty());
        assert!(hint.contains("discrawl"));
        assert!(hint.contains("Esc"));
    }
}
