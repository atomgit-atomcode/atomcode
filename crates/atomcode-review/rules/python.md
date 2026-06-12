[Python] This batch contains Python code; pay extra attention to:

#### Typos / Dead Code
- Spelling errors in variable names, function names, and class names at the **declaration site** (do not report at call sites); spelling in log/exception messages that affects readability
- Unreachable code, declared but unreferenced variables, large blocks of commented-out code

#### Exception Handling
- Broad `except:` / `except Exception` swallows exceptions without logging or re-raising
- Catch followed by only `pass` masks errors; overly broad catch scope hides bugs that should be exposed
- Missing finally / with causing resources not to be released

#### Error-Prone Semantics
- Mutable default arguments (`def f(x=[])` / `={}`) causing cross-call state pollution
- Closure/lambda capturing loop variables in loops (late binding)
- Confusing `is` with `==` (use `==` except for None/singletons)
- Integer/floating-point precision, mutable objects used as dict keys
- Calling methods or subscripting on None without checking
- Missing key in dict/JSON access: `obj["field"]`, `res[0]`, `owner["email"]`, etc. without checking existence or empty lists
- Function may return `None` but is used for file writing, concatenation, subscripting, or iteration, causing TypeError or false success
- HTTP response `json()`, time parsing, type conversion failures are unhandled, causing crashes on boundary input
- Missing `await` on async calls, leading to premature success or lost exceptions

#### Security
- SQL string concatenation / f-string SQL → injection; use parameterized queries
- Command injection: `os.system`/`subprocess(shell=True)` concatenating external input
- `eval`/`exec`/`pickle` processing untrusted data; path traversal
- URL/path validation cannot rely solely on substring checks; file names from URLs/user input must prevent path traversal and overwriting
- TLS `verify=False`, CORS `*`, binding `0.0.0.0`, hard-coded API keys / passwords / tokens should all be reported as real exposure risks

#### Concurrency and Resources (report only in the following cases)
- Shared mutable state without locks under multithreading, shared state between `multiprocessing` processes
- Resources not managed with `with` (files, connections, locks)
- Global DB connections, SQLite `check_same_thread=False`, global cache/state read/written across requests without checking locks and transaction boundaries
- Bare `except` writing cache/disk/network failure then continuing to return success is a false-success/data-consistency issue
- Do not report: "thread safety" issues in pure single-threaded scripts, local variables

#### Performance
- Database queries inside loops (N+1), string `+` concatenation inside loops, building large lists when generators should be used

#### Ineffective Defenses
- A function whose NAME claims sanitizing/validating/escaping must be verified to actually do it: replacing a string with itself, a no-op regex, escaping the wrong characters, or returning the input unchanged are real vulnerabilities hiding behind a reassuring name — report them as the security issue they mask, not as style
