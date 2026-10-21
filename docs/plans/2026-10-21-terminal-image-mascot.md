# Terminal image mascot

Use the user-supplied PNG unchanged for the default welcome mascot. Embed it in the binary. Keep the existing half-block cat for terminals without supported image protocols, narrow layouts, and custom brands.

The welcome renderer reserves a 18-column by 8-row image box using paint-only Line metadata. The terminal encoder emits iTerm2 or Kitty control sequences after text, and keeps graphics out of plain text, copy and transcript output. A clipped or overlaid image is suppressed; its old region is repainted when it disappears or moves. Unchanged images must not be retransmitted for unrelated spinner updates.

Use existing terminal graphics capabilities; disable image protocols in tmux/screen without confirmed passthrough. No new dependency or runtime/session change.

Validate protocol encoding, clipping, overlays, repaint caching/removal, fallback, custom-brand behavior and the embedded PNG. Run TUI tests, formatting and compile gate.

## Validation and protocol references

- TUI nextest: 1780 passed. Includes PNG/payload integrity, protocol chunk sizes, image clipping/overlay removal, diff caching and real host stream composition/capability fallback.
- `bash gates/compile.sh`: passed. Formatting and diff checks passed; Cargo.lock unchanged.
- Real iTerm2/Kitty visual verification is still pending.
- iTerm2: https://iterm2.com/documentation-images.html
- Kitty: https://sw.kovidgoyal.net/kitty/graphics-protocol/

WezTerm uses its documented iTerm2-compatible image protocol: https://wezterm.org/imgcat.html

## Click sequence

A click on the visible image restarts one sequence: wait PNG for 1500 ms, go PNG for 1500 ms, then the original PNG. All frames share the existing 18 by 8 cell box. Pixel fallback remains static. Playback belongs to Moment, driven by the existing injected clock/event loop, and is reset on session view changes; no task or runtime/session-log state is added. Overlay-hidden or clipped images are not clickable.

Click-sequence validation: 1785 TUI tests passed, including phase boundaries, fixed geometry, click/restart, static pixel fallback, overlay hit testing and view reset. Compile gate and formatting passed. Assets are copied unchanged from the supplied PNGs. Animation has not yet been visually checked in a real terminal.
