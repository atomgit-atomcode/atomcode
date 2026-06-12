[Java] This batch contains Java code; pay extra attention to:

#### Obvious Spelling Errors
- Spelling errors in variable names, method names, and class names at the **declaration site** (confirm with naming conventions for similar identifiers)
- Spelling errors in log/exception message strings that affect readability
- Do not report spelling errors at **reference sites** (reference sites are determined by declarations)

#### Dead Code
- Unreachable code blocks (always-false branches, code after return)
- Declared but never read/referenced variables
- Large blocks of commented-out code (no retention intent)

#### Logic Errors
- if condition logic errors (confirm expected logic based on context)
- Boundary condition handling errors (pay special attention to index and array length checks)
- Boolean operator misuse (precedence and short-circuit evaluation)
- Obvious infinite loops / unterminated recursion
- Using return/break/continue where they should not be used
- Missing break in switch causing accidental fall-through; missing comments for intentional fall-through
- Patterns that may cause NPE (follow the data source call chain to confirm risk)
- Missing parentheses in logical expressions causing execution order to differ from intent

#### Severe Performance Issues
- Database queries executed inside loops (confirm whether the call involves DB operations)
- N+1 queries (suggest batch queries)
- Processing large datasets without pagination
- Inefficient algorithms in nested loops (O(n^2) or worse where a better solution exists)

#### Thread Safety (report only in the following cases)
- Race conditions: "check-then-act" where the intermediate state may be changed by another thread
- Non-atomic composite operations: multi-step operations requiring atomicity lack synchronization
- Unsafe lazy initialization: double-checked locking defects in singletons/caches
- Concurrent writes to non-thread-safe collections: modifying ArrayList, HashMap, etc. under multiple threads

Do not report the following:
- Local variables inside methods (independent copy per thread, naturally thread-safe)
- Single-threaded context (no evidence of multi-threaded calls; confirm based on call context)
- Read-only operations (even if the data structure is not thread-safe)
- Immutable objects (final fields pointing to immutable objects)
- Already correct synchronization (synchronized, Lock, atomic classes, etc.)
- Components designed for single-threaded use (e.g., Builder construction phase, temporary DTOs)
