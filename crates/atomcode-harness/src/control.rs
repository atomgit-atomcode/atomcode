//! Runtime reconfiguration: the host side of the `control` seam.
//!
//! The runtime could always replace a row while running — that is what
//! [`App::patch`](atomcode_plexus::App::patch) does, and what the "swap a
//! provider under a running consumer" tests assert. What was missing was a way
//! to ask for it from inside: a front end is handed a `Context`, and the `App`
//! lives in the launcher.
//!
//! Putting the capability in the tree closes that without coupling anything: a
//! REPL command, an HTTP endpoint and a JSON-RPC method all reach the same
//! service, and a front end that does not want it simply never resolves it.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{App, Layer};
use tokio::sync::Mutex;

use crate::seams::Control;

/// The shipped implementation: it owns the `App`.
pub struct AppControl {
    app: Arc<Mutex<App>>,
}

impl AppControl {
    pub fn new(app: Arc<Mutex<App>>) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Control for AppControl {
    async fn patch(&self, toml: &str) -> Result<String, String> {
        let layer = Layer::from_toml(toml).map_err(|e| e.to_string())?;

        // A patch remounts fibers, so it must not run inside a plugin's apply.
        // Refusing beats deadlocking: the caller gets a message, not a hang.
        let mut app = self
            .app
            .try_lock()
            .map_err(|_| "a reconfiguration is already in progress".to_string())?;

        let before: Vec<String> = app.tree().entries.iter().map(row_label).collect();
        app.patch(&layer).await.map_err(|e| e.to_string())?;
        let after: Vec<String> = app.tree().entries.iter().map(row_label).collect();

        let changed: Vec<&String> = after.iter().filter(|r| !before.contains(r)).collect();
        let gone: Vec<&String> = before.iter().filter(|r| !after.contains(r)).collect();
        if changed.is_empty() && gone.is_empty() {
            return Ok("applied; no row changed".into());
        }
        let mut out = String::from("applied");
        if !gone.is_empty() {
            out.push_str(&format!(
                "\n  was: {}",
                gone.iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !changed.is_empty() {
            out.push_str(&format!(
                "\n  now: {}",
                changed
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Ok(out)
    }

    async fn dump(&self) -> String {
        match self.app.try_lock() {
            Ok(app) => app.dump_runtime(),
            Err(_) => "a reconfiguration is in progress".into(),
        }
    }

    async fn audit(&self) -> Vec<String> {
        match self.app.try_lock() {
            Ok(app) => app
                .audit_with(
                    crate::seam_map::HOST_CONSUMED,
                    crate::seam_map::HOST_PROVIDED,
                )
                .iter()
                .map(ToString::to_string)
                .collect(),
            Err(_) => vec!["a reconfiguration is in progress".into()],
        }
    }

    async fn rows(&self) -> Vec<(String, String, bool)> {
        match self.app.try_lock() {
            Ok(app) => app
                .tree()
                .entries
                .iter()
                .map(|e| (e.id.clone(), e.name.clone(), !e.disabled))
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// The shape both sides of a diff are compared in.
///
/// It includes the config, because "which plugin" is only half of what a row
/// is: changing an approval mode or a round budget changes the running system
/// as surely as swapping the plugin, and a diff that reported "no row changed"
/// for it would be lying.
fn row_label(entry: &atomcode_plexus::Entry) -> String {
    let config = match &entry.config {
        serde_json::Value::Null => String::new(),
        value if value.as_object().is_some_and(|o| o.is_empty()) => String::new(),
        value => format!(" {value}"),
    };
    format!(
        "{} <- {}{}{}",
        entry.id,
        entry.name,
        if entry.disabled { " (disabled)" } else { "" },
        config
    )
}
