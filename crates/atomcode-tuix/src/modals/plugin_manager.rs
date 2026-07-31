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
use atomcode_capabilities::plugin::installer::InstalledPluginInfo;
use atomcode_capabilities::plugin::marketplace::MarketplaceInfo;
use atomcode_capabilities::plugin::InstallScope;
use atomcode_capabilities::plugin::PluginJobEvent;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::{build_status, reload_plugins, Buffer, LoopCtx};
use crate::i18n::{t, Msg};
use crate::render::{MenuKind, MenuPayload, Renderer, UiLine};
use crate::state::UiState;

/// Which screen the manager is currently showing.
#[allow(dead_code)]
enum Screen {
    Browse,
    AddUrl,
    Installed,
    /// Scope selection — shown before installing a plugin.
    ScopeSelect {
        plugin: String,
        mp: String,
    },
    /// Installing in progress — shown after scope is selected.
    /// Waits for async install to complete; Esc cancels.
    Installing {
        plugin: String,
        mp: String,
    },
    InstalledDetails {
        plugin: String,
        mp: String,
        scope: InstallScope,
    },
    Marketplaces,
    MarketplaceDetails {
        mp: String,
    },
    RemoveMarketplaceConfirm {
        mp: String,
        from_details: bool,
    },
}

#[derive(Clone)]
struct BrowsePluginItem {
    name: String,
    marketplace: String,
    description: String,
}

pub struct PluginManager {
    screen: Screen,
    selected: usize,
    marketplaces: Vec<MarketplaceInfo>,
    installed: Vec<InstalledPluginInfo>,
    /// Buffer for the Add-marketplace URL entry screen.
    url_input: String,
    /// Cursor character position in url_input.
    url_cursor: usize,
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
    /// Search / filter query. Captures printable characters when typing
    /// on Browse, Plugins, and Installed screens.
    search_query: String,
    /// The install scope selected for the plugin currently being installed.
    installing_scope: Option<InstallScope>,
    is_updating: bool,
}

/// Path-safe segment, mirroring `marketplace::sanitize_name` (which is crate
/// private to atomcode-core). Used only to match a marketplace's raw plugin
/// name against the sanitized name recorded in `installed_plugins.json`.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

impl PluginManager {
    /// Open the manager, loading the current marketplace + installed state.
    /// Infallible: on a read error the lists are simply empty and the screens
    /// show their empty-state hints.
    pub fn open() -> Self {
        let mut m = Self {
            screen: Screen::Browse,
            selected: 0,
            marketplaces: Vec::new(),
            installed: Vec::new(),
            url_input: String::new(),
            url_cursor: 0,
            pending: None,
            installing_plugin: None,
            cancelled_installs: HashSet::new(),
            close_requested: false,
            search_query: String::new(),
            installing_scope: None,
            is_updating: false,
        };
        m.reload();
        m
    }

    fn all_browse_plugins(&self) -> Vec<BrowsePluginItem> {
        let mut items = Vec::new();
        let root_opt = atomcode_capabilities::plugin::marketplaces_root();

        for m in &self.marketplaces {
            let mut descriptions = std::collections::HashMap::new();
            if let Some(ref root) = root_opt {
                let mp_dir = root.join(&m.name);
                if let Ok(Some(manifest)) =
                    atomcode_capabilities::plugin::load_marketplace_manifest(&mp_dir)
                {
                    for p in manifest.plugins {
                        if let Some(desc) = p.description {
                            descriptions.insert(p.name, desc);
                        }
                    }
                }
            }

            for p in &m.plugins {
                let desc = descriptions.get(p).cloned().unwrap_or_default();
                items.push(BrowsePluginItem {
                    name: p.clone(),
                    marketplace: m.name.clone(),
                    description: desc,
                });
            }
        }
        items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        items
    }

    fn filtered_browse_plugins(&self) -> Vec<BrowsePluginItem> {
        let all = self.all_browse_plugins();

        if self.search_query.is_empty() {
            all
        } else {
            let q = self.search_query.to_lowercase();
            all.into_iter()
                .filter(|item| {
                    item.name.to_lowercase().contains(&q)
                        || item.marketplace.to_lowercase().contains(&q)
                        || item.description.to_lowercase().contains(&q)
                })
                .collect()
        }
    }

    fn filtered_installed(&self) -> Vec<InstalledPluginInfo> {
        if self.search_query.is_empty() {
            self.installed.clone()
        } else {
            let q = self.search_query.to_lowercase();
            self.installed
                .iter()
                .filter(|i| i.plugin.to_lowercase().contains(&q))
                .cloned()
                .collect()
        }
    }

    fn tab_bar(&self) -> String {
        let current_tab = match &self.screen {
            Screen::Browse | Screen::ScopeSelect { .. } | Screen::Installing { .. } => 0,
            Screen::Installed | Screen::InstalledDetails { .. } => 1,
            Screen::Marketplaces
            | Screen::AddUrl
            | Screen::MarketplaceDetails { .. }
            | Screen::RemoveMarketplaceConfirm { .. } => 2,
        };
        let installed_count = self.installed.len();
        // Palette-independent active/inactive contrast — see modals::tab_chip
        // (fixed 256-colours, correct on Solarized Dark and every theme).
        let t0 = crate::modals::tab_chip("All Plugins", current_tab == 0);
        let t1 =
            crate::modals::tab_chip(&format!("Installed ({installed_count})"), current_tab == 1);
        let t2 = crate::modals::tab_chip("Marketplaces", current_tab == 2);
        format!("{t0}   {t1}   {t2}")
    }

    fn switch_tab(&mut self, forward: bool) {
        let current_tab = match &self.screen {
            Screen::Browse | Screen::ScopeSelect { .. } | Screen::Installing { .. } => 0,
            Screen::Installed | Screen::InstalledDetails { .. } => 1,
            Screen::Marketplaces
            | Screen::AddUrl
            | Screen::MarketplaceDetails { .. }
            | Screen::RemoveMarketplaceConfirm { .. } => 2,
        };
        let next_tab = if forward {
            (current_tab + 1) % 3
        } else {
            (current_tab + 2) % 3
        };
        self.search_query.clear();
        match next_tab {
            0 => self.goto(Screen::Browse),
            1 => self.goto(Screen::Installed),
            2 => {
                self.goto(Screen::Marketplaces);
            }
            _ => {}
        }
    }

