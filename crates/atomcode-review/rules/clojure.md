[Clojure] This batch contains Clojure code; pay extra attention to:

#### Typos / Dead Code
- Spelling errors in `def`/`defn` names and keywords at the **declaration site** (do not report at call sites); spelling in log/error messages
- Unreachable code, unused bindings, large commented-out blocks (or stray `#_`/`comment` forms left in)

#### Error Handling
- Exceptions swallowed by a bare `catch Throwable` returning `nil`; failures not logged or re-thrown
- nil punning hiding logic errors (nil treated as an empty seq); side effects placed inside a lazy seq that is never realized

#### Error-Prone Semantics
- `nil`/`false` are the only falsy values (`0`/`""` are truthy); `=` (value) vs `identical?` (reference)
- `(get m :k)` vs keyword-as-function on non-map values; integer vs ratio/double division

#### Security
- SQL injection: `clojure.java.jdbc`/`next.jdbc` with string-concatenated SQL → use parameterized vectors `["... = ?" v]`
- Code execution: `read-string`/`load-string`/`eval` on untrusted input (core `read-string` honors `*read-eval*` and `#=` → RCE) — use `clojure.edn/read-string` for data
- Command injection: `clojure.java.shell/sh` or `Runtime.exec` with interpolated external input; insecure Java deserialization via interop; hard-coded secrets

#### Concurrency and Resources (report only in the following cases)
- Non-atomic read-modify-write on an atom (`reset!` after a separate `deref` instead of `swap!`); side effects inside `swap!`/`alter` fns (retried, so must be pure)
- Unsynchronized shared mutable Java-interop state across threads; agent error handlers missing
- Resource handles not closed (use `with-open`) on error paths
- Do not report: immutable values, local-only state, or already-correct STM/atom usage

#### Performance
- N+1 / DB or remote calls inside a `map`/`doseq` (batch instead); holding the head of a large lazy seq causing full realization in memory
- Reflection on hot interop paths (add type hints; enable `*warn-on-reflection*`); `apply concat`/repeated `conj` where transients or `into` are clearer
