# WebUI session browsing implementation plan

Goal: browse another session without changing the runtime binding or interrupting its turn.

Owner: the existing Chat retains stream, queue, draft, and interaction state. App owns a separate optional historical view. Only explicit continuation changes the bound session, and only when the existing Chat reports idle with no blocking interaction. Server binding and persistence semantics remain unchanged.

1. Add a cancellable historical reader with explicit loading, error, retry and missing-project handling.
2. Route sidebar navigation to historical browsing while execution is active; retain the original Chat instance and expose its pending interactions and explicitly targeted stop action in the browsing surface.
3. Mark the running session in the sidebar; keep browsing after completion until return or explicit continuation.
4. Validate navigation policy, TypeScript, WebUI tests/build, formatting and compile gate. No runtime or server changes.

Review: interaction cards use one stable portal host when changing presentation seats, preserving partially entered answers. Picking a skill returns to the bound view before changing its draft. Workbench remains mounted while hidden.

Validation: 382 unit tests; TypeScript and production build; desktop/mobile browser scenarios for background text, approval, input-draft retention, completion retention and no binding switch; compile and format gates.