    fn reload(&mut self) {
        self.marketplaces =
            atomcode_capabilities::plugin::marketplace::list_marketplaces().unwrap_or_default();
        self.installed =
            atomcode_capabilities::plugin::installer::list_installed().unwrap_or_default();
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
            Screen::Browse => self.filtered_browse_plugins().len() + 1,
            Screen::Installed => self.filtered_installed().len() + 1,
            Screen::AddUrl => 0,
            Screen::ScopeSelect { .. } => 3, // user / project / local
            Screen::Installing { .. } => 0,  // No selectable rows — just status text
            Screen::InstalledDetails { .. } => 3, // Update, Uninstall, Back
            Screen::Marketplaces => 1 + self.marketplaces.len(),
            Screen::MarketplaceDetails { mp } => {
                if let Some(m) = self.marketplaces.iter().find(|x| &x.name == mp) {
                    if is_official_marketplace(&m.source) {
                        2
                    } else {
                        3
                    }
                } else {
                    0
                }
            }
            Screen::RemoveMarketplaceConfirm { .. } => 2,
        }
    }

    fn is_installed(&self, plugin: &str, mp: &str) -> bool {
        let key = sanitize(plugin);
        self.installed
            .iter()
            .any(|i| i.marketplace == mp && (i.plugin == plugin || i.plugin == key))
    }

    fn get_plugin_details(&self, plugin: &str, mp: &str) -> (Option<String>, Option<String>) {
        let mut version = None;
        let mut description = None;

        if let Some(root) = atomcode_capabilities::plugin::marketplaces_root() {
            let mp_dir = root.join(mp);

            // 1. Try to get details from the marketplace manifest first (as it contains details for both inline and external plugins)
            if let Ok(Some(mp_manifest)) =
                atomcode_capabilities::plugin::load_marketplace_manifest(&mp_dir)
            {
                if let Some(p) = mp_manifest
                    .plugins
                    .iter()
                    .find(|p| p.name == plugin || sanitize(&p.name) == plugin)
                {
                    version = p.version.clone();
                    description = p.description.clone();

                    // If it is an inline plugin, we can try to read its actual plugin.json for more accurate / updated details
                    if let atomcode_capabilities::plugin::PluginSource::Inline(ref path) = p.source
                    {
                        let normalized = path.trim_start_matches("./");
                        let dir = if normalized.is_empty() {
                            mp_dir.clone()
                        } else {
                            mp_dir.join(normalized)
                        };
                        if let Ok(manifest) =
                            atomcode_capabilities::plugin::load_plugin_manifest(&dir)
                        {
                            if manifest.version.is_some() {
                                version = manifest.version;
                            }
                            if manifest.description.is_some() {
                                description = manifest.description;
                            }
                        }
                    }
                }
            }

            // 2. Fall back to loading from mp_dir/plugin (or mp_dir itself) if not found in marketplace manifest
            if version.is_none() && description.is_none() {
                let plugin_dir = mp_dir.join(plugin);
                if let Ok(manifest) =
                    atomcode_capabilities::plugin::load_plugin_manifest(&plugin_dir)
                {
                    if manifest.version.is_some() || manifest.description.is_some() {
                        version = manifest.version;
                        description = manifest.description;
                    }
                }
                if version.is_none() && description.is_none() {
                    if let Ok(manifest) =
                        atomcode_capabilities::plugin::load_plugin_manifest(&mp_dir)
                    {
                        version = manifest.version;
                        description = manifest.description;
                    }
                }
            }
        }
        (version, description)
    }

    fn get_installed_plugin_details(
        &self,
        plugin: &str,
        mp: &str,
        scope: &InstallScope,
    ) -> (Option<String>, Option<String>) {
        let root_opt = atomcode_capabilities::plugin::plugins_root();
        let cwd = std::env::current_dir().ok();
        let dir_opt = match scope {
            InstallScope::User => root_opt,
            InstallScope::Project | InstallScope::Local => {
                if let Some(ref wd) = cwd {
                    atomcode_capabilities::plugin::project_plugins_root(wd, scope)
                } else {
                    None
                }
            }
        };
        if let Some(root) = dir_opt {
            if let Some(info) = self
                .installed
                .iter()
                .find(|i| i.plugin == plugin && i.marketplace == mp && i.scope == *scope)
            {
                let plugin_dir = root.join(&info.plugin_dir);
                if let Ok(manifest) =
                    atomcode_capabilities::plugin::load_plugin_manifest(&plugin_dir)
                {
                    let version = manifest.version;
                    let description = manifest.description;
                    if version.is_some() || description.is_some() {
                        return (version, description);
                    }
                }
            }
        }
        self.get_plugin_details(plugin, mp)
    }

    fn goto(&mut self, screen: Screen) {
        self.screen = screen;
        self.selected = 0;
    }

    // ─── Async dispatch (clone-heavy ops) ───

    fn dispatch_install(
        &mut self,
        plugin: String,
        mp: String,
        scope: InstallScope,
        is_update: bool,
        ctx: &LoopCtx,
    ) {
        let tx = ctx.plugin_job_tx.clone();
        self.installing_plugin = Some(plugin.clone());
        self.installing_scope = Some(scope.clone());
        self.is_updating = is_update;
        if is_update {
            self.pending = Some(t(Msg::PluginMgrUpdating { plugin: &plugin }).into_owned());
        } else {
            self.pending = Some(t(Msg::PluginMgrInstalling { plugin: &plugin }).into_owned());
        }
        tokio::task::spawn_blocking(move || {
            let _ =
                atomcode_capabilities::plugin::installer::uninstall(&plugin, &mp, scope.clone());
            let ev = match atomcode_capabilities::plugin::installer::install(&plugin, &mp, scope) {
                Ok(info) => {
                    if is_update {
                        PluginJobEvent::PluginUpdated(info)
                    } else {
                        PluginJobEvent::PluginInstalled(info)
                    }
                }
                Err(e) => {
                    if let Some(aie) = e
                        .downcast_ref::<atomcode_capabilities::plugin::installer::AlreadyInstalledError>(
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
            let ev = match atomcode_capabilities::plugin::marketplace::add_marketplace(&url) {
                Ok(info) => PluginJobEvent::MarketplaceAdded(info),
                Err(e) => PluginJobEvent::Failed {
                    op: "add".into(),
                    msg: format!("{:#}", e),
                },
            };
            let _ = tx.send(ev);
        });
    }

    fn enter_marketplaces(&mut self, _ctx: &mut LoopCtx, _renderer: &mut dyn Renderer) {
        if self.selected == 0 {
            self.url_input.clear();
            self.url_cursor = 0;
            self.goto(Screen::AddUrl);
        } else {
            if let Some(m) = self.marketplaces.get(self.selected - 1) {
                self.goto(Screen::MarketplaceDetails { mp: m.name.clone() });
            }
        }
    }

    fn enter_marketplace_details(&mut self, _ctx: &mut LoopCtx, _renderer: &mut dyn Renderer) {
        if let Screen::MarketplaceDetails { mp } = &self.screen {
            let mp_name = mp.clone();
            let is_official = self
                .marketplaces
                .iter()
                .find(|x| x.name == mp_name)
                .map(|x| is_official_marketplace(&x.source))
                .unwrap_or(false);

            match self.selected {
                0 => {
                    self.search_query = mp_name;
                    self.goto(Screen::Browse);
                }
                1 => {
                    self.dispatch_update_marketplace(mp_name, _ctx);
                }
                2 if !is_official => {
                    self.goto(Screen::RemoveMarketplaceConfirm {
                        mp: mp_name,
                        from_details: true,
                    });
                }
                _ => {}
            }
        }
    }

    fn confirm_remove_selected_marketplace(&mut self) {
        if self.selected == 0 {
            return;
        }
        if let Some(m) = self.marketplaces.get(self.selected - 1) {
            let name = m.name.clone();
            self.goto(Screen::RemoveMarketplaceConfirm {
                mp: name,
                from_details: false,
            });
        }
    }

    fn enter_remove_marketplace_confirm(&mut self, ctx: &mut LoopCtx, renderer: &mut dyn Renderer) {
        let Screen::RemoveMarketplaceConfirm { mp, from_details } = &self.screen else {
            return;
        };
        let mp_name = mp.clone();
        let from_details = *from_details;
        match self.selected {
            0 => {
                // Yes, remove.
                self.dispatch_remove_marketplace(mp_name, ctx, renderer);
            }
            1 => {
                // No, keep.
                if from_details {
                    self.goto(Screen::MarketplaceDetails { mp: mp_name });
                } else {
                    self.goto(Screen::Marketplaces);
                }
            }
            _ => {}
        }
    }

    fn dispatch_remove_marketplace(
        &mut self,
        name: String,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) {
        let installed_from_mp: Vec<_> = self
            .installed
            .iter()
            .filter(|i| i.marketplace == name)
            .cloned()
            .collect();
        for i in installed_from_mp {
            if let Err(e) = atomcode_capabilities::plugin::installer::uninstall(
                &i.plugin,
                &i.marketplace,
                i.scope,
            ) {
                renderer.render(crate::render::UiLine::Error(format!(
                    "Failed to auto-uninstall plugin '{}': {:#}",
                    i.plugin, e
                )));
            }
        }

        match atomcode_capabilities::plugin::marketplace::remove_marketplace(&name) {
            Ok(()) => {
                reload_plugins(ctx);
                renderer.render(UiLine::CommandOutput(
                    t(Msg::PluginMarketplaceRemoved { name: &name }).into_owned(),
                ));
                self.reload();
                self.goto(Screen::Marketplaces);
            }
            Err(e) => renderer.render(UiLine::Error(format!("{:#}", e))),
        }
        renderer.flush();
    }

    fn dispatch_update_marketplace(&mut self, name: String, ctx: &LoopCtx) {
        let tx = ctx.plugin_job_tx.clone();
        self.pending = Some(format!("Updating marketplace '{}'...", name));
        tokio::task::spawn_blocking(move || {
            let ev = match atomcode_capabilities::plugin::marketplace::update_marketplace(&name) {
                Ok(info) => PluginJobEvent::MarketplaceUpdated(info),
                Err(e) => PluginJobEvent::Failed {
                    op: "update".into(),
                    msg: format!("{:#}", e),
                },
            };
            let _ = tx.send(ev);
        });
    }

    // ─── Per-screen Enter handlers ───

    fn enter_browse(&mut self, _ctx: &mut LoopCtx, _renderer: &mut dyn Renderer) {
        if self.selected == 0 {
            let max = self.current_len().saturating_sub(1);
            if max > 0 {
                self.selected = 1;
            }
            return;
        }
        let plugins = self.filtered_browse_plugins();
        let Some(item) = plugins.get(self.selected - 1) else {
            return;
        };
        let (plugin, mp) = (item.name.clone(), item.marketplace.clone());
        if self.is_installed(&plugin, &mp) {
            let key = sanitize(&plugin);
            let scope = self
                .installed
                .iter()
                .find(|i| i.marketplace == mp && (i.plugin == plugin || i.plugin == key))
                .map(|i| i.scope.clone())
                .unwrap_or(InstallScope::User);
            self.goto(Screen::InstalledDetails { plugin, mp, scope });
        } else {
            self.goto(Screen::ScopeSelect { plugin, mp });
        }
    }

    fn enter_scope_select(&mut self, ctx: &LoopCtx) {
        let Screen::ScopeSelect { plugin, mp } = &self.screen else {
            return;
        };
        let (plugin, mp) = (plugin.clone(), mp.clone());
        let scope = match self.selected {
            0 => InstallScope::User,
            1 => InstallScope::Project,
            2 => InstallScope::Local,
            _ => return,
        };
        self.dispatch_install(plugin, mp, scope, false, ctx);
    }

    fn enter_remove(&mut self, ctx: &mut LoopCtx, renderer: &mut dyn Renderer) {
        if self.selected == 0 {
            return;
        }
        let plugins = self.filtered_browse_plugins();
        let Some(item) = plugins.get(self.selected - 1) else {
            return;
        };
        let name = item.marketplace.clone();
        match atomcode_capabilities::plugin::marketplace::remove_marketplace(&name) {
            Ok(()) => {
                reload_plugins(ctx);
                renderer.render(UiLine::CommandOutput(
                    t(Msg::PluginMarketplaceRemoved { name: &name }).into_owned(),
                ));
                self.reload();
            }
            Err(e) => renderer.render(UiLine::Error(format!("{:#}", e))),
        }
        renderer.flush();
    }

    fn enter_installed(&mut self, _ctx: &mut LoopCtx, _renderer: &mut dyn Renderer) {
        if self.selected == 0 {
            let max = self.current_len().saturating_sub(1);
            if max > 0 {
                self.selected = 1;
            }
            return;
        }
        let inst = self.filtered_installed();
        let Some(i) = inst.get(self.selected - 1) else {
            return;
        };
        let (plugin, mp, scope) = (i.plugin.clone(), i.marketplace.clone(), i.scope.clone());
        self.goto(Screen::InstalledDetails { plugin, mp, scope });
    }

    fn enter_installed_details(&mut self, ctx: &mut LoopCtx, renderer: &mut dyn Renderer) {
        let Screen::InstalledDetails { plugin, mp, scope } = &self.screen else {
            return;
        };
        let (plugin, mp, scope) = (plugin.clone(), mp.clone(), scope.clone());
        match self.selected {
            0 => {
                // Uninstall
                match atomcode_capabilities::plugin::installer::uninstall(&plugin, &mp, scope) {
                    Ok(()) => {
                        reload_plugins(ctx);
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::PluginUninstalled {
                                plugin: &plugin,
                                marketplace: &mp,
                            })
                            .into_owned(),
                        ));
                        self.close_requested = true;
                    }
                    Err(e) => renderer.render(UiLine::Error(
                        t(Msg::PluginUninstallFailed {
                            error: &format!("{:#}", e),
                        })
                        .into_owned(),
                    )),
                }
                renderer.flush();
            }
            1 => {
                // Update
                self.dispatch_install(plugin, mp, scope, true, ctx);
            }
            2 => {
                // Back to parent
                self.goto(Screen::Installed);
            }
            _ => {}
        }
    }

    // ─── Rendering helpers ───

    /// Build (rows, hint) for the current screen.
    fn rows(&self) -> (Vec<(String, String)>, String) {
        match &self.screen {
            Screen::Browse => {
                let plugins = self.filtered_browse_plugins();
                if plugins.is_empty() {
                    let hint = if self.search_query.is_empty() {
                        t(Msg::PluginMgrEmptyPlugins).into_owned()
                    } else {
                        format!("No plugins match '{}'", self.search_query)
                    };
                    return (Vec::new(), hint);
                }
                let installing_label = t(Msg::PluginMgrInstallingStatus).into_owned();
                let installed_label = t(Msg::PluginMgrInstalledStatus).into_owned();
                let rows = plugins
                    .iter()
                    .map(|item| {
                        let status =
                            if self.installing_plugin.as_deref() == Some(item.name.as_str()) {
                                format!("@{} ({})", item.marketplace, installing_label)
                            } else if self.is_installed(&item.name, &item.marketplace) {
                                format!("@{} ({})", item.marketplace, installed_label)
                            } else {
                                format!("@{}  {}", item.marketplace, get_mock_category(&item.name))
                            };
                        let desc = if item.description.is_empty() {
                            status
                        } else {
                            format!("{}  ·  {}", status, item.description)
                        };
                        (item.name.clone(), desc)
                    })
                    .collect();
                let hint = if self.pending.is_some() {
                    t(Msg::PluginMgrHintPending).into_owned()
                } else {
                    t(Msg::PluginMgrHintToggle).into_owned()
                };
                (rows, hint)
            }
            Screen::Installed => {
                let inst = self.filtered_installed();
                if inst.is_empty() {
                    let hint = if self.search_query.is_empty() {
                        t(Msg::PluginMgrEmptyInstalled).into_owned()
                    } else {
                        format!("No installed plugins match '{}'", self.search_query)
                    };
                    return (Vec::new(), hint);
                }

                let root_opt = atomcode_capabilities::plugin::plugins_root();
                let cwd = std::env::current_dir().ok();
                let mp_root_opt = atomcode_capabilities::plugin::marketplaces_root();
                let mut descriptions = std::collections::HashMap::new();
                for i in &inst {
                    let mut found_desc = None;
                    let dir_opt = match i.scope {
                        InstallScope::User => root_opt.clone(),
                        InstallScope::Project | InstallScope::Local => {
                            if let Some(ref wd) = cwd {
                                atomcode_capabilities::plugin::project_plugins_root(wd, &i.scope)
                            } else {
                                None
                            }
                        }
                    };
                    if let Some(root) = dir_opt {
                        let plugin_dir = root.join(&i.plugin_dir);
                        if let Ok(manifest) =
                            atomcode_capabilities::plugin::load_plugin_manifest(&plugin_dir)
                        {
                            if let Some(desc) = manifest.description {
                                if !desc.trim().is_empty() {
                                    found_desc = Some(desc);
                                }
                            }
                        }
                    }
                    if found_desc.is_none() {
                        if let Some(ref mp_root) = mp_root_opt {
                            let mp_dir = mp_root.join(&i.marketplace);
                            if let Ok(Some(mp_manifest)) =
                                atomcode_capabilities::plugin::load_marketplace_manifest(&mp_dir)
                            {
                                if let Some(p) = mp_manifest
                                    .plugins
                                    .iter()
                                    .find(|p| p.name == i.plugin || sanitize(&p.name) == i.plugin)
                                {
                                    if let Some(desc) = &p.description {
                                        if !desc.trim().is_empty() {
                                            found_desc = Some(desc.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if let Some(desc) = found_desc {
                        descriptions.insert(i.plugin.clone(), desc);
                    }
                }

                let rows = inst
                    .iter()
                    .map(|i| {
                        let scope_label = match i.scope {
                            InstallScope::User => t(Msg::PluginScopeUserShort).into_owned(),
                            InstallScope::Project => t(Msg::PluginScopeProjectShort).into_owned(),
                            InstallScope::Local => t(Msg::PluginScopeLocalShort).into_owned(),
                        };
                        let name = i.plugin.clone();
                        let status = format!("@{} ({})", i.marketplace, scope_label);
                        let desc = if let Some(d) = descriptions.get(&i.plugin) {
                            if !d.is_empty() {
                                format!("{}  ·  {}", status, d)
                            } else {
                                status
                            }
                        } else {
                            status
                        };
                        (name, desc)
                    })
                    .collect();
                (rows, t(Msg::PluginMgrHintUninstall).into_owned())
            }
            Screen::AddUrl => {
                let rows = vec![
                    ("Add Marketplace".to_string(), String::new()),
                    (String::new(), String::new()),
                    ("Enter marketplace source:".to_string(), String::new()),
                    ("Examples:".to_string(), String::new()),
                    (
                        "  · git@atomgit.com:owner/repo.git (SSH)".to_string(),
                        String::new(),
                    ),
                    (
                        "  · https://example.com/marketplace.json".to_string(),
                        String::new(),
                    ),
                    ("  · ./path/to/marketplace".to_string(), String::new()),
                    (String::new(), String::new()),
                ];
                (rows, t(Msg::PluginMgrHintUrl).into_owned())
            }
            Screen::Marketplaces => {
                let mut rows = Vec::new();
                for m in &self.marketplaces {
                    let available = m.plugins.len();
                    let installed = self
                        .installed
                        .iter()
                        .filter(|i| i.marketplace == m.name)
                        .count();
                    let updated = get_directory_modified_date(&m.name);
                    let desc = format!("{}|{}|{}|{}", m.source, available, installed, updated);
                    rows.push((m.name.clone(), desc));
                }
                (rows, t(Msg::PluginMgrHintNav).into_owned())
            }
            Screen::ScopeSelect { .. } => {
                let installing = self.installing_plugin.is_some();
                let (user_lbl, user_desc) =
                    if installing && self.installing_scope == Some(InstallScope::User) {
                        (
                            format!("{}...", t(Msg::PluginMgrInstallingStatus)),
                            "".to_string(),
                        )
                    } else {
                        (
                            t(Msg::PluginScopeUser).into_owned(),
                            t(Msg::PluginScopeUserDesc).into_owned(),
                        )
                    };
                let (project_lbl, project_desc) =
                    if installing && self.installing_scope == Some(InstallScope::Project) {
                        (
                            format!("{}...", t(Msg::PluginMgrInstallingStatus)),
                            "".to_string(),
                        )
                    } else {
                        (
                            t(Msg::PluginScopeProject).into_owned(),
                            t(Msg::PluginScopeProjectDesc).into_owned(),
                        )
                    };
                let (local_lbl, local_desc) =
                    if installing && self.installing_scope == Some(InstallScope::Local) {
                        (
                            format!("{}...", t(Msg::PluginMgrInstallingStatus)),
                            "".to_string(),
                        )
                    } else {
                        (
                            t(Msg::PluginScopeLocal).into_owned(),
                            t(Msg::PluginScopeLocalDesc).into_owned(),
                        )
                    };

                let rows = vec![
                    (user_lbl, user_desc),
                    (project_lbl, project_desc),
                    (local_lbl, local_desc),
                ];
                let hint = if installing {
                    t(Msg::PluginMgrHintPending).into_owned()
                } else {
                    t(Msg::PluginScopeHint).into_owned()
                };
                (rows, hint)
            }
            Screen::Installing { plugin, .. } => {
                let hint = format!(
                    "{} ({})",
                    t(Msg::PluginMgrInstalling { plugin }),
                    t(Msg::PluginMgrEscToCancel)
                );
                (Vec::new(), hint)
            }
            Screen::InstalledDetails { plugin, .. } => {
                let installing = self.installing_plugin.as_deref() == Some(plugin.as_str());
                let (update_label, update_desc) = if installing {
                    (
                        format!("{}...", t(Msg::PluginMgrUpdatingStatus)),
                        "".to_string(),
                    )
                } else {
                    (
                        t(Msg::PluginActionUpdate).into_owned(),
                        t(Msg::PluginActionUpdateDesc).into_owned(),
                    )
                };

                let rows = vec![
                    (
                        t(Msg::PluginActionUninstall).into_owned(),
                        t(Msg::PluginActionUninstallDesc).into_owned(),
                    ),
                    (update_label, update_desc),
                    (
                        t(Msg::PluginActionBack).into_owned(),
                        t(Msg::PluginActionBackDesc).into_owned(),
                    ),
                ];
                let hint = if installing {
                    t(Msg::PluginMgrHintUpdating).into_owned()
                } else {
                    "↑↓ Select action · Enter confirm · Esc back".to_string()
                };
                (rows, hint)
            }
            Screen::MarketplaceDetails { mp } => {
                let mut rows = Vec::new();
                if let Some(m) = self.marketplaces.iter().find(|x| &x.name == mp) {
                    let available_count = m.plugins.len();
                    rows.push((
                        format!("Browse plugins ({})", available_count),
                        String::new(),
                    ));

                    let last_updated = get_directory_modified_date(&m.name);
                    rows.push((
                        format!("Update marketplace (last updated {})", last_updated),
                        String::new(),
                    ));

                    if !is_official_marketplace(&m.source) {
                        rows.push(("Remove marketplace".to_string(), String::new()));
                    }
                }
                (rows, t(Msg::PluginMgrHintNav).into_owned())
            }
            Screen::RemoveMarketplaceConfirm { .. } => {
                let rows = vec![
                    (
                        t(Msg::PluginMgrRemoveMarketplaceYes).into_owned(),
                        String::new(),
                    ),
                    (
                        t(Msg::PluginMgrRemoveMarketplaceNo).into_owned(),
                        String::new(),
                    ),
                ];
                (rows, t(Msg::PluginMgrRemoveMarketplaceHint).into_owned())
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
        // If an install/clone is pending, restrict key inputs.
        // ESC is allowed so the user can cancel the action.
        // Other navigation keys (Up, Down, Left, Right, Tab, BackTab) and inputs are blocked.
        if self.pending.is_some() {
            match code {
                KeyCode::Esc => {
                    // fall through to normal Esc handling
                }
                _ => {
                    // swallow all other key inputs
                    return Ok(ModalAction::Continue);
                }
            }
        }

        // Text-entry screen: capture characters into url_input.
        if matches!(self.screen, Screen::AddUrl) {
            match code {
                KeyCode::Esc => {
                    self.url_input.clear();
                    self.url_cursor = 0;
                    self.goto(Screen::Marketplaces);
                }
                KeyCode::Left => {
                    if self.url_cursor > 0 {
                        self.url_cursor -= 1;
                    }
                }
                KeyCode::Right => {
                    let char_len = self.url_input.chars().count();
                    if self.url_cursor < char_len {
                        self.url_cursor += 1;
                    }
                }
                KeyCode::Home => {
                    self.url_cursor = 0;
                }
                KeyCode::End => {
                    self.url_cursor = self.url_input.chars().count();
                }
                KeyCode::BackTab => {}
                KeyCode::Tab => {}
                KeyCode::Enter => {
                    let url = self.url_input.trim().to_string();
                    if !url.is_empty() {
                        self.dispatch_add(url, ctx);
                        self.url_input.clear();
                        self.url_cursor = 0;
                    }
                }
                KeyCode::Backspace => {
                    let mut chars: Vec<char> = self.url_input.chars().collect();
                    if self.url_cursor > 0 && self.url_cursor <= chars.len() {
                        chars.remove(self.url_cursor - 1);
                        self.url_cursor -= 1;
                        self.url_input = chars.into_iter().collect();
                    }
                }
                KeyCode::Delete => {
                    let mut chars: Vec<char> = self.url_input.chars().collect();
                    if self.url_cursor < chars.len() {
                        chars.remove(self.url_cursor);
                        self.url_input = chars.into_iter().collect();
                    }
                }
                KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                    let mut chars: Vec<char> = self.url_input.chars().collect();
                    if self.url_cursor <= chars.len() {
                        chars.insert(self.url_cursor, c);
                        self.url_cursor += 1;
                        self.url_input = chars.into_iter().collect();
                    }
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
            KeyCode::Left | KeyCode::BackTab => {
                if matches!(
                    self.screen,
                    Screen::Browse | Screen::Installed | Screen::Marketplaces
                ) {
                    self.switch_tab(false);
                }
            }
            KeyCode::Right | KeyCode::Tab => {
                if matches!(
                    self.screen,
                    Screen::Browse | Screen::Installed | Screen::Marketplaces
                ) {
                    self.switch_tab(true);
                }
            }
            KeyCode::Backspace => {
                if !self.search_query.is_empty() {
                    self.search_query.pop();
                    self.selected = 0;
                } else if matches!(self.screen, Screen::Browse) {
                    self.enter_remove(ctx, renderer);
                } else if matches!(self.screen, Screen::Marketplaces) {
                    self.confirm_remove_selected_marketplace();
                }
            }
            KeyCode::Delete => {
                if matches!(self.screen, Screen::Browse) {
                    self.enter_remove(ctx, renderer);
                } else if matches!(self.screen, Screen::Marketplaces) {
                    self.confirm_remove_selected_marketplace();
                }
            }
            KeyCode::Esc => {
                if !self.search_query.is_empty() {
                    self.search_query.clear();
                    self.selected = 0;
                } else {
                    match &self.screen {
                        Screen::ScopeSelect { plugin, mp } => {
                            if self.installing_plugin.is_some() {
                                let id = format!("{}@{}", sanitize(plugin), mp);
                                self.cancelled_installs.insert(id);
                                self.pending = None;
                                self.installing_plugin = None;
                                self.installing_scope = None;
                            }
                            self.goto(Screen::Browse);
                        }
                        Screen::Installing { plugin, mp } => {
                            let id = format!("{}@{}", sanitize(plugin), mp);
                            self.cancelled_installs.insert(id);
                            self.pending = None;
                            self.installing_plugin = None;
                            self.installing_scope = None;
                            self.goto(Screen::Browse);
                        }
                        Screen::InstalledDetails { plugin, mp, .. } => {
                            if self.installing_plugin.is_some() {
                                let id = format!("{}@{}", sanitize(plugin), mp);
                                self.cancelled_installs.insert(id);
                                self.pending = None;
                                self.installing_plugin = None;
                                self.installing_scope = None;
                            }
                            self.goto(Screen::Installed);
                        }
                        Screen::MarketplaceDetails { .. } => {
                            self.goto(Screen::Marketplaces);
                        }
                        Screen::RemoveMarketplaceConfirm { mp, from_details } => {
                            if *from_details {
                                self.goto(Screen::MarketplaceDetails { mp: mp.clone() });
                            } else {
                                self.goto(Screen::Marketplaces);
                            }
                        }
                        _ => {
                            return Ok(ModalAction::Close);
                        }
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
                        Screen::Browse => self.enter_browse(ctx, renderer),
                        Screen::ScopeSelect { .. } => self.enter_scope_select(ctx),
                        Screen::Installing { .. } => {
                            // No-op: already installing, Enter does nothing.
                        }
                        Screen::Installed => self.enter_installed(ctx, renderer),
                        Screen::InstalledDetails { .. } => {
                            self.enter_installed_details(ctx, renderer)
                        }
                        Screen::AddUrl => {}
                        Screen::Marketplaces => self.enter_marketplaces(ctx, renderer),
                        Screen::MarketplaceDetails { .. } => {
                            self.enter_marketplace_details(ctx, renderer)
                        }
                        Screen::RemoveMarketplaceConfirm { .. } => {
                            self.enter_remove_marketplace_confirm(ctx, renderer)
                        }
                    }
                }
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                self.search_query.push(c);
                self.selected = 0;
            }
            _ => {}
        }
        if std::mem::take(&mut self.close_requested) {
            return Ok(ModalAction::Close);
        }
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
    }

    fn draw(&self, _buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
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

        let mut final_items = Vec::new();
        // Prepend tab bar and a blank line separator
        final_items.push((self.tab_bar(), String::new()));
        final_items.push((String::new(), String::new()));

        let mut selected_offset = 2;

        if matches!(self.screen, Screen::Browse | Screen::Installed) {
            final_items.push((self.search_query.clone(), String::new()));
            final_items.push((String::new(), String::new()));
            selected_offset += 2;
        }

        if let Screen::RemoveMarketplaceConfirm { mp, .. } = &self.screen {
            final_items.push((
                t(Msg::PluginMgrRemoveMarketplaceTitle).into_owned(),
                String::new(),
            ));
            final_items.push((
                format!("  Marketplace: {}{}\x1b[39m", muted_esc(), mp),
                String::new(),
            ));
            final_items.push((String::new(), String::new()));
            final_items.push((
                t(Msg::PluginMgrRemoveMarketplacePrompt { name: mp }).into_owned(),
                String::new(),
            ));
            final_items.push((String::new(), String::new()));
            let mut added_lines = 5;

            let installed_from_mp: Vec<_> = self
                .installed
                .iter()
                .filter(|i| i.marketplace == *mp)
                .collect();
            if !installed_from_mp.is_empty() {
                let warning_text = if crate::i18n::current_locale() == crate::i18n::Locale::ZhCn {
                    format!(
                        "  此操作将同时卸载该市场下的 {} 个插件：",
                        installed_from_mp.len()
                    )
                } else {
                    format!(
                        "  This will also uninstall {} plugins from this marketplace:",
                        installed_from_mp.len()
                    )
                };
                final_items.push((format!("\x1b[33m{}\x1b[39m", warning_text), String::new()));
                added_lines += 1;
                for i in &installed_from_mp {
                    final_items.push((
                        format!("    • {}{}\x1b[39m", muted_esc(), i.plugin),
                        String::new(),
                    ));
                    added_lines += 1;
                }
                final_items.push((String::new(), String::new()));
                added_lines += 1;
            }
            selected_offset += added_lines;
        }

        let details_opt = match &self.screen {
            Screen::ScopeSelect { plugin, mp } | Screen::Installing { plugin, mp } => {
                let (version, description) = self.get_plugin_details(plugin, mp);
                Some((plugin.clone(), mp.clone(), version, description, None))
            }
            Screen::InstalledDetails { plugin, mp, scope } => {
                let (version, description) = self.get_installed_plugin_details(plugin, mp, scope);
                Some((
                    plugin.clone(),
                    mp.clone(),
                    version,
                    description,
                    Some(scope.clone()),
                ))
            }
            _ => None,
        };

        if let Some((plugin, mp, version, description, scope_opt)) = details_opt {
            final_items.push(("  ◆ Plugin Info".to_string(), String::new()));
            final_items.push((format!("  Name:        {}", plugin), String::new()));
            final_items.push((
                format!("  Marketplace: {}@{}\x1b[39m", muted_esc(), mp),
                String::new(),
            ));
            final_items.push((
                format!(
                    "  Version:     {}{}\x1b[39m",
                    muted_esc(),
                    version.unwrap_or_else(|| "unknown".to_string())
                ),
                String::new(),
            ));
            selected_offset += 4;
            if let Some(scope) = scope_opt {
                let scope_label = match scope {
                    InstallScope::User => t(Msg::PluginScopeUserShort).into_owned(),
                    InstallScope::Project => t(Msg::PluginScopeProjectShort).into_owned(),
                    InstallScope::Local => t(Msg::PluginScopeLocalShort).into_owned(),
                };
                final_items.push((
                    format!("  Scope:       {}{}\x1b[39m", muted_esc(), scope_label),
                    String::new(),
                ));
                selected_offset += 1;
            }
            if let Some(desc) = description {
                let trimmed = desc.trim();
                if !trimmed.is_empty() {
                    let truncated = truncate_plugin_desc(trimmed);
                    final_items.push((
                        format!("  Description: {}{}\x1b[39m", muted_esc(), truncated),
                        String::new(),
                    ));
                    selected_offset += 1;
                }
            }
            final_items.push((String::new(), String::new()));
            if matches!(self.screen, Screen::ScopeSelect { .. }) {
                final_items.push(("  Select Install Scope:".to_string(), String::new()));
                final_items.push((String::new(), String::new()));
                selected_offset += 3;
            } else if matches!(self.screen, Screen::InstalledDetails { .. }) {
                final_items.push(("  Manage Plugin:".to_string(), String::new()));
                final_items.push((String::new(), String::new()));
                selected_offset += 3;
            } else {
                let installing_label = t(Msg::PluginMgrInstallingStatus).into_owned();
                final_items.push((
                    format!("  Status:      {}...", installing_label),
                    String::new(),
                ));
                final_items.push((String::new(), String::new()));
                selected_offset += 3;
            }
        }

        if let Screen::MarketplaceDetails { mp } = &self.screen {
            if let Some(m) = self.marketplaces.iter().find(|x| &x.name == mp) {
                let installed_plugins: Vec<&InstalledPluginInfo> = self
                    .installed
                    .iter()
                    .filter(|i| &i.marketplace == mp)
                    .collect();
                let installed_count = installed_plugins.len();

                final_items.push((format!("  \x1b[1m{}\x1b[22m", m.name), String::new()));
                final_items.push((
                    format!("  {}{}\x1b[39m", muted_esc(), m.source),
                    String::new(),
                ));
                final_items.push((String::new(), String::new()));
                final_items.push((
                    format!("  {} available plugins", m.plugins.len()),
                    String::new(),
                ));
                final_items.push((String::new(), String::new()));
                final_items.push((
                    format!("  \x1b[1mInstalled plugins ({}):\x1b[22m", installed_count),
                    String::new(),
                ));

                if installed_count == 0 {
                    final_items.push((
                        format!(
                            "  {}No plugins installed from this marketplace.\x1b[39m",
                            muted_esc()
                        ),
                        String::new(),
                    ));
                } else {
                    for inst in installed_plugins {
                        final_items.push((
                            format!("  {}● {}\x1b[39m", muted_esc(), inst.plugin),
                            String::new(),
                        ));
                        let (_, description) = self.get_installed_plugin_details(
                            &inst.plugin,
                            &inst.marketplace,
                            &inst.scope,
                        );
                        if let Some(desc) = description {
                            let trimmed = desc.trim();
                            if !trimmed.is_empty() {
                                let truncated = truncate_plugin_desc(trimmed);
                                final_items.push((
                                    format!("    {}{}\x1b[39m", muted_esc(), truncated),
                                    String::new(),
                                ));
                            }
                        }
                    }
                }
                final_items.push((String::new(), String::new()));
                selected_offset = final_items.len();
            }
        }

        if matches!(self.screen, Screen::Marketplaces) {
            final_items.push(("+ Add Marketplace".to_string(), String::new()));
            final_items.push((String::new(), String::new()));
            for item in items {
                final_items.push(item);
                final_items.push((String::new(), String::new()));
            }
        } else {
            for item in items {
                final_items.push(item);
            }
            if matches!(
                self.screen,
                Screen::InstalledDetails { .. }
                    | Screen::ScopeSelect { .. }
                    | Screen::MarketplaceDetails { .. }
                    | Screen::RemoveMarketplaceConfirm { .. }
            ) {
                final_items.push((String::new(), String::new()));
            } else if matches!(self.screen, Screen::AddUrl) {
                final_items.push((format!("❯ {}", self.url_input), String::new()));
            }
        }
        final_items.push((format!("— {} —", hint), String::new()));

        let selectable = self.current_len();
        let selected = if selectable == 0 {
            // No items are selectable, so nothing should be highlighted.
            final_items.len()
        } else if matches!(self.screen, Screen::Marketplaces) {
            if self.selected == 0 {
                2
            } else {
                4 + (self.selected - 1) * 2
            }
        } else if matches!(self.screen, Screen::Browse | Screen::Installed) {
            if self.selected == 0 {
                2
            } else {
                (self.selected + 3).min(final_items.len().saturating_sub(2))
            }
        } else {
            (self.selected + selected_offset).min(final_items.len().saturating_sub(2))
        };
        let kind = match &self.screen {
            Screen::Browse | Screen::Installed => MenuKind::Plugin,
            Screen::Marketplaces | Screen::AddUrl => MenuKind::Marketplace,
            Screen::ScopeSelect { .. }
            | Screen::Installing { .. }
            | Screen::InstalledDetails { .. }
            | Screen::MarketplaceDetails { .. }
            | Screen::RemoveMarketplaceConfirm { .. } => MenuKind::PluginInfo,
        };
        let payload = MenuPayload {
            items: final_items,
            selected,
            kind,
        };

        let (text, cursor) = if matches!(self.screen, Screen::AddUrl) {
            let byte_idx = self
                .url_input
                .chars()
                .take(self.url_cursor)
                .map(|c| c.len_utf8())
                .sum::<usize>();
            (self.url_input.clone(), byte_idx)
        } else {
            (self.search_query.clone(), self.search_query.len())
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
        self.installing_scope = None;
        self.is_updating = false;

        // If a cancelled install completed successfully, roll it back
        // (uninstall) so the filesystem is not left in an inconsistent
        // state.  This handles the race where the user presses Esc while
        // a git clone is still in flight: the clone finishes and updates
        // installed_plugins.json, but the user already expressed intent to
        // cancel.
        if let PluginJobEvent::PluginInstalled(info) | PluginJobEvent::PluginUpdated(info) = ev {
            let id = format!("{}@{}", info.plugin, info.marketplace);
            if self.cancelled_installs.take(&id).is_some() {
                // Best-effort rollback — if this fails the stale dir
                // cleanup logic in install_external will handle it on
                // the next install attempt.
                let _ = atomcode_capabilities::plugin::installer::uninstall(
                    &info.plugin,
                    &info.marketplace,
                    info.scope.clone(),
                );
            } else {
                self.close_requested = true;
            }
        }

        self.reload();
        // If we were on the Installing screen, navigate back to the
        // Browse screen so the user sees the updated installed state.
        if let Screen::Installing { .. } = &self.screen {
            self.goto(Screen::Browse);
        }
        if let Screen::AddUrl = &self.screen {
            self.goto(Screen::Marketplaces);
        }
    }

    fn close_requested(&self) -> bool {
        self.close_requested
    }
}

fn get_directory_modified_date(name: &str) -> String {
    if let Some(root) = atomcode_capabilities::plugin::marketplaces_root() {
        let dir = root.join(name);
        let target = if dir.join(".atomcode-plugin/marketplace.json").exists() {
            dir.join(".atomcode-plugin/marketplace.json")
        } else if dir.join(".git").exists() {
            dir.join(".git")
        } else {
            dir
        };
        if let Ok(metadata) = std::fs::metadata(&target) {
            if let Ok(modified) = metadata.modified() {
                let dt: chrono::DateTime<chrono::Local> = modified.into();
                return dt.format("%-m/%-d/%Y").to_string();
            }
        }
    }
    "unknown".to_string()
}

/// Display-column budget for a plugin description rendered on one line
/// (includes the trailing `…` column when truncation happens).
const PLUGIN_DESC_DISPLAY_COLS: usize = 60;

/// Truncate a plugin description for single-line display in the manager.
/// `trimmed` is caller-trimmed and non-empty. Uses the grapheme/CJK-safe
/// `truncate_with_ellipsis` — a raw byte slice here would panic on a
/// non-ASCII description and, under `panic = "abort"`, crash the process.
fn truncate_plugin_desc(trimmed: &str) -> String {
    crate::width::truncate_with_ellipsis(trimmed, PLUGIN_DESC_DISPLAY_COLS)
}

fn is_official_marketplace(source: &str) -> bool {
    source == "https://atomgit.com/atomgit_atomcode/atomcode-plugins-official.git"
        || source == "git@atomgit.com:atomgit_atomcode/atomcode-plugins-official.git"
}

fn muted_esc() -> &'static str {
    if crate::highlight::theme::is_light_for_render() {
        "\x1b[90m"
    } else {
        "\x1b[37m"
    }
}

fn get_mock_category(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    if lower.contains("git") || lower.contains("commit") || lower.contains("lens") {
        "Git"
    } else if lower.contains("lint") || lower.contains("check") || lower.contains("eslint") {
        "Linter"
    } else if lower.contains("format") || lower.contains("prettier") || lower.contains("style") {
        "Formatter"
    } else if lower.contains("rust")
        || lower.contains("go")
        || lower.contains("python")
        || lower.contains("lang")
        || lower.contains("analyzer")
    {
        "Language"
    } else if lower.contains("security") || lower.contains("auth") || lower.contains("guard") {
        "Security"
    } else if lower.contains("ai")
        || lower.contains("gpt")
        || lower.contains("copilot")
        || lower.contains("model")
    {
        "AI"
    } else {
        let mut hash = 0u64;
        for c in name.chars() {
            hash = hash.wrapping_add(c as u64).wrapping_mul(31);
        }
        match hash % 3 {
            0 => "Utility",
            1 => "Tool",
            _ => "Completion",
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
            screen: Screen::Browse,
            selected: 0,
            marketplaces: mps,
            installed: inst,
            url_input: String::new(),
            url_cursor: 0,
            pending: None,
            installing_plugin: None,
            cancelled_installs: HashSet::new(),
            close_requested: false,
            search_query: String::new(),
            installing_scope: None,
            is_updating: false,
        }
    }

    #[test]
    fn browse_displays_all_plugins() {
        let m = manager(
            vec![
                mk_mp("official", &["a", "b"]),
                mk_mp("community", &["c", "d"]),
            ],
            vec![],
        );
        // Starts at Browse screen
        assert!(matches!(m.screen, Screen::Browse));
        assert_eq!(m.selected, 0);
        assert_eq!(m.current_len(), 5);

        let plugins = m.all_browse_plugins();
        assert_eq!(plugins.len(), 4);
        assert_eq!(plugins[0].name, "a");
        assert_eq!(plugins[0].marketplace, "official");
        assert_eq!(plugins[2].name, "c");
        assert_eq!(plugins[2].marketplace, "community");
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
    fn installed_count_in_tab_bar() {
        let m = manager(vec![], vec![mk_installed("a", "x"), mk_installed("b", "x")]);
        let bar = m.tab_bar();
        assert!(bar.contains("Installed (2)"));
    }

    #[test]
    fn tab_bar_colors_on_dark_theme() {
        let _theme = crate::highlight::theme::test_lock();
        let m = manager(vec![], vec![]);
        crate::highlight::theme::set_theme_mode(false); // force dark
        let bar = m.tab_bar();
        crate::highlight::theme::set_theme_mode(false); // restore
                                                        // Palette-independent chips (shared modals::tab_chip): active = fixed
                                                        // near-white 231, inactive = fixed grey 245. No SGR 90/37/1;39 — all
                                                        // broke on Solarized Dark.
        assert!(
            bar.contains("\x1b[1;38;5;231mAll Plugins\x1b[22;39m"),
            "active tab should be bold + fixed near-white 231; got: {bar}"
        );
        assert!(
            bar.contains("\x1b[38;5;245m"),
            "inactive tabs should use fixed grey 245; got: {bar}"
        );
        assert!(
            !bar.contains("\x1b[1;90m") && !bar.contains("\x1b[90m") && !bar.contains("\x1b[37m"),
            "tabs must not use palette-dependent SGR 90/37; got: {bar}"
        );
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

    /// Regression: a plugin description containing CJK text must not panic when
    /// truncated. The old code byte-sliced `&trimmed[..57]`, which panics when
    /// byte 57 lands inside a multi-byte character — and under `panic = "abort"`
    /// that aborts the whole process (repro: `/plugin` → a large official
    /// marketplace with Chinese descriptions → Update).
    #[test]
    fn truncate_plugin_desc_cjk_does_not_panic() {
        // 2 ASCII + 20×3-byte CJK = 62 bytes; byte 57 sits inside the char
        // spanning bytes 56–58, so the old `&trimmed[..57]` slice panicked here.
        let straddling = format!("ab{}", "描".repeat(20));
        assert!(
            !straddling.is_char_boundary(57),
            "fixture must straddle byte 57 to exercise the old panic"
        );
        // Only 42 display columns (CJK = 2 cols each) → under budget, returned
        // unchanged. The point is that it no longer panics.
        assert_eq!(truncate_plugin_desc(&straddling), straddling);

        // A description wider than the budget is truncated char-safely with an
        // ellipsis, never exceeding the budget and never splitting a CJK char.
        let wide = "描".repeat(40); // 80 display columns
        let out = truncate_plugin_desc(&wide);
        assert!(out.ends_with('…'), "over-budget description must be marked truncated");
        assert!(crate::width::display_width(&out) <= PLUGIN_DESC_DISPLAY_COLS);
        assert!(
            out.chars().all(|c| c == '描' || c == '…'),
            "truncation must not split a CJK character"
        );

        // Short ASCII is returned unchanged.
        assert_eq!(truncate_plugin_desc("hello"), "hello");
    }

    #[test]
    fn scope_select_has_three_rows() {
        let m = manager(vec![], vec![]);
        let mut m2 = m;
        m2.goto(Screen::ScopeSelect {
            plugin: "test".into(),
            mp: "mp".into(),
        });
        assert_eq!(m2.current_len(), 3);
    }

    #[test]
    fn scope_select_rows_have_labels() {
        let mut m = manager(vec![], vec![]);
        m.goto(Screen::ScopeSelect {
            plugin: "test".into(),
            mp: "mp".into(),
        });
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
        m.goto(Screen::Installing {
            plugin: "discrawl".into(),
            mp: "mp".into(),
        });
        let (rows, hint) = m.rows();
        assert!(rows.is_empty());
        assert!(hint.contains("discrawl"));
        assert!(hint.contains("Esc"));
    }

    #[test]
    fn search_filtering_browse_plugins() {
        let mut m = manager(
            vec![
                mk_mp("official", &["git-lens", "rust-analyzer"]),
                mk_mp("community", &["go-pls"]),
            ],
            vec![],
        );
        assert_eq!(m.filtered_browse_plugins().len(), 3);

        m.search_query = "rust".to_string();
        let filtered = m.filtered_browse_plugins();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "rust-analyzer");
        assert_eq!(filtered[0].marketplace, "official");

        m.search_query = "comm".to_string(); // filters by marketplace name too
        let filtered = m.filtered_browse_plugins();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "go-pls");
        assert_eq!(filtered[0].marketplace, "community");
    }

    #[test]
    fn search_query_clears_on_tab_switch() {
        let mut m = manager(vec![], vec![]);
        m.search_query = "rust".to_string();
        m.switch_tab(true);
        assert!(m.search_query.is_empty());
    }

    #[test]
    fn search_filtering_by_description() {
        let items = vec![BrowsePluginItem {
            name: "git-lens".to_string(),
            marketplace: "official".to_string(),
            description: "Visualizes code history".to_string(),
        }];
        let q = "history".to_string();
        let filtered: Vec<_> = items
            .into_iter()
            .filter(|item| {
                item.name.to_lowercase().contains(&q)
                    || item.marketplace.to_lowercase().contains(&q)
                    || item.description.to_lowercase().contains(&q)
            })
            .collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "git-lens");
    }

    #[test]
    fn browse_retains_installed_plugins() {
        let m = manager(
            vec![
                mk_mp("official", &["git-lens", "rust-analyzer"]),
                mk_mp("community", &["go-pls"]),
            ],
            vec![mk_installed("git-lens", "official")],
        );
        let browse = m.filtered_browse_plugins();
        assert_eq!(browse.len(), 3);
        assert_eq!(browse[0].name, "git-lens");
        assert_eq!(browse[1].name, "go-pls");
        assert_eq!(browse[2].name, "rust-analyzer");
    }

    #[test]
    fn browse_shows_categories() {
        let m = manager(vec![mk_mp("official", &["git-lens"])], vec![]);
        let (rows, _) = m.rows();
        assert_eq!(rows.len(), 1);
        let (_name, desc) = &rows[0];
        assert!(desc.contains("Git"));
    }

    #[test]
    fn installed_list_shows_scopes() {
        let mut m = manager(
            vec![],
            vec![InstalledPluginInfo {
                plugin: "git-lens".to_string(),
                marketplace: "official".to_string(),
                plugin_dir: "installed/official/git-lens".to_string(),
                scope: InstallScope::Project,
            }],
        );
        m.goto(Screen::Installed);
        let (rows, _) = m.rows();
        assert_eq!(rows.len(), 1);
        let (name, desc) = &rows[0];
        assert_eq!(name, "git-lens");
        assert!(desc.contains("project") || desc.contains("项目级"));
    }

    #[test]
    fn installed_details_actions() {
        let mut m = manager(
            vec![],
            vec![InstalledPluginInfo {
                plugin: "git-lens".to_string(),
                marketplace: "official".to_string(),
                plugin_dir: "installed/official/git-lens".to_string(),
                scope: InstallScope::Project,
            }],
        );
        m.goto(Screen::InstalledDetails {
            plugin: "git-lens".to_string(),
            mp: "official".to_string(),
            scope: InstallScope::Project,
        });
        assert_eq!(m.current_len(), 3);
        let (rows, _) = m.rows();
        assert_eq!(rows.len(), 3);
        // Action options
        assert!(rows[0].0.contains("Uninstall") || rows[0].0.contains("卸载"));
        assert!(rows[1].0.contains("Update") || rows[1].0.contains("更新"));
        assert!(rows[2].0.contains("Back") || rows[2].0.contains("返回"));
    }

    #[test]
    fn marketplaces_screen_rows_and_navigation() {
        let mps = vec![MarketplaceInfo {
            name: "test-marketplace".to_string(),
            source: "https://example.com/test-marketplace.git".to_string(),
            git_commit: "commit123".to_string(),
            plugins: vec!["plugin1".to_string(), "plugin2".to_string()],
        }];
        let mut m = manager(mps, vec![]);
        m.goto(Screen::Marketplaces);

        // current_len should be 1 (+ Add Marketplace) + 1 (test-marketplace) = 2
        assert_eq!(m.current_len(), 2);

        // rows should return test-marketplace with formatting info
        let (rows, _) = m.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "test-marketplace");
        assert!(rows[0]
            .1
            .contains("https://example.com/test-marketplace.git"));
        assert!(rows[0].1.contains("2|0|unknown")); // 2 available, 0 installed, unknown updated
    }

    #[test]
    fn add_marketplace_help_screen_rows() {
        let mut m = manager(vec![], vec![]);
        m.goto(Screen::AddUrl);

        // current_len should be 0 since it is text-entry only
        assert_eq!(m.current_len(), 0);

        let (rows, hint) = m.rows();
        assert_eq!(rows.len(), 8);
        assert_eq!(rows[0].0, "Add Marketplace");
        assert_eq!(rows[2].0, "Enter marketplace source:");
        assert_eq!(rows[3].0, "Examples:");
        assert!(rows[4].0.contains("git@atomgit.com"));
        assert!(rows[5].0.contains("https://example.com/marketplace.json"));
        assert!(rows[6].0.contains("./path/to/marketplace"));
        assert!(hint.contains("to add") || hint.contains("添加"));
    }
}
