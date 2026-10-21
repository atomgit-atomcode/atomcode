# Default MCP discovery to on

Goal: make existing schema discovery effective without manual configuration, including moderate MCP catalogs on large-context models.

Baseline: release/v5.2.4 at 6cb9011ce. Private crypto overlay and Cargo.lock changes present before this task are user-owned and must be preserved. Configuration remains owned by atomcode-config; coding's existing request projection remains the only schema selector. No new lifecycle owner, persistence format, provider transport, or approval path.

Change: default the config enum to On. Keep explicit Auto/Off values and the existing byte budget, latest-search selection, and eager fallback when search is unavailable. Update runtime self-description and user docs together. Missing settings adopt On on config load; already-running runtimes need normal capability reprepare/restart. Search introduces an extra discovery step and model-quality risk; Off remains the escape hatch. Do not write global user configuration.

Verify: absent/nested-empty config, all explicit values round-trip through save/load, and reset returns On; default mode defers a moderate MCP catalog at a 1M window while explicit Auto/Off preserve prior behavior. Run config and coding crate suites sequentially, formatting, and compilation gate. No changes to the pre-existing overlay/lockfile are included.

Fixture adaptation: storage and runtime connection/publication tests explicitly choose Off so their existing presence/absence assertions continue to test connection, trust, cancel, session switch and undo rather than discovery selection. The real-stdio discovery regression uses the product default and runs both small and large schemas, preserving original-name execution and bounded history checks.
