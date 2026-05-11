//! Cross-batch tool-call loop guard.
//!
//! The runner already deduplicates *within a single LLM batch*
//! (`is_dup` in `runner.rs`). This module covers the orthogonal case:
//! a model emitting the same `(tool, arguments)` pair across many
//! sequential batches with no progress between them — the symptom
//! that produced the 22-identical-`Bash(cargo check)` screenshot.
//!
//! Trigger conditions (all required, to avoid the false positives that
//! killed commit `9339cf1`'s removed `detect_call_loop`):
//!
//! * `(name, arguments)` exactly matches a prior call's pair —
//!   normalised once via `\0` separator. No fuzzy / token-streak
//!   detection (that was the bit that wrongly caught batches like ssh
//!   `ls`/`cat`/`sshpass`).
//! * Every prior matching call had identical output and identical
//!   `success` — polling a flaky endpoint where the body changes is
//!   real progress, not a loop.
//! * No state-changing tool succeeded with a *new* key in between —
//!   when an `edit_file` to a previously-untouched file lands, the
//!   world has changed and any prior repeats no longer count. (A
//!   state-changing call to a key that is *already* in the window
//!   does NOT clear it: that means `edit_file` itself is being
//!   spammed with identical args, which is a loop we want to catch.)
//!
//! The trigger threshold is intentionally low (third attempt blocks)
//! so the model gets two "maybe this time" chances on transient
//! flakes but doesn't burn a full turn budget repeating itself. The
//! agent clears the window at every user-message boundary, so the
//! per-turn-only semantics are guaranteed by the caller.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Strict trigger: identical `(name, args, output, success)` repeats.
/// Catches deterministic loops where output is byte-stable. With
/// THRESHOLD = 3, the first two attempts execute normally and the
/// third is short-circuited.
const THRESHOLD: usize = 3;

/// Hard cap regardless of output drift. Catches loops where the model
/// is clearly stuck on the same `(name, args)` but output varies
/// slightly between runs — e.g., `cargo check 2>&1 | tail -20` whose
/// warning order, paths, or timing differ run-to-run. Without this
/// gate the strict trigger above never fires for cargo/test/lint
/// polling and the model can spam 30+ identical commands before
/// hitting the per-turn step budget. Threshold 6 leaves room for
/// legitimate progress-polling (curl health-check waiting for a
/// service to start) before blocking.
const HARD_THRESHOLD: usize = 6;

/// Maximum number of recent entries to keep. The window is per-turn
/// (cleared at every user-message boundary by the agent), so a small
/// bounded ring is enough — even a chatty model rarely emits more
/// than a dozen tools per assistant turn. Sized above HARD_THRESHOLD
/// so the soft-trigger count survives the ring eviction.
const WINDOW: usize = 32;

/// Tools whose successful execution counts as "progress" — when one
/// fires with a previously-unseen key, the loop guard resets so any
/// prior read-only repeats no longer count toward the threshold.
const STATE_CHANGING: &[&str] =
    &["edit_file", "write_file", "create_file", "search_replace"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    /// `name + "\0" + arguments` — the raw `(name, args)` pair pre-joined
    /// so equality checks are a single string compare instead of a
    /// tuple compare on every lookup.
    key: String,
    output_hash: u64,
    success: bool,
}

#[derive(Default, Debug)]
pub struct LoopGuardState {
    recent: Vec<Entry>,
}

/// What the runner should do with the call it's about to dispatch.
#[derive(Debug, PartialEq, Eq)]
pub enum LoopGuardDecision {
    /// Run the tool normally. After execution, call `record(...)`.
    Allow,
    /// Skip execution. The string carries a synthetic `ToolResult`
    /// body explaining to the model why this attempt was blocked.
    Block(String),
}

impl LoopGuardState {
    pub fn clear(&mut self) {
        self.recent.clear();
    }

