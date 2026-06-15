[Groovy] This batch contains Groovy scripts; pay extra attention to:

#### dynamic typing pitfalls
- `def` / dynamic typing hiding type errors until runtime; missing safe-navigation `?.`
- Truthiness surprises (empty string, `0`, empty collection are all falsy)

#### security
- Command/shell execution with interpolated external input; `Eval` / `GroovyShell` on untrusted input
- GString interpolation into SQL / commands (injection)

#### robustness
- Swallowed exceptions in catch blocks; closures capturing mutable state; resource leaks (streams/connections not closed)
- Implicit return of the last expression returning an unexpected value
