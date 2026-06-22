[Objective-C] This batch contains Objective-C code; pay extra attention to:

#### Typos / Dead Code
- Spelling errors in variable, method, and class names at the **declaration site** (do not report at call sites); spelling in log/error messages
- Unreachable code (after `return`), declared but unreferenced variables, large commented-out blocks

#### Error Handling
- `NSError**` out-params ignored (only the BOOL/return value checked, or vice versa); empty `@catch` swallowing the cause
- Messaging `nil` silently returns 0/nil — masks logic errors on critical paths; inserting `nil` into `NSArray`/`NSDictionary` throws

#### Error-Prone Semantics
- Pointer equality vs `isEqual:`/`isEqualToString:`; `BOOL` vs `bool`; unchecked casts and `id` used without type checks
- Out-of-bounds `objectAtIndex:`; signed/unsigned (`NSInteger`/`NSUInteger`) comparison surprises

#### Security
- Format-string injection: user input passed as the format argument to `stringWithFormat:`/`NSLog`/`predicateWithFormat:` (use `@"%@"`, not the value as format)
- Insecure transport: `http://` URLs, ATS disabled (`NSAllowsArbitraryLoads`); custom `NSURLSession` trust evaluation accepting any server cert
- SQL injection: `sqlite3` queries built with `sprintf`/`stringWithFormat:` → use bound parameters
- Sensitive data in `NSUserDefaults`/plist/logs instead of Keychain; insecure randomness (`arc4random`/`rand`) for security tokens (use `SecRandomCopyBytes`); hard-coded secrets

#### Concurrency and Resources (report only in the following cases)
- UI updates off the main thread; shared mutable state without synchronization (GCD serial queue / `@synchronized`); check-then-act races
- Retain cycles leaking memory: blocks capturing `self` strongly (use `__weak typeof(self) weakSelf`), delegate properties not `weak`/`assign`
- Core Foundation objects / file handles not released (`CFRelease`); toll-free bridging ownership wrong (`__bridge_transfer`/`__bridge_retained`)
- Do not report: local-only state, read-only access, or already-correct synchronization

#### Performance
- Expensive work (image decode, parsing, disk/network) on the main thread blocking UI; missing `@autoreleasepool` around large loops accumulating autoreleased objects
- Repeated `NSDateFormatter`/regex creation inside loops (hoist/reuse); string building with repeated `stringByAppending` in loops (use `NSMutableString`)
