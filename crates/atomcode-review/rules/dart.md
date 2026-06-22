[Dart] This batch contains Dart/Flutter code; pay extra attention to:

#### Typos / Dead Code
- Spelling errors in variable, function, and class names at the **declaration site** (do not report at call sites); spelling in log/error messages
- Unreachable code (after `return`/`throw`), declared but unreferenced variables, large commented-out blocks

#### Error Handling
- Unawaited `Future`s losing errors and ordering; missing try/catch around `await`; swallowed `Future` errors (empty `catch`)
- Force unwrap `!` or `late` used before assignment (`LateInitializationError`); unhandled null from external/JSON parsing

#### Error-Prone Semantics
- `==` vs `identical`; `/` (double division) vs `~/` (integer division); `==` and `hashCode` not overridden together
- `setState` after `dispose` / on an unmounted widget (check `mounted`); `BuildContext` used across an async gap after the widget unmounted

#### Security
- Insecure transport: `http://` URLs; `badCertificateCallback` returning `true` (accepts any cert)
- SQL injection: `sqflite` `rawQuery`/`rawInsert` with string-interpolated user input → use `?` placeholders + args
- `WebView` with JavaScript enabled loading user-controlled URLs/HTML (XSS / injection); path traversal from user-controlled file paths
- Hard-coded API keys/secrets/tokens in source or committed config

#### Concurrency and Resources (report only in the following cases)
- Controllers / `StreamSubscription`s / `AnimationController`s / `Timer`s not cancelled or disposed in `dispose()` (leaks, callbacks after teardown)
- Shared mutable state across isolates without message passing; unguarded concurrent access
- Do not report: local-only state or already-correct disposal

#### Performance
- N+1 / network or DB calls inside a loop or per list item (batch instead)
- Expensive work in `build()` (it runs every rebuild); missing `const` constructors; `ListView`/`GridView` without `.builder` for large/long lists; large synchronous JSON parse on the UI isolate (use `compute`)
