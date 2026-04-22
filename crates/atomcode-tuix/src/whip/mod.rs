// crates/atomcode-tuix/src/whip/mod.rs
//
// The "whip" feature — Ctrl+G / `/whip` nudge. Split into its own module
// so the tuix event loop doesn't grow another ad-hoc feature dir:
//   - phrases.rs : phrase pool + RNG-backed picker
//   - anim.rs    : frame generator (added in Task 4)
//   - mod.rs     : Cooldown + fire_whip orchestration (fire_whip in Task 7)

pub mod anim;
pub mod art;
pub mod phrases;

use std::time::{Duration, Instant};

use anyhow::Result;
use atomcode_core::agent::AgentCommand;

use crate::event_loop::LoopCtx;
use crate::modals::{Modal, WhipOverlay};
use crate::render::{Renderer, UiLine};
use crate::state::{UiPhase, UiState};

/// Monotonic rate-limit gate shared by Ctrl+G and `/whip`. `last` is
/// stored on `LoopCtx` (not inside this struct) so a single source of
/// truth lives with the event loop; this struct is a stateless helper.
pub struct Cooldown;

impl Cooldown {
    /// Returns true if a whip may fire right now (the stored `last` is
    /// either None or older than `window`). Callers update `last` on
    /// their own after a successful fire.
    pub fn try_fire(last: Option<Instant>, now: Instant, window: Duration) -> bool {
        match last {
            None => true,
            Some(t) => now.duration_since(t) >= window,
        }
    }
}

/// Fire a whip: print a scrollback marker, play the animation, and (if
/// a turn is running) queue the encouragement phrase via
/// `AgentCommand::AppendInput`. Idempotent under gates (disabled
/// config, cooldown, modal conflict, approval/suspended phases) —
/// returns `Ok(())` silently. Must be called from both the Ctrl+G
/// keyboard handler and the `/whip` slash command so their semantics
/// stay identical.
pub fn fire_whip(
    ctx: &mut LoopCtx,
    active_modal: &mut Option<Box<dyn Modal>>,
    state: &UiState,
    renderer: &mut dyn Renderer,
) -> Result<()> {
    if !ctx.config.whip.enabled {
        return Ok(());
    }
    // No whip during tool approval (agent is waiting on you, not slow)
    // or while suspended (stdin is handed off to a child process).
    if matches!(state.phase, UiPhase::Approval | UiPhase::Suspended) {
        return Ok(());
    }
    if active_modal.is_some() {
        return Ok(());
    }
    let now = Instant::now();
    let window = Duration::from_millis(ctx.config.whip.cooldown_ms);
    if !Cooldown::try_fire(ctx.last_whip_at, now, window) {
        return Ok(());
    }

    let phrase = phrases::pick_phrase(&ctx.config.whip.phrases);
    ctx.last_whip_at = Some(now);

    // Scrollback record — multi-row ASCII whip art pushed into history
    // each time whip fires. The footer overlay is the live flourish;
    // this is the permanent visual trace that survives scroll-back.
    let suffix = if matches!(state.phase, UiPhase::Streaming) {
        state
            .turn_elapsed()
            .map(|d| format!(" (after {:.1}s)", d.as_secs_f32()))
            .unwrap_or_default()
    } else {
        "  (no turn running)".to_string()
    };
    let (cols, _) = crossterm::terminal::size().unwrap_or((80, 24));
    for row in art::crack_art(&phrase, &suffix, cols) {
        renderer.render(UiLine::CommandOutput(format!("{}\n", row)));
    }
    renderer.flush();

    // Inject into the LLM context only when a turn is actually running.
    if matches!(state.phase, UiPhase::Streaming) {
        ctx.agent
            .cmd_tx
            .send(AgentCommand::AppendInput(phrase.clone()))
            .ok();
    }

    // Install the animation overlay in all eligible phases. The event
    // loop's 33ms tick advances it via `WhipOverlay::advance`.
    *active_modal = Some(Box::new(WhipOverlay::open(phrase)));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_fire_is_always_allowed() {
        let now = Instant::now();
        assert!(Cooldown::try_fire(None, now, Duration::from_millis(1000)));
    }

    #[test]
    fn fire_within_window_is_blocked() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(500);
        assert!(!Cooldown::try_fire(Some(t0), t1, Duration::from_millis(1000)));
    }

    #[test]
    fn fire_exactly_at_window_is_allowed() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(1000);
        assert!(Cooldown::try_fire(Some(t0), t1, Duration::from_millis(1000)));
    }

    #[test]
    fn fire_after_window_is_allowed() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(1001);
        assert!(Cooldown::try_fire(Some(t0), t1, Duration::from_millis(1000)));
    }
}

#[cfg(test)]
mod fire_whip_tests {
    //! Black-box-ish tests for the `fire_whip` orchestrator. Uses the
    //! `#[cfg(test)]` `LoopCtx::for_tests` helper in `event_loop/mod.rs`
    //! so we can construct a real ctx with dangling channels and inspect
    //! the AgentCommand stream.

