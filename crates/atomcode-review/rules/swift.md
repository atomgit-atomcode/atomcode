[Swift] This batch contains Swift code; pay extra attention to:

#### Typos / Dead Code
- Spelling errors in variable, function, and type names at the **declaration site** (do not report at call sites); spelling in log/error messages that affects readability
- Unreachable code (after `return`/`fatalError`), declared but unreferenced variables, large commented-out blocks

#### Error Handling
- Force unwrap `!`, forced `try!` / `as!` on fallible paths (use `if let` / `guard let` / `try?`)
- Errors swallowed by `try?` discarding context; empty `catch {}` masking failures
- `fatalError` / `precondition` on reachable branches; implicitly unwrapped optionals used before assignment

#### Error-Prone Semantics
- Array out-of-bounds subscript; value vs reference type (struct vs class) copy surprises; `guard` without a meaningful `else`
- Floating-point equality; integer overflow (Swift traps — use `&+`/`&*` only when wrapping is intended)

#### Security
- Insecure transport: `http://` URLs, ATS disabled (`NSAllowsArbitraryLoads`); certificate-pinning bypass (`URLSession` delegate accepting any server trust)
- Sensitive data (tokens, passwords, keys) in `UserDefaults`/plist/logs instead of Keychain; hard-coded API keys/secrets
- SQL injection via string-formatted queries (FMDB/SQLite `sprintf`-style); `WKWebView.evaluateJavaScript` with unescaped user input
- Insecure randomness for security tokens (`arc4random`/`Int.random`) instead of `SecRandomCopyBytes`

#### Concurrency and Resources (report only in the following cases)
- Data races on shared mutable state across threads/tasks without synchronization; actor-isolation violations
- UI updates off the main thread (`DispatchQueue.main`); `DispatchQueue.main.sync` from the main thread (deadlock)
- Strong reference cycles leaking memory: closures capturing `self` (need `[weak self]`/`[unowned self]`), delegate properties not `weak`; `unowned` on an object that may be deallocated (crash)
- Do not report: local-only state, read-only access, or already-correct synchronization (actor, serial queue, lock)

#### Performance
- Expensive work (image decode, JSON parse, disk/network) on the main thread blocking UI; repeated allocations or large value-type copies on hot paths
- Building results in a loop with `+`/array append where reserve/`map` is clearer; redundant `NSDateFormatter`/regex creation inside loops
