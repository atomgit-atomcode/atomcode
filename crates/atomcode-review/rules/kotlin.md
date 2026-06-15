[Kotlin] This batch contains Kotlin code; pay extra attention to:

#### Null Safety
- Avoid abusing `!!` non-null assertions (may cause NPE); prefer safe call `?.` or Elvis `?:`
- Are nullable properties in data classes / API responses handled correctly
- Example: `text!!.length` is risky → `text?.length ?: 0`

#### Dead Code
- Unreachable branches, declared but unreferenced variables, large blocks of commented-out code

#### Function and Expression Conciseness
- Use `=` for single-expression functions; replace complex if-else chains with `when`
- Avoid unnecessary `return` (use expression results directly in lambdas)

#### Collection Operation Optimization
- Prefer `Sequence` for large collections (lazy evaluation, reduces intermediate objects)
- Merge multiple `filter`/`map` calls; use `groupBy`/`associate` instead of hand-written iteration

#### Coroutines
- Use structured concurrency (`coroutineScope`/`supervisorScope` to manage lifecycle); avoid `GlobalScope` (prone to leaks)
- Exception handling: wrap `withContext`/`async` in try/catch

#### Class and Object Design
- Use `data class` for pure data objects (auto-generates equals/hashCode)
- Use `sealed class` for restricted type hierarchies (exhaustive when checks)
- Delegation: `by lazy` property delegation, `by` class delegation

#### Resource Management and Scope Functions
- Use `use` for files/network resources to auto-close
- Keep scope functions (`let`/`apply`, etc.) readable; avoid excessive nesting

#### Performance Pitfalls
- Use `inline` for higher-order functions to reduce lambda overhead (but don't inline large functions)
- Use `const val` for compile-time constants
- Avoid creating objects inside loops (e.g., Regex instances)

#### Interoperability (with Java)
- Use `@JvmStatic`/`@JvmOverloads` for APIs exposed to Java
- Use `@Nullable`/`@NonNull` to help Java recognize nullability

#### Other
- Prefer `val` over `var`; use string templates (`"Value: $value"`) instead of concatenation
