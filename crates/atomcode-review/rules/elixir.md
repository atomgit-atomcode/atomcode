[Elixir] This batch contains Elixir code; pay extra attention to:

#### Typos / Dead Code
- Spelling errors in function, module, and variable names at the **declaration site** (do not report at call sites); spelling in log/error messages
- Unreachable clauses, unused variables (prefix with `_` only when intentional), large commented-out blocks

#### Error Handling
- `{:error, _}` tuples ignored (only `{:ok, _}` matched), so failures pass silently
- `!`-bang functions (which raise) on fallible paths where the `{:ok, _}`/`{:error, _}` variant should be handled
- Non-matching patterns crashing a process unexpectedly; `rescue`/`catch` swallowing the root cause

#### Error-Prone Semantics
- `==` vs `===` (strict); String (binary) vs charlist confusion in pattern matches and concatenation
- Keyword list vs map access assumptions; `nil` flowing into arithmetic/string ops

#### Security
- SQL injection: `Ecto.Adapters.SQL.query`/`fragment("... #{user_input}")` with interpolation → use parameter placeholders (`?` / pinned args)
- Atom-table exhaustion (DoS): `String.to_atom`/`List.to_atom` on external input → use `String.to_existing_atom`
- Code/term injection: `Code.eval_string`/`Code.eval_quoted` on user input; `:erlang.binary_to_term/1` on untrusted data without `[:safe]` (arbitrary term/RCE)
- Command injection: `:os.cmd`/`System.cmd` with a shell or interpolated args from external input; hard-coded secrets (prefer runtime config/env)

#### Concurrency and Resources (report only in the following cases)
- Unsupervised processes (spawned outside a supervision tree); `GenServer.call` with blocking work causing timeouts/back-pressure
- Unbounded mailbox growth (faster producer than consumer); `receive` without an `after` timeout
- ETS tables / ports / connections not cleaned up on process exit
- Do not report: short-lived tasks under a supervisor or already-bounded queues

#### Performance
- Ecto N+1: associations accessed per row without `preload`/`join`; queries inside `Enum.map`/loops (batch instead)
- `Enum` (eager, builds intermediate lists) where `Stream` (lazy) fits large/infinite data; list `++` or `Kernel.++` in a loop (O(n) per call) — prepend then reverse
