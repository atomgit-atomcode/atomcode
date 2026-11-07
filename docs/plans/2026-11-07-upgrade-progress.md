# Upgrade progress implementation plan

**Goal:** Replace download text spam with an in-place TUI card and a TTY-only CLI progress bar, with rate and ETA.

**Boundary:** The launcher owns updating the executable and samples monotonic time; TUI owns presentation. No updater event schema, agent lifecycle, session persistence or restart behavior changes. Use the existing block writer's open/amend/settle lifecycle through a dedicated TUI service, not UserInterface or a new runtime owner.

1. Implement shared presentation data and width-aware ASCII progress rendering.
2. Sample the last three seconds at 150 ms intervals; retain a bounded set of samples and represent unknown rate/size/ETA honestly.
3. Update one live TUI block until success/failure; continue to show verification and replacement after download completion.
4. Render one CLI line only on TTY; retain stage logs for redirected output and clear the line on every terminal path.
5. Test sampling/stalls, unknown totals, narrow widths, and live-block settlement. Run affected crate tests, the compile gate, and formatting checks.

Throughput charts are deferred. No download/install is invoked by verification.
