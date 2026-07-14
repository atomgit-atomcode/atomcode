//! Stable driver control plane for a coding runtime.
//!
//! The bridge is still the lifecycle owner during the incremental migration, but
//! drivers must not cache its current kernel `AgentHandle`: provider/session
//! changes replace that handle. This channel targets the long-lived owner, which
//! forwards each request to whichever kernel agent is current at that moment.

use std::error::Error;
use std::fmt;

use atomcode_kernel::event::AgentCommand;
use tokio::sync::mpsc;

/// Cloneable, stable control handle held by a driver.
#[derive(Clone, Debug)]
pub struct CodingRuntimeHandle {
    tx: mpsc::UnboundedSender<CodingRuntimeControl>,
}

impl CodingRuntimeHandle {
    /// Request manual conversation compaction from the current kernel agent.
    pub fn compact(&self, focus: Option<String>) -> Result<(), RuntimeUnavailable> {
        self.send(AgentCommand::Compact { focus })
    }

    fn send(&self, command: AgentCommand) -> Result<(), RuntimeUnavailable> {
        self.tx
            .send(CodingRuntimeControl::Kernel(command))
            .map_err(|_| RuntimeUnavailable)
    }
}

/// The runtime owner side of [`CodingRuntimeHandle`].
///
/// This type intentionally hides the Tokio receiver so ownership stays singular.
#[derive(Debug)]
pub struct CodingRuntimeControlReceiver {
    rx: mpsc::UnboundedReceiver<CodingRuntimeControl>,
}

impl CodingRuntimeControlReceiver {
    pub async fn recv(&mut self) -> Option<CodingRuntimeControl> {
        self.rx.recv().await
    }
}

/// Internal control envelope consumed by the current runtime owner.
///
/// It is public only because the temporary owner lives in `atomcode-bridge`, a
/// separate crate. Drivers should use capability methods on [`CodingRuntimeHandle`].
#[doc(hidden)]
#[derive(Debug)]
pub enum CodingRuntimeControl {
    Kernel(AgentCommand),
}

/// Build the two ends of the stable runtime control channel.
#[doc(hidden)]
pub fn coding_runtime_control_channel() -> (CodingRuntimeHandle, CodingRuntimeControlReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        CodingRuntimeHandle { tx },
        CodingRuntimeControlReceiver { rx },
    )
}

/// The runtime owner has stopped and can no longer accept controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeUnavailable;

impl fmt::Display for RuntimeUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("coding runtime is unavailable")
    }
}

impl Error for RuntimeUnavailable {}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compact_emits_kernel_command() {
        let (handle, mut controls) = coding_runtime_control_channel();

        handle.compact(Some("recent tool output".into())).unwrap();

        assert!(matches!(
            controls.recv().await,
            Some(CodingRuntimeControl::Kernel(AgentCommand::Compact {
                focus: Some(focus)
            })) if focus == "recent tool output"
        ));
    }

    #[test]
    fn closed_runtime_returns_typed_error() {
        let (handle, controls) = coding_runtime_control_channel();
        drop(controls);

        assert_eq!(handle.compact(None), Err(RuntimeUnavailable));
    }
}
