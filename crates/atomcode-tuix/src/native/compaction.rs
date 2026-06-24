//! Compaction result-text synthesis for the native adapter.
//!
//! The kernel measures conversation BYTES, not tokens, and is terse about compaction;
//! the driver renders v1-parity result lines. These pure helpers turn a kernel
//! `Compacted` event into the text the TUI shows (a streamed `manual /compact` result,
//! or a terse auto/overflow advisory). Relocated from the bridge. The async
//! CompactionStarted/Compacted handling that calls them lives in the adapter run loop.

/// Terse one-line advisory for an AUTO / OVERFLOW compaction.
///
/// A manual `/compact` is handled by the richer [`manual_compact_result`] path (emitted as
/// a `TextDelta`) so this returns `None` for it. A committed auto/overflow compaction
/// announces itself once; a no-op stays silent — no user action to acknowledge.
pub(crate) fn compaction_advisory(
    committed: bool,
    trigger: &atomcode_kernel::message::CompactTrigger,
) -> Option<String> {
    use atomcode_kernel::message::CompactTrigger;
    if matches!(trigger, CompactTrigger::Manual { .. }) {
        return None; // manual → the TextDelta `manual_compact_result` path
    }
    committed.then(|| "conversation compacted".to_string())
}

/// v1-parity result line for a manual `/compact`, emitted as a `TextDelta`. The kernel
/// measures BYTES, so token figures are derived: `before_tokens` is the real pre-compaction
/// context size (last provider usage); the after-figure scales it by the byte-reduction
/// ratio. A refused / no-op compaction reports `(nothing to compact …)`.
pub(crate) fn manual_compact_result(
    committed: bool,
    removed: usize,
    bytes_before: usize,
    bytes_after: usize,
    before_tokens: usize,
) -> String {
    use atomcode_core::i18n::{t, Msg};
    // Fall back to a coarse bytes/4 estimate only if no usage has been reported yet (e.g.
    // `/compact` before any turn) so figures never read "0 → 0".
    let before_tokens = if before_tokens > 0 { before_tokens } else { bytes_before / 4 };
    let after_tokens = estimate_after_tokens(before_tokens, bytes_before, bytes_after);
    let before = fmt_k_tokens(before_tokens);
    let after = fmt_k_tokens(after_tokens);
    if committed {
        return t(Msg::CompactDropped { messages: removed, before: &before, after: &after })
            .into_owned();
    }
    // Refused. Distinguish "would not save tokens" (candidate grew) from "conversation is
    // short" (byte-identical true no-op). Mirrors v1's two distinct messages.
    if bytes_after > bytes_before {
        t(Msg::CompactNothingNoSavings { before: &before, after: &after }).into_owned()
    } else {
        t(Msg::CompactNothingShort).into_owned()
    }
}

/// Estimate the post-compaction token count by scaling the pre-compaction count by the
/// kernel's byte-reduction ratio. `bytes_before == 0` ⇒ unchanged (no divide-by-zero).
pub(crate) fn estimate_after_tokens(
    before_tokens: usize,
    bytes_before: usize,
    bytes_after: usize,
) -> usize {
    if bytes_before > 0 {
        ((before_tokens as u128 * bytes_after as u128) / bytes_before as u128) as usize
    } else {
        before_tokens
    }
}

/// Port of core's (private) `fmt_k_tokens`: `1234` → `"1.2K"`, `< 1000` → verbatim.
pub(crate) fn fmt_k_tokens(t: usize) -> String {
    if t >= 1000 {
        format!("{:.1}K", t as f64 / 1000.0)
    } else {
        t.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::CompactTrigger;

    #[test]
    fn compaction_advisory_announces_committed_auto_overflow_and_ignores_manual() {
        assert!(compaction_advisory(true, &CompactTrigger::Manual { focus: None }).is_none());
        assert!(compaction_advisory(false, &CompactTrigger::Manual { focus: None }).is_none());
        assert!(compaction_advisory(true, &CompactTrigger::Auto { utilization: 0.9 }).is_some());
        assert!(compaction_advisory(true, &CompactTrigger::Overflow { attempt: 0 }).is_some());
        assert!(compaction_advisory(false, &CompactTrigger::Auto { utilization: 0.9 }).is_none());
        assert!(compaction_advisory(false, &CompactTrigger::Overflow { attempt: 0 }).is_none());
    }

    #[test]
    fn manual_compact_result_reports_dropped_count_and_token_reduction() {
        let line = manual_compact_result(true, 129, 170_000, 44_000, 42_900);
        assert!(line.contains("129"), "dropped-count missing: {line}");
        assert!(line.contains("42.9K"), "before figure missing: {line}");
        assert!(line.contains("11.1K"), "after figure missing: {line}");
        assert!(line.contains('→'), "token-reduction arrow missing: {line}");
    }

    #[test]
    fn manual_compact_result_true_noop_is_short_line_without_arrow() {
        let noop = manual_compact_result(false, 0, 8_000, 8_000, 6_000);
        assert!(!noop.is_empty(), "no-op must still acknowledge the command");
        assert!(!noop.contains('→'), "true no-op must not show a token comparison: {noop}");
        assert!(!manual_compact_result(false, 0, 0, 0, 0).contains('→'));
    }

    #[test]
    fn manual_compact_result_net_loss_reports_no_savings_with_figures() {
        let line = manual_compact_result(false, 0, 10_000, 15_000, 5_000);
        assert!(line.contains('→'), "net-loss line shows a token comparison: {line}");
        assert!(line.contains("5.0K"), "before figure: {line}");
        assert!(line.contains("7.5K"), "after figure: {line}");
    }

    #[test]
    fn manual_compact_result_falls_back_to_byte_estimate_without_usage() {
        let line = manual_compact_result(true, 3, 40_000, 20_000, 0);
        assert!(line.contains("10.0K"), "byte-fallback before figure: {line}");
        assert!(line.contains("5.0K"), "byte-fallback after figure: {line}");
    }

    #[test]
    fn estimate_after_tokens_scales_by_byte_ratio() {
        assert_eq!(estimate_after_tokens(42_900, 170_000, 44_000), 11_103);
        assert_eq!(estimate_after_tokens(5_000, 0, 0), 5_000);
    }

    #[test]
    fn fmt_k_tokens_matches_core_format() {
        assert_eq!(fmt_k_tokens(0), "0");
        assert_eq!(fmt_k_tokens(999), "999");
        assert_eq!(fmt_k_tokens(1000), "1.0K");
        assert_eq!(fmt_k_tokens(42_900), "42.9K");
    }
}