    /// Decide whether to execute or block the call. Read-only — the
    /// caller still needs `record(...)` after a real execution.
    pub fn check(&self, name: &str, arguments: &str) -> LoopGuardDecision {
        let key = make_key(name, arguments);
        let matches: Vec<&Entry> = self.recent.iter().filter(|e| e.key == key).collect();

        // Hard cap first: HARD_THRESHOLD identical-key repeats regardless
        // of output drift. The strict trigger below would never fire for
        // cargo / test / lint polling because their output is timing- or
        // cache-dependent.
        if matches.len() >= HARD_THRESHOLD - 1 {
            return LoopGuardDecision::Block(format!(
                "[Loop guard] `{}` with identical arguments has run {} time(s) \
                 already in this turn. Output varies slightly each run but you \
                 keep issuing the same query — that's a loop. Try a different \
                 approach: change the command (different filter / different \
                 file), edit code to make progress, or stop and ask the user. \
                 Continuing to repeat will keep getting blocked.",
                name,
                matches.len()
            ));
        }

        // Strict trigger: THRESHOLD identical-output, identical-success
        // repeats. Catches deterministic loops where the model keeps
        // hitting the exact same wall (same compile error, same grep
        // result, etc).
        if matches.len() < THRESHOLD - 1 {
            return LoopGuardDecision::Allow;
        }
        let first = matches[0];
        let all_same =
            matches.iter().all(|e| e.output_hash == first.output_hash && e.success == first.success);
        if !all_same {
            return LoopGuardDecision::Allow;
        }
        LoopGuardDecision::Block(format!(
            "[Loop guard] This exact tool call (`{}` with identical arguments) has \
             run {} time(s) already in this turn with the same output and no \
             intervening state change. Try a different approach: change the \
             command, read different files, edit code to make progress, or stop \
             and ask the user. Repeating the same query will not change the \
             result.",
            name,
            matches.len()
        ))
    }

    /// Record a real execution result. Call exactly once per non-blocked
    /// dispatch. Handles the state-change reset internally so the runner
    /// doesn't have to know the tool taxonomy.
    pub fn record(&mut self, name: &str, arguments: &str, output: &str, success: bool) {
        let key = make_key(name, arguments);
        let is_state_changing = STATE_CHANGING.contains(&name);
        let key_is_new = !self.recent.iter().any(|e| e.key == key);
        if is_state_changing && success && key_is_new {
            // Genuine progress: a previously-untouched edit/write/create
            // landed. Wipe prior repeats — they're stale relative to the
            // new world state. The just-executed call is still recorded
            // below so we can detect spam-of-the-same-edit.
            self.recent.clear();
        }
        self.recent.push(Entry {
            key,
            output_hash: hash_str(output),
            success,
        });
        if self.recent.len() > WINDOW {
            // Drop oldest. `remove(0)` on a Vec<Entry> of bounded size
            // is fine — WINDOW is tiny and this runs at most once per
            // tool dispatch.
            self.recent.remove(0);
        }
    }
}

