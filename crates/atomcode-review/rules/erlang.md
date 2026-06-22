[Erlang] This batch contains Erlang code; pay extra attention to:

#### Typos / Dead Code
- Spelling errors in function, module, and variable names at the **declaration site** (do not report at call sites); spelling in log/error messages
- Unreachable clauses, unused variables (use `_`-prefix only when intentional), large commented-out blocks

#### Error Handling
- Unmatched return tuples ignored (`{error, _}` dropped); catch-all clauses hiding the real error
- Non-exhaustive `case`/function clauses crashing the process where graceful handling is expected

#### Error-Prone Semantics
- `=:=`/`=/=` (exact) vs `==`/`/=` (coercing); list (string) vs binary string handling mismatches
- Integer/float coercion in arithmetic and comparisons

#### Security
- Atom-table exhaustion (DoS): `list_to_atom`/`binary_to_atom` on external input → use `list_to_existing_atom`
- Term injection / RCE: `binary_to_term/1` on untrusted data without `[safe]`; `os:cmd` with interpolated external input (command injection)
- Code loading/eval from untrusted sources; hard-coded secrets/credentials in source or `sys.config`

#### Concurrency and Resources (report only in the following cases)
- Unsupervised spawned processes (outside a supervisor); `gen_server` handle_call doing blocking work causing timeouts
- Unbounded mailbox growth (selective `receive` scanning a large mailbox); `receive` without an `after` timeout
- ETS tables / ports / sockets not cleaned up on process exit
- Do not report: short-lived processes under supervision or already-bounded queues

#### Performance
- `++` (list append) or `lists:append` inside loops/recursion (O(n) each — prepend and `lists:reverse`)
- Large terms copied between processes on hot paths (consider binaries/ETS); repeated DB/remote calls in a loop where batching applies
