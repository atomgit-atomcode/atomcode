# Mention categories

Replace the mixed @ list with Files & directories and Sessions tabs. Keep the existing composer token as the filter; add no search field. Left/right switch tabs, up/down select, Enter/Tab insert, Esc closes; category labels can also be clicked. Use resume-style rule, selected-row pointer and muted session metadata. Keep insertion spellings and file-index/session-catalog ownership unchanged. Default to files; an empty category remains open and switchable. Share render/hit-test header offsets so scrolling and pointer selection agree.

Validation: TUI suite, i18n compilation and formatting. Preserve concurrent search/console/relay work. No runtime, persistence or dependency changes.

Audit: hide tab hit targets when there is no room to draw them; clamp result count before converting height to u16. Cover a clipped footer click in the empty-category test.

Validation complete: 1764 TUI tests passed after audit fixes, cargo fmt check and git diff check passed, all four compile gate phases passed. No release binary rebuild or interactive terminal verification performed.
