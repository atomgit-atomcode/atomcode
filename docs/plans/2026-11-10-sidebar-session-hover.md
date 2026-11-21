# Sidebar session identity card

Remove the persistent running-task sidebar hint; retain the existing running badge and the actual read-only browsing banner. Session selection and runtime switching remain unchanged.

The local deepseek-harness `ui-workspace/src/client/rows/Rows.tsx` reveals clipped titles with a slow hover marquee. Its `ui-primitives/src/Tooltip.tsx` uses delayed hover/focus, portals and viewport positioning. AtomCode uses those latter interaction principles for a full-title/path card, rather than copying the marquee, because the requested content includes the directory as well as the title.

`SessionHoverCard` owns only presentation state. It renders the existing session button without a wrapper that changes row layout. The card uses shared light/dark menu tokens, wraps both the title and the full directory, and is portaled outside the sidebar's scroll clipping. It opens after 350 ms, closes on Escape, outside pointer, resize or sidebar scroll, and can remain visible while the pointer moves over it. Keyboard focus uses aria-describedby. Click closes the preview before running the existing selection callback. Touch interaction does not require a hover.

Validation: WebUI typecheck and production build, existing desktop/narrow session-browsing E2E expanded with hover/focus/dismissal/content/bounds assertions, and visual checks of light/dark captures. No live provider or user session is used. Reviewed in the isolated fix/sidebar-session-hover worktree, then integrated on the latest release base with the sidebar resize behavior preserved. Review also covers persistence while the trigger has keyboard focus or the pointer is selecting text inside the card.
