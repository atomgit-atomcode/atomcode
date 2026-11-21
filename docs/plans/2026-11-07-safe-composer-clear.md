# Safe composer clearing implementation plan

**Goal:** Ctrl+C on a nonempty composer discards only its draft, including recalled history and attachments.

**Architecture:** The TUI resolves Cancel to its existing Clear action before quit bookkeeping. Moment owns composer cleanup; runtime cancellation is unchanged for an empty composer. Selection, search and modal routing retain priority.

**Validation:** Cover idle and active turns, attachment-only drafts, quit disarming, history retention, and queued work returned by Esc. Run TUI and i18n tests, the compile gate, and formatting checks.

1. Centralize draft cleanup and route nonempty Cancel to Clear.
2. Add localized hints when history browsing begins or a search match is accepted.
3. Add regressions and update the queued-work scenario for the new input-first behavior.
4. Review the diff and complete validation.
