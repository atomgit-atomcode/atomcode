[Cangjie] This batch contains Cangjie (.cj) code; pay extra attention to:

#### Type Safety
- Unsafe blocks (`unsafe { ... }`): confirm every raw pointer dereference, union access, and foreign-function call is truly necessary and correctly bounded; prefer safe abstractions.
- Type casts (`as`) that narrow, reinterpret, or erase signedness; confirm no information loss or undefined behavior.
- Uninitialized or partially initialized variables; Cangjie requires definite assignment before use — flag paths where this may not hold.
- `Option<T>` / `Result<T, E>` unwraps without prior checks; prefer `match`/`if-let`/`?` and propagate errors explicitly.

#### Memory & Resource Management
- Resource leaks: file handles, sockets, locks, or other resources acquired without deterministic release; prefer RAII/`try-finally` or scoped wrappers.
- Cyclic references with `class` types that can prevent collection; consider weak references or ownership restructuring.
- Large allocations inside hot loops or unbounded collection growth.

#### Concurrency
- Data races: shared mutable state across threads/tasks without synchronization.
- Use of non-atomic counters or collections from multiple concurrent contexts.
- Missing `await` on async operations or mixing blocking calls inside async contexts.
- Deadlocks from lock ordering or holding locks across `await`/`suspend` points.
- `unsafe` concurrency primitives (raw mutexes, atomics) used incorrectly.

#### Language-Specific Idioms
- `enum` variants matched incompletely; ensure `match` handles all cases or is intentionally exhaustive.
- Mutable variables declared with `var` where `let` would suffice; unnecessary mutability signals.
- Shadowing that obscures a binding in a wider scope and creates confusion.
- `break`/`continue` in nested loops pointing to the wrong loop label.
- Empty `interface` used as a universal type; prefer generic constraints or sum types.

#### Error Handling
- Silent swallowing of `Result` errors (`let _ = ...` on a failing operation).
- Panic-prone APIs (`!` return or explicit `panic`) used for recoverable errors.
- Error messages that leak sensitive values (tokens, paths, user data).

#### Generics & Macros
- Generic constraints that are too loose and allow invalid type arguments at runtime.
- Macro hygiene issues: generated code referencing outer-scope bindings unexpectedly.
- Recursive macro expansions that may diverge or produce huge code.

#### Performance
- Repeated string/buffer concatenation in loops; use builders or streams.
- Reflection or metaprogramming used on hot paths.
- Boxing/heap allocation of small values in tight loops.

#### Security
- User input flowing into command construction, SQL, paths, or foreign-function calls without validation/escaping.
- Hard-coded secrets, keys, or tokens in source.
- `unsafe` FFI calls that trust pointer lengths or buffer sizes from external input.

Do not report the following:
- Pure formatting, indentation, or import ordering.
- Naming style preferences that do not mislead about function or type semantics.
- Comments written in Chinese or non-English text.
