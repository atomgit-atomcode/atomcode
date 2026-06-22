[Rust] This batch contains Rust code; pay extra attention to:

#### panic risks
- `unwrap()`/`expect()` on fallible paths (should propagate `Result`/`Option`)
- Out-of-bounds slice/array index, integer arithmetic overflow (wraps in release; use checked_/saturating_ when needed)
- `unreachable!()`/`panic!()` on reachable branches

#### unsafe / memory
- Whether memory-safety preconditions in `unsafe` blocks hold (raw pointer dereference, lifetimes, aliasing rules)
- transmute, uninitialized memory, ownership and release responsibility at FFI boundaries

#### error handling
- Errors silently discarded by `let _ =` or `.ok()`; missing `?` causing errors not to propagate
- Custom errors losing context

#### concurrency
- Holding locks across `.await` causing deadlock, misuse of `Mutex`/`RwLock`
- Misuse of `Send`/`Sync` constraints, race logic in `Arc<Mutex<_>>`

#### other
- Unnecessary `clone()` affecting performance, borrow and lifetime simplification opportunities, prefer iterators over hand-written loops