    use super::*;
    use atomcode_core::config::Config;
    use crate::event_loop::LoopCtx;
    use crate::modals::Modal;
    use crate::render::plain::PlainRenderer;
    use crate::state::UiState;

    fn mk_config() -> Config {
        // Minimal viable Config — matches the pattern used in
        // `atomcode-core::turn::tests::make_test_config`.
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "test".to_string(),
            atomcode_core::config::provider::ProviderConfig {
                provider_type: "openai".to_string(),
                api_key: Some("sk-test".to_string()),
                model: "m".to_string(),
                base_url: None,
                system_prompt: None,
                user_agent: None,
                context_window: 16000,
                max_tokens: None,
                ephemeral: false,
            },
        );
        Config {
            default_provider: "test".to_string(),
            default_workdir: None,
            providers,
            datalog: Default::default(),
            auto_update: false,
            whip: Default::default(),
        }
    }

    #[test]
    fn during_streaming_sends_append_input() {
        let (mut ctx, mut cmd_rx) = LoopCtx::for_tests(mk_config());
        let mut state = UiState::new();
        state.on_submit(); // phase = Streaming
        let mut modal: Option<Box<dyn Modal>> = None;
        let mut r = PlainRenderer::new();

        fire_whip(&mut ctx, &mut modal, &state, &mut r).unwrap();

        let mut found = false;
        while let Ok(c) = cmd_rx.try_recv() {
            if matches!(c, atomcode_core::agent::AgentCommand::AppendInput(_)) {
                found = true;
            }
        }
        assert!(found, "AppendInput must be sent while streaming");
        assert!(modal.is_some(), "overlay must be installed");
    }

    #[test]
    fn during_idle_does_not_send_append_input() {
        let (mut ctx, mut cmd_rx) = LoopCtx::for_tests(mk_config());
        let state = UiState::new(); // Idle
        let mut modal: Option<Box<dyn Modal>> = None;
        let mut r = PlainRenderer::new();

        fire_whip(&mut ctx, &mut modal, &state, &mut r).unwrap();

        while let Ok(c) = cmd_rx.try_recv() {
            assert!(
                !matches!(c, atomcode_core::agent::AgentCommand::AppendInput(_)),
                "no AppendInput at idle"
            );
        }
        assert!(modal.is_some(), "overlay still shown at idle");
    }

    #[test]
    fn during_approval_is_a_noop() {
        let (mut ctx, mut cmd_rx) = LoopCtx::for_tests(mk_config());
        let mut state = UiState::new();
        state.on_submit();
        state.on_approval_needed("bash");
        let mut modal: Option<Box<dyn Modal>> = None;
        let mut r = PlainRenderer::new();

        fire_whip(&mut ctx, &mut modal, &state, &mut r).unwrap();
        assert!(modal.is_none(), "no overlay during approval");
        assert!(cmd_rx.try_recv().is_err(), "no commands sent during approval");
    }

    #[test]
    fn cooldown_blocks_second_fire() {
        let mut cfg = mk_config();
        cfg.whip.cooldown_ms = 1000;
        let (mut ctx, _rx) = LoopCtx::for_tests(cfg);
        let mut state = UiState::new();
        state.on_submit();
        let mut modal: Option<Box<dyn Modal>> = None;
        let mut r = PlainRenderer::new();

        fire_whip(&mut ctx, &mut modal, &state, &mut r).unwrap();
        modal = None; // simulate overlay having closed
        fire_whip(&mut ctx, &mut modal, &state, &mut r).unwrap();
        assert!(modal.is_none(), "second fire within cooldown must be silent");
    }

    #[test]
    fn disabled_config_suppresses_everything() {
        let mut cfg = mk_config();
        cfg.whip.enabled = false;
        let (mut ctx, mut cmd_rx) = LoopCtx::for_tests(cfg);
        let mut state = UiState::new();
        state.on_submit();
        let mut modal: Option<Box<dyn Modal>> = None;
        let mut r = PlainRenderer::new();

        fire_whip(&mut ctx, &mut modal, &state, &mut r).unwrap();
        assert!(modal.is_none());
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn modal_busy_blocks_whip() {
        use crate::modals::SessionPicker;
        let (mut ctx, mut cmd_rx) = LoopCtx::for_tests(mk_config());
        let mut state = UiState::new();
        state.on_submit();
        // Install a dummy modal to simulate an open picker.
        let mut modal: Option<Box<dyn Modal>> =
            Some(Box::new(SessionPicker::open(Vec::new())));
        let mut r = PlainRenderer::new();

        fire_whip(&mut ctx, &mut modal, &state, &mut r).unwrap();
        // Existing modal should NOT have been replaced.
        assert!(cmd_rx.try_recv().is_err(), "no commands sent when modal busy");
    }
}
