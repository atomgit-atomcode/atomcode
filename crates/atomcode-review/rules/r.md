[R] This batch contains R code; pay extra attention to:

#### Typos / Dead Code
- Spelling errors in variable and function names at the **declaration site** (do not report at call sites); spelling in messages/warnings
- Unreachable code, declared but unused variables, large commented-out blocks

#### Error Handling
- Errors/warnings suppressed (`try`/`tryCatch` returning silently, `suppressWarnings`) so failures pass unnoticed
- Return values of file/DB/HTTP operations unchecked; partial argument matching masking a typo'd argument name

#### Error-Prone Semantics
- Silent vectorized recycling of mismatched-length vectors; `==` with `NA` propagating (use `is.na`); 1-based indexing
- `<-` vs `=` vs `<<-` (global assignment) confusion; `T`/`F` are reassignable, unlike `TRUE`/`FALSE`
- `sapply` returning an unexpected type (prefer `vapply`); factor vs character surprises (`stringsAsFactors`)

#### Security
- Code injection: `eval(parse(text = user_input))` on untrusted strings (arbitrary execution)
- Command injection: `system`/`system2`/`shell` with concatenated external input
- SQL injection: `paste`/`sprintf` building queries → use `DBI::dbBind` parameterized queries / `dbQuoteLiteral`
- Deserializing untrusted `.RData`/`readRDS`/`load` (can execute on load); hard-coded credentials/API keys in scripts

#### Concurrency and Resources (report only in the following cases)
- Connections (`file`, `DBI`, `url`) not closed (`on.exit(close(...))`) on error paths
- `parallel`/`foreach` workers sharing mutable state or non-reproducible RNG without `set.seed`/`clusterSetRNGStream`
- Do not report: single-threaded scripts with locally-scoped connections already closed

#### Performance
- Growing a vector/data.frame inside a loop (`c()`/`rbind` reallocating each iteration) — preallocate or use vectorized ops
- Explicit loops where a vectorized op / `apply` family fits; repeated copy-on-modify of large objects in a loop
