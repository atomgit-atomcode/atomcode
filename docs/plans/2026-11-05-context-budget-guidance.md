# Context budget guidance implementation plan

Goal: explain configured context budgets and the uncertainty of auto routing,
while retaining discovery-prefilled model capacities and explicit user values.
Architecture: presentation and documentation only. Runtime context windows,
compaction thresholds, provider requests and configuration schema stay unchanged.
Historical saved profiles do not retain whether a value originally came from
manual input or discovery, so label the current source as configuration/default,
never invent more specific provenance.
Tech stack: Rust TUI/i18n, Preact WebUI, existing config/discovery tests.

Baseline: release/v5.2.6 at 7d80137cb, clean worktree.

1. Audit TUI take_discovered, WebUI adoptPicked and ProviderBook writes. Existing
   paths already prefill discovered windows and preserve existing tuned profiles;
   reuse them rather than adding another discovery or precedence mechanism.
2. Clarify window/budget labels in TUI and WebUI. Model cards explain that values
   are configuration/default budgets, not measured server capacity; auto routes
   need a conservative budget because a response does not pin the next model.
3. Add the same guidance to WebUI add/edit forms and a bilingual configuration
   document, including discovery/manual precedence, output reserves and no zero
   sentinel. Do not silently overwrite configuration or auto-expand on responses.
4. Verify existing discovery/override regressions, affected TUI/config/i18n tests,
   WebUI unit/type checks, workspace compile gate, format and diff checks.

Validation: TUI 1917 passed, config 358 passed, i18n 73 passed; WebUI provider
form tests 14 passed and TypeScript typecheck passed. Existing discovery and
manual-override tests cover the reused population path. Updated one TUI assertion
to use the translated unspecified-window label instead of the old literal.
Workspace/feature compile gate, format and diff checks passed. No real-provider
requests or native-terminal/browser visual verification were performed.
