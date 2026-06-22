[Haskell] This batch contains Haskell code; pay extra attention to:

#### Typos / Dead Code
- Spelling errors in function, type, and binding names at the **declaration site** (do not report at call sites); spelling in log/error messages
- Unreachable/dead bindings, unused top-level definitions, large commented-out blocks

#### Error Handling
- Partial functions on possibly-empty/invalid input: `head`/`tail`/`fromJust`/`!!`/`read` (use safe variants / pattern matching)
- Incomplete (non-exhaustive) pattern matches; `error`/`undefined` on reachable paths
- `Either`/`Maybe`/`ExceptT` results discarded; IO exceptions unhandled where recovery is expected

#### Error-Prone Semantics
- Orphan/overlapping instances causing surprising resolution; `Integer` vs `Int` (overflow) choice
- Lazy `Maybe`/tuple fields hiding bottoms; `Data.Map` lookup assumptions

#### Security
- Command injection: `System.Process` `callCommand`/`shell` / `system` with concatenated input → use `proc`/`RawCommand` with an argument list
- SQL injection: raw query strings (postgresql-simple `Query` built by concatenation) → use `?` placeholders / parameter lists
- `read` / `Read`-based parsing or deserialization of untrusted input; path traversal from user-controlled paths; hard-coded secrets

#### Concurrency and Resources (report only in the following cases)
- `MVar` deadlocks (taking without putting, nested takes); `STM` retries with side effects; async exceptions not masked around resource acquisition (use `bracket`)
- Shared `IORef` read-modify-write races (use `atomicModifyIORef'`)
- Lazy I/O (`readFile`/`hGetContents`) holding handles open / reading after close
- Do not report: pure code or already-correct `bracket`/`STM` usage

#### Performance
- Space leaks from lazy left folds (`foldl`/lazy accumulators) — use strict `foldl'`/`BangPatterns`; thunk buildup retaining large structures
- `String` (`[Char]`) on hot paths where `Text`/`ByteString` is appropriate; `++` building large lists in a loop (use difference lists / `Builder`)
