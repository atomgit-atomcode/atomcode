[Lua] This batch contains Lua code; pay extra attention to:

#### Typos / Dead Code
- Spelling errors in local/function/field names at the **declaration site** (do not report at call sites); spelling in log/error messages
- Unreachable code, unused locals, large commented-out blocks

#### Error Handling
- `pcall`/`xpcall` results ignored (the `ok, err` pair not checked); `error()` thrown but never caught on a path that must recover
- Functions returning `nil, err` where only the first value is used, dropping the error

#### Error-Prone Semantics
- Accidental globals (missing `local`) leaking or clobbering shared state; indexing a `nil` value (runtime error)
- nil vs false truthiness (only `nil`/`false` are falsy; `0` and `""` are truthy)
- 1-based indexing off-by-one; `#t` length is undefined for tables with `nil` holes; `ipairs` stops at the first `nil`; integer/float (5.3+) and string↔number coercion surprises

#### Security
- Code injection: `loadstring`/`load`/`dofile`/`loadfile` on untrusted input (arbitrary execution)
- Command injection: `os.execute`/`io.popen` with concatenated external input
- SQL injection: `string.format`/concatenation into queries (esp. OpenResty/lua-resty-mysql) → use parameter binding / quoting APIs
- In OpenResty: trusting `ngx.req`/`ngx.var` input without validation; path traversal from user-controlled paths; hard-coded secrets

#### Concurrency and Resources (report only in the following cases)
- File/socket handles (`io.open`, sockets) not closed on error paths
- Coroutines left suspended holding resources; in OpenResty, blocking the event loop with synchronous I/O
- Do not report: local-only short-lived handles already closed

#### Performance
- String concatenation with `..` inside loops (build a table + `table.concat`); repeated global lookups in hot loops (localize to upvalues)
- Rebuilding/rehashing large tables in loops; redundant pattern compilation where it can be hoisted
