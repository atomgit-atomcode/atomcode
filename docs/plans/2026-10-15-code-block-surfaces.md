# Code block surfaces

Baseline: 7e7a10fbf; clean worktree. Use a theme-derived panel background to group the language header with highlighted source. No printable gutters, fences, line numbers or padding are added to source.

Introduce a generic background attribute on rendered Line. ANSI paints the available row with ECH under the requested background, then draws text inheriting that background. No spaces are appended to Line.plain or transcript payloads. Preserve background through clipping/restyling and account for erased cells in frame projections and link hit testing. Disable decoration on no-color terminals.

Copy code continues to extract original fenced source from session events, excluding label and styling. Native terminal selection can include the language header if selected, and terminal-specific wrapping/trailing-space behavior is not controlled by this renderer.

Verify plain YAML/command content, blank/soft-wrapped code rows, background-only rows, no-color ANSI output, exact copy, and TUI regression/compile gates.

Validation: all 1672 TUI tests passed (1483 unit, 177 e2e, 12 other integration); workspace/feature compile gates and formatting/diff checks passed. The extra tui-layers gate failed, but the archived pre-change HEAD reproduces the exact same counts and existing ansi.rs environment probe (literal_glyphs=465, raw_colours=1, os_probes=1); this patch adds no layering violations and does not alter that baseline. Ghostty/Windows visual and native-selection behavior have not been manually exercised. The release executable has not been rebuilt.

Commit audit: no blocking findings. Added regression coverage for cache invalidation when only the background changes (including removal), background capability fallback and inheritance onto text, and surface preservation through the host inset. All 1486 unit tests passed after these test-only additions; the prior complete 1672-test run covers unchanged production code. Compile gates, formatting and diff checks passed again. The existing layering-gate failures remain unchanged and are not waived or hidden by altering the baseline.
