[Go] This batch contains Go code; pay extra attention to (real defects that are hard for lint to catch and easy to miss):

#### Typos / Dead Code
- Spelling errors in variable names, function names, and type names at the **declaration site** (do not report at call sites); spelling in log/error messages that affects readability
- Unreachable code, declared but unreferenced variables, large blocks of commented-out code

#### Error Handling
- Swallowed errors: `_ = f()`, using return values without checking err, err overwritten by subsequent assignment and lost
- Error wrapping losing context (direct `return err` where `fmt.Errorf("...: %w", err)` is needed on critical paths)
- Errors produced in `defer` are unhandled (e.g., `defer f.Close()` should check close error when writing files)
- Silent fail-open / false success: external dependencies (Redis/DB/HTTP/script loading) fail but return success or allow pass-through, causing rate limiting, auth, cache, and audit to fail
- Protocol/runtime errors such as `NOSCRIPT`, connection reset, timeout, short read must have proper fallback instead of permanently staying in a bad state

#### nil / panic
- Nil pointer dereference: empty interface check, using map lookup result directly, dereferencing pointer without validation
- Type assertion without `, ok` (`v := x.(T)` will panic when uncertain)
- Slice/array out-of-bounds index; writing to nil map (reading nil map is safe, writing to nil map panics)
- Boundary value paths: does `limit=0`, `maxSize=0`, empty key, empty slice/map, `n<=0`, `Rate<=0` cause division by zero, unbounded growth, or invalid requests

#### Concurrency (report only in the following cases)
- Race conditions: "check-then-act" on shared variables, multiple goroutines reading/writing the same map/slice without locks
- Goroutine leaks: started goroutines have no exit path, not controlled by context cancellation
- Channel: sending to closed channel, duplicate close, unbuffered channel send/receive mismatch causing deadlock
- Loop variable capture (Go pre-1.22 `for` variable captured by closure/goroutine)
- `WaitGroup.Add` called inside goroutine, `sync.Mutex` copied by value
- Shutdown path: does `Stop/Close` wait for goroutines to exit, does it still access closed resources

- For each shared variable with a reported race: check ALL of its access points, not just the flagged write — an unsynchronized READ (a getter, a stats/Count method) of the same variable is a SEPARATE defect at a different line; report it separately

Do not report the following: local variables, no evidence of multi-goroutine calls, read-only access, already correct synchronization (Mutex/atomic/channel)

#### Resources and Performance
- `defer` inside loops causing resources to accumulate until function exit before release
- `http.Response.Body`, files, DB rows not closed
- String concatenation with `+` inside loops (should use `strings.Builder`), repeated regex compilation inside loops, database queries inside loops (N+1)
- Redis/cache keys missing TTL causing unbounded accumulation; write-only maps/caches growing unboundedly with connection IDs or request IDs
- Repeated allocation of small objects or repeated loading of scripts/configurations on hot paths; when call frequency or actual impact is uncertain, report as P3 with lower confidence instead of dropping it

#### Protocol / Time Semantics
- `net.Conn.Read` / stream reads cannot assume reading full in one shot; fixed-length protocols should use `io.ReadFull` or loop to read
- `time.Duration(float) * time.Second` truncates float first; subsecond wait, retry, reset times must not lose precision
- `time.Time{}` deadline cleanup: does it swallow errors, does it pollute subsequent connection state

#### context
- Blocking calls missing context or timeout; cancellation signal not passed downstream
- Abusing context to store business data, using `context.Background()` where a passed ctx should be used

#### Other Pitfalls
- External input not validated, integer overflow/conversion truncation, time comparison using `==`, relying on map iteration order
