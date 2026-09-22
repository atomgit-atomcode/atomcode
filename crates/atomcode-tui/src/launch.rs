//! The screen as an App of its own, the one way it is mounted.
//!
//! The screen and the agent are two Apps (`docs/adr/0022` §3). This is the
//! screen's: a surface, the `ui-tui2` row and one row per panel. It builds no
//! agent and knows no product assembly — whoever launches it hosts the agent
//! and hands over a [`HostConnection`]. The command-line entry does that for
//! the product runtime; the end-to-end tests do it for a tree the harness
//! mounts itself. Both mount the screen through here, so what is tested is what
//! ships.

use std::sync::Arc;

use atomcode_harness::seams::{UiSvc, UserInterface};
use atomcode_host_api::HostConnection;
use atomcode_plexus::{App, ConfigTree, Layer, Plugin, PluginRegistry};

use crate::plugin::{
    Connection, ConnectionSvc, HeadlessSurfacePlugin, ModulesSvc, SurfaceSvc,
    TerminalSurfacePlugin, TuiUiPlugin,
};

/// How the screen is drawn. Everything here is a row edit on the screen's own
/// App; nothing reaches the agent.
#[derive(Clone, Debug)]
pub struct Screen {
    /// Paint into memory at this size instead of taking the terminal.
    pub headless: Option<(u16, u16)>,
    /// Bring the cat: the `tui-panel-mascot` row on.
    pub mascot: bool,
    /// `auto`, `dark` or `light`; `None` asks the terminal.
    pub theme: Option<String>,
    /// Take the pointer. Off leaves selection to the terminal.
    pub mouse: bool,
}

impl Default for Screen {
    fn default() -> Self {
        Self {
            headless: None,
            mascot: false,
            theme: None,
            mouse: true,
        }
    }
}

/// Every row the screen can mount.
///
/// This is the screen's own rows. A launcher that has rows of its own adds them
/// through [`catalog_with`] — the screen does not know, and must not have to,
/// which product is in front of it (`docs/adr/0022` §3).
pub fn catalog() -> PluginRegistry {
    catalog_with(&[])
}

/// [`catalog`] plus the launcher's own rows.
///
/// The opening a launcher needs is a *row*, not a call: a plugin registered here
/// takes its place in the tree like every other one, so it can be `[[remove]]`d,
/// shows up in `--audit`, and reaches the screen only through the services the
/// tree provides. A launcher that reached past this into `Modules` and
/// `add_view`ed would be the hard-coded panel that `crate::rows` exists to
/// replace.
///
/// A duplicate name panics inside `register`, which is deliberate: two
/// implementations answering to one name is a build-time mistake, not a runtime
/// condition.
pub fn catalog_with(extra: &[Arc<dyn Plugin>]) -> PluginRegistry {
    let mut registry = PluginRegistry::new();
    registry
        .register(Arc::new(TuiUiPlugin))
        .register(Arc::new(TerminalSurfacePlugin))
        .register(Arc::new(HeadlessSurfacePlugin));
    for row in crate::rows::catalog() {
        registry.register(row);
    }
    for row in extra {
        registry.register(row.clone());
    }
    registry
}

