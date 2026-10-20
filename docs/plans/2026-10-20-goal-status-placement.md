# Goal status placement

Keep the user-set rename pill on the input box's upper rule. Move `/goal active` /
`/goal paused` to the already-reserved tip row above the input box, right-aligned.
Concurrent notices/clipboard hints use the remaining left-hand space, clipped to
one row. Hide goal status for secret prompts and degrade gracefully on narrow
terminals. This adds no layout rows and changes no goal lifecycle state.

The `/goal` command acknowledgement is a short localized confirmation; the goal
condition remains in the authoritative user message rather than being duplicated
in the command response before it. No runtime event or session format changes.

Test active/paused styling, rename coexistence, narrow widths and generic tool
progress visibility without additional transcript rows.
