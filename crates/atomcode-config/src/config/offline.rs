//! `offline_mode`: a single process-level verdict, seeded once at startup from the
//! `offline_mode` config value + `ATOMCODE_OFFLINE` env, read everywhere via
//! `is_offline_active()`. `auto` starts optimistic-online and a lazy network-failure
//! hook (`mark_network_unreachable`) flips it offline. Default is `Off` — the online
//! build never enters an offline branch.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OfflineMode {
    Auto,
    On,
    #[default]
    Off,
}

// Verdict states: 0=un-seeded(online), 1=forced online, 2=forced offline,
// 3=auto optimistic-online, 4=auto detected-offline.
const V_UNSEEDED: u8 = 0;
const V_ONLINE: u8 = 1;
const V_OFFLINE: u8 = 2;
const V_AUTO_ONLINE: u8 = 3;
const V_AUTO_OFFLINE: u8 = 4;

static VERDICT: AtomicU8 = AtomicU8::new(V_UNSEEDED);
static NOTE: Mutex<Option<String>> = Mutex::new(None);

pub fn offline_from_env(raw: Option<&str>) -> Option<OfflineMode> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" => Some(OfflineMode::On),
        "0" | "false" | "off" => Some(OfflineMode::Off),
        "auto" => Some(OfflineMode::Auto),
        _ => None,
    }
}

pub fn seed_offline_verdict(mode: OfflineMode, env_raw: Option<&str>) {
    let effective = offline_from_env(env_raw).unwrap_or(mode);
    let v = match effective {
        OfflineMode::On => V_OFFLINE,
        OfflineMode::Off => V_ONLINE,
        OfflineMode::Auto => V_AUTO_ONLINE,
    };
    VERDICT.store(v, Ordering::Relaxed);
}

pub fn mark_network_unreachable() {
    // Only the optimistic-auto state is allowed to flip; forced states are sticky.
    let _ = VERDICT.compare_exchange(
        V_AUTO_ONLINE,
        V_AUTO_OFFLINE,
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
}

pub fn is_offline_active() -> bool {
    matches!(VERDICT.load(Ordering::Relaxed), V_OFFLINE | V_AUTO_OFFLINE)
}

/// Store the environment-level mirror/registry note (blank/whitespace → cleared).
pub fn set_offline_note(note: Option<String>) {
    let cleaned = note
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    *NOTE.lock().unwrap() = cleaned;
}

pub fn offline_note() -> Option<String> {
    NOTE.lock().unwrap().clone()
}

pub fn reset_offline_verdict_for_test() {
    VERDICT.store(V_UNSEEDED, Ordering::Relaxed);
    *NOTE.lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Global mutex to serialize tests that share process-level state.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn fresh() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_LOCK.lock().unwrap();
        reset_offline_verdict_for_test();
        guard
    }

    #[test]
    fn env_parse() {
        // No global state involved; no lock needed.
        assert_eq!(offline_from_env(Some("on")), Some(OfflineMode::On));
        assert_eq!(offline_from_env(Some("1")), Some(OfflineMode::On));
        assert_eq!(offline_from_env(Some("off")), Some(OfflineMode::Off));
        assert_eq!(offline_from_env(Some("auto")), Some(OfflineMode::Auto));
        assert_eq!(offline_from_env(Some("garbage")), None);
        assert_eq!(offline_from_env(None), None);
    }

    #[test]
    fn default_off_is_online() {
        let _g = fresh();
        seed_offline_verdict(OfflineMode::Off, None);
        assert!(!is_offline_active());
    }

    #[test]
    fn on_is_offline() {
        let _g = fresh();
        seed_offline_verdict(OfflineMode::On, None);
        assert!(is_offline_active());
    }

    #[test]
    fn env_wins_over_mode() {
        let _g = fresh();
        seed_offline_verdict(OfflineMode::Off, Some("on"));
        assert!(is_offline_active());
        reset_offline_verdict_for_test();
        seed_offline_verdict(OfflineMode::On, Some("off"));
        assert!(!is_offline_active());
    }

    #[test]
    fn auto_optimistic_until_marked() {
        let _g = fresh();
        seed_offline_verdict(OfflineMode::Auto, None);
        assert!(!is_offline_active(), "auto starts optimistic-online");
        mark_network_unreachable();
        assert!(is_offline_active(), "auto flips offline after a network failure");
    }

    #[test]
    fn mark_is_noop_for_forced_online() {
        let _g = fresh();
        seed_offline_verdict(OfflineMode::Off, None);
        mark_network_unreachable();
        assert!(!is_offline_active(), "explicit off is never flipped by detection");
    }

    #[test]
    fn note_roundtrip() {
        let _g = fresh();
        assert_eq!(offline_note(), None);
        set_offline_note(Some("npm via nexus.internal".to_string()));
        assert_eq!(offline_note().as_deref(), Some("npm via nexus.internal"));
        set_offline_note(Some("   ".to_string()));
        assert_eq!(offline_note(), None, "blank note reads as None");
        reset_offline_verdict_for_test();
        assert_eq!(offline_note(), None, "reset clears note");
    }
}
