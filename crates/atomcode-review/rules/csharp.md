[C#] This batch contains C# code; pay extra attention to:

#### Typos / Dead Code
- Spelling errors in variable, method, and type names at the **declaration site** (do not report at call sites); spelling in log/exception messages that affects readability
- Unreachable code (code after `return`/`throw`, always-false branches), declared but unreferenced variables, large commented-out blocks

#### Exception Handling
- Empty `catch {}` or `catch (Exception)` swallowing the root cause without logging or re-throwing
- Rethrow with `throw ex;` losing the original stack trace (use `throw;`)
- `async void` outside event handlers (exceptions cannot be caught); fire-and-forget `Task` not awaited, losing exceptions
- `IDisposable` not disposed (missing `using`) on error paths

#### Error-Prone Semantics
- NullReferenceException: dereferencing possibly-null returns, `as` cast result used without a null check, dictionary indexer `[]` on a missing key throws (use `TryGetValue`)
- `==` on reference types when value equality is intended; struct copy semantics; `decimal` vs `double` for money
- LINQ deferred-execution surprises (query re-evaluated each enumeration); integer division truncation; `DateTime` vs `DateTimeOffset` and UTC handling

#### Security
- SQL injection: string-concatenated/interpolated SQL in `SqlCommand`, EF `FromSqlRaw`/`ExecuteSqlRaw` with interpolation → use parameters / `FromSqlInterpolated`
- Command injection: `Process.Start` with a concatenated command line or `UseShellExecute=true` on external input
- Insecure deserialization of untrusted data: `BinaryFormatter`, `SoapFormatter`, `JavaScriptSerializer`, Json.NET `TypeNameHandling.All/Auto`
- Path traversal: `Path.Combine`/file APIs with unvalidated user input; XXE: `XmlReader`/`XmlDocument` with `DtdProcessing`/`XmlResolver` enabled
- TLS bypass: `ServerCertificateValidationCallback => true`; weak crypto (MD5/SHA1/DES); hard-coded connection strings / API keys / passwords

#### Concurrency and Resources (report only in the following cases)
- `.Result` / `.Wait()` / `.GetAwaiter().GetResult()` on async work deadlocking in a sync context; missing `ConfigureAwait(false)` in library code
- Shared mutable state (static fields, captured locals) read/written from multiple tasks without a lock or concurrent collection; check-then-act races
- `async` lambda passed where `Action` is expected (silently becomes `async void`); broken double-checked locking on singletons/caches
- Do not report: thread-local/local variables, read-only access, or already-correct synchronization (`lock`, `Interlocked`, `Concurrent*`)

#### Performance
- N+1 in EF/LINQ (lazy loading inside a loop; use `Include`/projection); `IEnumerable` enumerated multiple times; missing `.ToList()` causing repeated queries
- `HttpClient` created per call (socket exhaustion — reuse/`IHttpClientFactory`); string `+` concatenation inside loops (use `StringBuilder`); avoidable boxing on hot paths