/// The screen's tree: the surface, `ui-tui2`, the panels of
/// [`crate::rows::SCREEN`] — the team strip on, since a screen in front of an
/// agent that can delegate is what it is for — then `extra`, in order.
pub fn tree(screen: &Screen, extra: &[&str]) -> Result<ConfigTree, String> {
    let surface = match screen.headless {
        Some((width, height)) => format!(
            "[[insert]]\nid = \"surface\"\nname = \"surface-headless\"\n\
             config = {{ width = {width}, height = {height} }}\n"
        ),
        None => {
            let mut config = vec![format!("mouse = {}", screen.mouse)];
            if let Some(theme) = &screen.theme {
                config.push(format!(
                    "theme = {}",
                    atomcode_harness::bundle::toml_string(theme)
                ));
            }
            format!(
                "[[insert]]\nid = \"surface\"\nname = \"surface-terminal\"\nconfig = {{ {} }}\n",
                config.join(", ")
            )
        }
    };
    let mut layers = vec![
        format!("{surface}\n[[insert]]\nid = \"ui\"\nname = \"ui-tui2\"\n"),
        crate::rows::SCREEN.to_string(),
        "[[patch]]\nid = \"tui-panel-team\"\ndisabled = false\n".to_string(),
    ];
    if screen.mascot {
        layers.push("[[patch]]\nid = \"tui-panel-mascot\"\ndisabled = false\n".to_string());
    }
    layers.extend(extra.iter().map(|layer| layer.to_string()));
    let layers = layers
        .iter()
        .map(|layer| Layer::from_toml(layer).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    ConfigTree::from_layers(layers).map_err(|e| e.to_string())
}

/// The seams a launcher fills, once the tree is up.
///
/// A struct rather than a parameter each: the screen has grown two of these
/// ports and will grow more, and every one added as an argument is every caller
/// edited to pass `None` again. A launcher fills what it has
/// (`docs/adr/0022` §3); what it leaves out is a seam with nothing behind it,
/// and the panel that wanted it says so rather than pretending.
#[derive(Default, Clone)]
pub struct Ports {
    /// The configuration, as this launcher reads and writes it.
    pub settings: Option<Arc<dyn crate::settings::Settings>>,
    /// The provider accounts and models, likewise.
    pub providers: Option<Arc<dyn crate::providers::Providers>>,
    /// The plugins and marketplaces, likewise.
    pub plugins: Option<Arc<dyn crate::plugins::Plugins>>,
    /// The seed installation, likewise: `/setup` on a machine that has never run
    /// it has to put the seed skills on disk before the agent can be asked for
    /// one, and unpacking them is not this crate's to know how to do.
    pub setup: Option<Arc<dyn crate::setup::Setup>>,
}

/// The screen, mounted and connected, not yet running.
pub struct Mounted {
    pub app: App,
    pub ui: Arc<dyn UserInterface>,
}

/// Mount the screen and hand it `connection`.
pub async fn mount(
    screen: &Screen,
    extra: &[&str],
    connection: HostConnection,
) -> Result<Mounted, String> {
    mount_with(screen, extra, &[], Ports::default(), connection).await
}

/// [`mount`], with the launcher's own rows registered and its settings port.
///
/// `plugins` go in beside the screen's own (see [`catalog_with`]), which is what
/// lets a layer in `extra` name one of them: `[[insert]] name = "…"` resolves
/// against the registry, so a row that is not registered is a row the tree
/// refuses to mount, by name, at startup.
///
/// `ports` are the seams this launcher fills. An empty one is a launcher with
/// nothing to offer — a test, or a product with no configuration file — and a
/// panel whose port is missing draws an empty list, which is honest about what
/// it has rather than a claim that there is nothing to configure.
///
/// The launcher supplies the rows; the screen still mounts an empty UI and the
/// tree fills it. Nothing here reaches into `Modules` to `add_view`, which is
/// the difference between adding a panel and hard-coding one.
pub async fn mount_with(
    screen: &Screen,
    extra: &[&str],
    plugins: &[Arc<dyn Plugin>],
    ports: Ports,
    connection: HostConnection,
) -> Result<Mounted, String> {
    let mut app = App::new(catalog_with(plugins), tree(screen, extra)?);
    app.start().await.map_err(|e| e.to_string())?;
    let ctx = app.context();
    let _ = ctx
        .provide::<ConnectionSvc>(Arc::new(Connection::new(connection)))
        .map_err(|e| e.to_string())?;
    // Provided after the tree is up, like the connection: the rows that want it
    // look it up when they run, and a screen mounted without one simply has no
    // settings to show.
    if let Some(settings) = ports.settings {
        let _ = ctx
            .provide::<crate::plugin::SettingsSvc>(settings)
            .map_err(|e| e.to_string())?;
    }
    if let Some(providers) = ports.providers {
        let _ = ctx
            .provide::<crate::plugin::ProvidersSvc>(providers)
            .map_err(|e| e.to_string())?;
    }
    if let Some(plugins) = ports.plugins {
        let _ = ctx
            .provide::<crate::plugin::PluginsSvc>(plugins)
            .map_err(|e| e.to_string())?;
    }
    if let Some(setup) = ports.setup {
        let _ = ctx
            .provide::<crate::plugin::SetupSvc>(setup)
            .map_err(|e| e.to_string())?;
    }
    let ui = ctx
        .service::<UiSvc>()
        .ok_or("the screen's tree has no `ui` row")?;
    Ok(Mounted { app, ui })
}

/// Run the screen until the person leaves.
pub async fn run(
    screen: &Screen,
    connection: HostConnection,
    initial: Option<String>,
) -> Result<(), String> {
    run_with(screen, &[], &[], Ports::default(), connection, initial).await
}

/// [`run`], with the launcher's own rows, its settings port, and extra layers.
///
/// The one a product launcher calls: it is [`mount_with`] plus the loop, so a
/// launcher gets its rows mounted on the same path the tests and `--audit` take
/// rather than a second assembly written for the product.
pub async fn run_with(
    screen: &Screen,
    extra: &[&str],
    plugins: &[Arc<dyn Plugin>],
    ports: Ports,
    connection: HostConnection,
    initial: Option<String>,
) -> Result<(), String> {
    let mounted = mount_with(screen, extra, plugins, ports, connection).await?;
    let ctx = mounted.app.context();
    mounted.ui.run(&ctx, initial).await
}

/// One audit finding: whether it is a defect, and what it says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub defect: bool,
    pub text: String,
}