fn make_key(name: &str, arguments: &str) -> String {
    // Canonicalise the JSON arguments so cosmetically-different repeats
    // ({"a":1, "b":2} vs {"b":2,"a":1} vs {"a": 1, "b": 2}) collapse to the
    // same key. Mirrors the in-batch `is_dup` normalisation in `runner.rs`
    // so cross-batch and in-batch dedup share semantics. Non-JSON args
    // pass through unchanged.
    let normalised = match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(v) => serde_json::to_string(&v).unwrap_or_else(|_| arguments.to_string()),
        Err(_) => arguments.to_string(),
    };
    let mut s = String::with_capacity(name.len() + 1 + normalised.len());
    s.push_str(name);
    s.push('\0');
    s.push_str(&normalised);
    s
}

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(d: &LoopGuardDecision) -> bool {
        matches!(d, LoopGuardDecision::Allow)
    }
    fn block(d: &LoopGuardDecision) -> bool {
        matches!(d, LoopGuardDecision::Block(_))
    }

    #[test]
    fn third_identical_call_is_blocked() {
        let mut g = LoopGuardState::default();
        let args = r#"{"command":"cargo check"}"#;
        let out = "error[E0599]: no method named `foo`";

        assert!(allow(&g.check("bash", args)));
        g.record("bash", args, out, false);

        assert!(allow(&g.check("bash", args)));
        g.record("bash", args, out, false);

        // Third call with identical (name, args, output, success) → blocked.
        let d = g.check("bash", args);
        assert!(block(&d), "expected Block, got {:?}", d);
    }

    #[test]
    fn rotating_arguments_do_not_trigger() {
        // Pins the false-positive case from the deleted `detect_call_loop`
        // (commit 9339cf1): a model running `cargo check 2>&1 | head` with
        // different `head -N` filters each time looks superficially
        // similar but has different args. Must not trip the guard.
        let mut g = LoopGuardState::default();
        let cmds = [
            r#"{"command":"cargo check 2>&1 | head -50"}"#,
            r#"{"command":"cargo check 2>&1 | head -100"}"#,
            r#"{"command":"cargo check 2>&1 | head -200"}"#,
            r#"{"command":"cargo check --message-format=short"}"#,
        ];
        for c in cmds {
            assert!(allow(&g.check("bash", c)));
            g.record("bash", c, "compile error", false);
        }
        // Even the 5th distinct command is allowed.
        assert!(allow(&g.check("bash", r#"{"command":"cargo build"}"#)));
    }

    #[test]
    fn ssh_batch_with_distinct_commands_does_not_trigger() {
        // The other 9339cf1 false positive: a five-tool ssh-into-box
        // sequence (sshpass / ls / cat / cp / chmod). All bash, all
        // distinct args → must not trigger. The (name, args) discrimination
        // alone handles this; included here as a regression pin.
        let mut g = LoopGuardState::default();
        let cmds = [
            r#"{"command":"sshpass -p ... ssh user@host echo ok"}"#,
            r#"{"command":"sshpass -p ... ssh user@host ls /var/log"}"#,
            r#"{"command":"sshpass -p ... ssh user@host cat /etc/hostname"}"#,
            r#"{"command":"sshpass -p ... ssh user@host uname -a"}"#,
            r#"{"command":"sshpass -p ... ssh user@host df -h"}"#,
        ];
        for c in cmds {
            let d = g.check("bash", c);
            assert!(allow(&d), "expected Allow for {:?}, got {:?}", c, d);
            g.record("bash", c, "ok", true);
        }
    }

    #[test]
    fn changing_output_does_not_trigger() {
        // Polling a flaky endpoint where the body genuinely changes
        // each call → real signal, must not block.
        let mut g = LoopGuardState::default();
        let args = r#"{"command":"curl -s http://service/health"}"#;

        g.record("bash", args, "starting up", false);
        g.record("bash", args, "starting up", false);
        // Third call: output would have changed → not the loop pattern.
        // We simulate by recording a different prior output for #2,
        // then asserting #3 is allowed.
        let mut g2 = LoopGuardState::default();
        g2.record("bash", args, "starting up", false);
        g2.record("bash", args, "ready", true);
        assert!(allow(&g2.check("bash", args)));
    }

    #[test]
    fn intervening_state_change_resets_window() {
        // The TDD scenario: bash(cargo test) → fails → edit_file →
        // bash(cargo test) again → fails (same output) → bash(cargo test) again.
        // Without the reset, the third bash would block. With it, the
        // edit clears the window, so the post-edit bash calls start
        // counting fresh.
        let mut g = LoopGuardState::default();
        let bash_args = r#"{"command":"cargo test"}"#;
        let edit_args = r#"{"file_path":"src/lib.rs","old":"a","new":"b"}"#;
        let test_out = "test failed: assertion 1 != 2";

        g.record("bash", bash_args, test_out, false);
        g.record("bash", bash_args, test_out, false);
        // Now an edit lands → progress.
        g.record("edit_file", edit_args, "ok", true);
        // The bash calls from before the edit should no longer count.
        assert!(allow(&g.check("bash", bash_args)));
        g.record("bash", bash_args, test_out, false);
        assert!(allow(&g.check("bash", bash_args)));
        g.record("bash", bash_args, test_out, false);
        // Now we've hit the threshold *post-edit* → block.
        assert!(block(&g.check("bash", bash_args)));
    }

    #[test]
    fn state_change_with_repeated_key_does_not_reset() {
        // A model spamming `edit_file` to the same path with the same
        // diff is itself a loop — the state-change reset rule only
        // fires on a *new* key. After three identical edit attempts
        // the third must block.
        let mut g = LoopGuardState::default();
        let edit_args = r#"{"file_path":"a.rs","old":"x","new":"y"}"#;

        g.record("edit_file", edit_args, "ok", true);
        g.record("edit_file", edit_args, "ok", true);
        // Third identical edit is still a loop, not progress.
        assert!(block(&g.check("edit_file", edit_args)));
    }

    #[test]
    fn clear_resets_state() {
        // Per-user-message boundary: agent calls clear() and the next
        // call starts fresh.
        let mut g = LoopGuardState::default();
        let args = r#"{"command":"ls"}"#;
        g.record("bash", args, "out", true);
        g.record("bash", args, "out", true);
        assert!(block(&g.check("bash", args)));
        g.clear();
        assert!(allow(&g.check("bash", args)));
    }

    #[test]
    fn hard_cap_blocks_loops_even_when_output_drifts() {
        // The cargo-check-30-times scenario: model keeps issuing the
        // same bash command, but each run produces slightly different
        // output (warning order / timing / cache state). The strict
        // THRESHOLD never fires because outputs differ; the hard cap
        // catches it at HARD_THRESHOLD-1 prior matches (5 records →
        // 6th is blocked).
        let mut g = LoopGuardState::default();
        let args = r#"{"command":"cargo check 2>&1 | tail -20"}"#;
        // Each record is the same call but with subtly different output.
        for i in 0..5 {
            assert!(
                allow(&g.check("bash", args)),
                "call {} should still allow",
                i + 1
            );
            g.record("bash", args, &format!("warning #{}: drift", i), false);
        }
        // 6th attempt: HARD_THRESHOLD reached, block regardless of drift.
        let d = g.check("bash", args);
        assert!(block(&d), "expected hard-cap Block on 6th call, got {:?}", d);
        if let LoopGuardDecision::Block(msg) = d {
            assert!(
                msg.contains("Output varies slightly"),
                "hard-cap message expected, got: {}",
                msg
            );
        }
    }

    #[test]
    fn polling_below_hard_cap_still_allowed() {
        // Legitimate progress polling (5 attempts of curl health-check)
        // must still be allowed under the hard cap. Only at attempt 6+
        // do we intervene. Five attempts is enough budget for typical
        // service-startup polling.
        let mut g = LoopGuardState::default();
        let args = r#"{"command":"curl http://service/health"}"#;
        for i in 0..5 {
            let d = g.check("bash", args);
            assert!(
                allow(&d),
                "polling attempt {} should be allowed, got {:?}",
                i + 1,
                d
            );
            g.record("bash", args, &format!("attempt {}: starting", i), false);
        }
        // 5 records made, 6th about to be blocked — pin the boundary.
        assert!(block(&g.check("bash", args)));
    }

    #[test]
    fn json_whitespace_variants_collapse_across_batches() {
        // The cross-batch counterpart to the runner's in-batch dedup
        // normalisation: same call, different JSON formatting across
        // turns must still trip the guard at THRESHOLD. Mirrors the
        // deepseek-v4-flash symptom: model emits identical Glob
        // intent with whitespace drift turn-after-turn.
        let mut g = LoopGuardState::default();
        let v1 = r#"{"pattern":"**/*.rs"}"#;
        let v2 = r#"{"pattern": "**/*.rs"}"#; // extra space
        let v3 = r#"{ "pattern":"**/*.rs" }"#; // padded braces
        let out = "203 files found";

        g.record("glob", v1, out, true);
        g.record("glob", v2, out, true);
        // v3 is the same call by intent → must block.
        let d = g.check("glob", v3);
        assert!(block(&d), "expected Block on JSON-variant repeat, got {:?}", d);
    }

    #[test]
    fn name_collision_is_not_a_match() {
        // Defensive: the `\0` separator in make_key prevents
        // (name="foo", args="bar") from colliding with
        // (name="foob", args="ar") even though concatenation would.
        let mut g = LoopGuardState::default();
        g.record("foo", "bar", "out", true);
        g.record("foo", "bar", "out", true);
        // A call whose name+args concatenate to "foobar" but with a
        // different split must not be considered a repeat.
        assert!(allow(&g.check("foob", "ar")));
    }
}
