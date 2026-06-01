//! Background OAuth polling for the QR-fast-path onboarding modal.
//!
//! On first-launch the wizard renders a QR for the AtomGit OAuth
//! short link and synchronously holds onto the `LoginSession`. This
//! module spawns a background thread that calls `LoginSession::
//! poll_once` every 2s — the moment AtomGit reports the user has
//! finished the in-browser consent, the task calls `finish()` to
//! exchange state → token (writing `auth.toml` as a side effect) and
//! pushes an [`OauthEvent::Authorized`] onto the event-loop channel.
//!
//! Why `std::thread::spawn` and not `tokio::spawn`:
//! [`LoginSession::poll_once`] / `finish` use a `reqwest::blocking::Client`.
//! Running them on a tokio worker would either block other tasks on
//! the same worker (no `spawn_blocking` indirection) or require a
//! reshape of the blocking client into async. A dedicated OS thread
//! sidesteps both — it sleeps between polls without touching the
//! runtime, and `tokio::sync::mpsc::Sender::blocking_send` lets it
//! push events back into the tokio world when it has something to
//! report.
//!
//! Cancellation: there isn't any. If the user hits Esc on the modal,
//! the modal closes but this thread continues polling until it
//! reaches a terminal state (Authorized / Err). `OauthEvent` arriving
//! on a closed-modal event loop is a silent no-op — the handler
//! checks `app.active_modal.is_some()` before acting. Worst case the
//! thread quietly writes a fresh `auth.toml` on its own — which is
//! the same effect as the user later running `/codingplan` after Esc,
//! so it's harmless.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use atomcode_core::auth::oauth::{LoginSession, PollOutcome};
use atomcode_telemetry::Telemetry;

/// Outcome the background poll thread emits at exactly one of: a
/// successful auth-token exchange, a fatal poll/finish error, or
/// (never) cancellation — the thread doesn't observe cancel signals.
#[derive(Debug)]
pub enum OauthEvent {
    /// The user finished AtomGit consent and `finish()` successfully
    /// wrote `auth.toml`. Event loop closes the modal + flips
    /// `pending_run_codingplan` so the existing `/codingplan` driver
    /// picks up the freshly-saved auth and claims the plan.
    Authorized,
    /// Either `poll_once` or `finish` returned an error. Carries the
    /// `Display`-formatted reason for the user — event loop closes
    /// the modal and surfaces this in scrollback so the user knows
    /// whether to retry (`/codingplan`), check the network, or check
    /// their system clock (for sign-stale errors).
    Failed(String),
}

/// Spawn the background poll thread. Returns immediately; the thread
/// owns the [`LoginSession`] and lives until it emits exactly one
/// [`OauthEvent`].
///
/// `wake_tx` is pulsed AFTER the event is queued so the event-loop
/// `tokio::select!` arm that reads `oauth_event_rx` actually fires —
/// `oauth_event_rx.recv()` alone would only fire on the next
/// scheduling tick, which on an idle TUI can be many seconds.
pub fn spawn_oauth_poll(
    session: LoginSession,
    tel: Option<Arc<Telemetry>>,
    event_tx: mpsc::UnboundedSender<OauthEvent>,
    wake_tx: mpsc::Sender<()>,
) {
    std::thread::spawn(move || {
        // Loop polls `&session` without moving it; on Authorized we
        // break out and `finish(session)` consumes it. Doing the
        // poll vs. consume split this way means `session` stays
        // intact across iterations.
        let poll_outcome: Result<(), String> = loop {
            match session.poll_once() {
                Ok(PollOutcome::Authorized) => break Ok(()),
                Ok(PollOutcome::Pending) => {
                    std::thread::sleep(Duration::from_secs(2));
                }
                Err(e) => break Err(format!("{e:#}")),
            }
        };

        let event = match poll_outcome {
            Ok(()) => {
                // finish() consumes session → exchanges state for
                // token → returns AuthInfo. NOTE: finish does NOT
                // write auth.toml — the caller is responsible. The
                // existing CLI `login()` driver pairs finish + save;
                // we have to mirror it here or downstream
                // `is_logged_in()` returns false and the subsequent
                // /codingplan flow re-runs login, popping a second
                // QR + asking the user to scan AGAIN.
                match session.finish(tel.as_ref()) {
                    Ok(auth_info) => {
                        match atomcode_core::auth::save_auth(&auth_info) {
                            Ok(()) => OauthEvent::Authorized,
                            Err(e) => OauthEvent::Failed(format!(
                                "auth.toml write failed: {e:#}"
                            )),
                        }
                    }
                    Err(e) => OauthEvent::Failed(format!("{e:#}")),
                }
            }
            Err(reason) => OauthEvent::Failed(reason),
        };

        // Queue the event BEFORE the wake pulse. Wake without an
        // event in the channel would make `oauth_event_rx.recv()`
        // hang and the wake handler look broken — order matters.
        let _ = event_tx.send(event);
        // `blocking_send` on a `Sender<()>` from a std thread is the
        // documented bridge between std-thread producers and tokio
        // consumers. wake_rx is bounded at 1 so multiple wakes
        // coalesce, but we only emit one here anyway.
        let _ = wake_tx.blocking_send(());
    });
}
