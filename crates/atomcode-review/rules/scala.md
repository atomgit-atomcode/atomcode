[Scala] This batch contains Scala code; pay extra attention to:

#### Typos / Dead Code
- Spelling errors in `val`/`def`/class/object names at the **declaration site** (do not report at call sites); spelling in log/error messages
- Unreachable code, declared but unreferenced bindings, large commented-out blocks

#### Error Handling
- `.get` on `Option`, `.head`/`.last` on a possibly-empty collection, non-exhaustive pattern matches (`MatchError`)
- `Try`/`Either` results ignored; `Future` composition swallowing failures (no `recover`/`onFailure`); `Await.result` blocking

#### Error-Prone Semantics
- `==` (structural) vs `eq` (reference); numeric widening / integer division truncation
- Collection strictness: views/iterators consumed once; side effects inside `map`/`for` comprehensions; implicit conversions/parameters causing surprising resolution

#### Security
- SQL injection: string-interpolated SQL via `s"..."` into JDBC/Anorm/raw queries → use parameterized queries / Slick's `sql"..."` (which escapes)
- Insecure deserialization: Java `ObjectInputStream` on untrusted bytes; command injection via `sys.process` (`"cmd".!` / `Seq(...).!`) with external input
- XXE: `scala.xml.XML.load*` with the default parser on untrusted input; hard-coded secrets/credentials; SSRF from user-controlled URLs

#### Concurrency and Resources (report only in the following cases)
- Blocking calls (`Await`, JDBC, I/O) on the global `ExecutionContext` starving the thread pool
- Mutable `var` or non-thread-safe mutable collections shared across threads/futures without synchronization; check-then-act races
- `lazy val` initialization races; resources not closed (use `Using`/try-finally) on error paths
- Do not report: immutable values, local-only state, or already-correct synchronization

#### Performance
- N+1 / DB queries inside loops or `for`-comprehensions (batch instead); building large strict collections where a `view`/`Iterator`/`Stream` suffices
- String `+` concatenation inside loops (use `StringBuilder`/`mkString`); repeated regex compilation inside loops