/// Whether the screen's composition is sound, with nothing connected: every row
/// declared what it does and did it. Only a defect fails; the rest are notes.
/// How the findings are marked is the caller's — this crate draws glyphs only
/// through a screen's capabilities.
pub async fn audit(screen: &Screen) -> Result<Vec<Finding>, String> {
    let mut app = App::new(catalog(), tree(screen, &[])?);
    app.start().await.map_err(|e| e.to_string())?;
    // Read from outside the tree by construction: the launcher runs `ui` and
    // hands over the connection, and tests read the modules and commands.
    let findings = app
        .audit_with(
            &["ui", "tui-modules", "tui-commands", "tui-agent-client"],
            &["agent-connection"],
        )
        .iter()
        .map(|f| Finding {
            defect: f.is_defect(),
            text: f.to_string(),
        })
        .collect();
    app.stop();
    Ok(findings)
}

/// One composed frame of the shipped screen over the conformance facts, as the
/// terminal would be sent it: what the screen looks like, with no tty, no model
/// and no keyboard. It goes through the real host, modules and encoder — a
/// mock-up that did not would be a picture of something that does not exist.
pub async fn demo(size: (u16, u16)) -> Result<String, String> {
    let screen = Screen {
        headless: Some(size),
        ..Screen::default()
    };
    let mut app = App::new(catalog(), tree(&screen, &[])?);
    app.start().await.map_err(|e| e.to_string())?;
    let ctx = app.context();
    let modules = ctx
        .service::<ModulesSvc>()
        .ok_or("the demo needs the module rows")?;
    let _surface = ctx
        .service::<SurfaceSvc>()
        .ok_or("the demo needs a surface row")?;
    let host = crate::host::Host::new(modules, crate::host::default_layout());
    for (seq, fact) in crate::conformance::facts().into_iter().enumerate() {
        host.absorb_logged(&atomcode_harness::session::LoggedEvent {
            seq: seq as u64 + 1,
            at: 0,
            event: fact,
        });
    }
    let caps = crate::caps::Caps::detect();
    {
        let mut moment = host.moment.write().expect("moment poisoned");
        moment.input = "再帮我看看 crates/ 的结构".into();
        moment.caret = moment.input.len();
        moment.caps = caps;
        moment.cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
    }
    let frame = host.compose(size);
    let violations = frame.containment_violations();
    app.stop();
    if !violations.is_empty() {
        return Err(violations
            .iter()
            .map(|v| format!("containment: {v}"))
            .collect::<Vec<_>>()
            .join("\n"));
    }
    Ok(format!(
        "{}\x1b[{};1H\n",
        crate::ansi::encode_with(&frame, caps),
        size.1 + 1
    ))
}
