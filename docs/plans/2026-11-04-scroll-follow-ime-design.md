# Scroll following and IME guards

The browser reproduction on release/v5.2.6 showed a 25px upward scroll being
reset by the next VSCode stream update. A 229 key event with isComposing=false
also closed plugin search and submitted WebUI session rename.

Use explicit following/reading intent: upward movement pauses following, and
reaching the bottom (2px rounding tolerance) or the latest button resumes it.
Keep an immediate ref for plugin follow intent so streaming effects see user
changes before React commits. Preserve WebUI background restoration and session
navigation ownership; update its existing scroll owner rather than adding one.
Ignore composition/229 events before secondary-input navigation, submit and
Escape handlers, including ancestor search listeners.

Validate real components with the existing Chrome reproduction, small/large
scrolls, return-to-bottom, touch, composition controls, and ordinary Enter/Escape.
Run WebUI and plugin checks plus the repository compile/format gates. No runtime,
provider, session format, terminal input or approval changes are needed.

Validation: 385 WebUI tests, plugin tests/build, WebUI typecheck/build,
scroll/IME browser checks, mobile/desktop timeline checks, compile gate and
format gate passed. Browser checks include 25px upward scrolling, large scrolls,
return-to-latest, layout shrink and 229/ordinary keyboard controls. Real touch
hardware, Windows and native IME candidate windows remain manual checks.
