# MCP search result limits

Scope: separate the default search result count (8) from a safety ceiling (32), retaining the existing schema byte budget, current admitted tool catalog, paired-result validation, and latest-successful-selection behavior. The request projection owns schema selection; no runtime lifecycle, persistence format, approval boundary, or auto-mode threshold changes.

Reference: local Codex tool_discovery.rs defaults to 8 and its tool_search handler accepts a caller-supplied positive limit. Codex returns full definitions as native ToolSearchOutput items; compact_remote_history.rs can clear their tools during sanitization. These transport semantics do not establish identical retention in AtomCode. Cumulative loading remains a separate investigation, not part of this change.

Alternatives: keeping five limits multi-tool workflows; unbounded caller limits can expand discovery output; bounded caller choice preserves predictable output while supporting broader searches. Search metadata stays capped at 32 descriptions of 240 characters, while full definitions must fit the unchanged byte budget. A successful search still replaces the previous selection; missing, failed, unpaired, or compacted-away results never introduce schemas.

Validation: default/explicit/oversized limits round-trip through search and request projection; existing pagination, schema-budget, policy, and session-switch regressions; coding crate tests and formatting. This improves discovery flexibility, not initial schema savings or measured model discovery accuracy.
