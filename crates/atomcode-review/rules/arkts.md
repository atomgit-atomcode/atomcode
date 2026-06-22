[ArkTS] This batch contains ArkTS (.ets) code (HarmonyOS); pay extra attention to:

#### Typos / Dead Code
- Spelling errors in component names, variable names, and function names; spelling errors in log or error messages that affect readability
- Never-executed code blocks (always-false branches, code after return), declared but unreferenced variables, large blocks of commented-out code

#### State Decorators (@State, etc.)
- Arrays/objects decorated with `@State` will not trigger UI refresh when modified via push/property change; the reference must be replaced
- Whether `@Prop` (one-way) and `@Link` (two-way) are used correctly for the scenario
- Nested object state updates must use `@Observed` + `@ObjectLink`
- Props passed through more than 3 layers should use `@Provide/@Consume` instead
- `@StorageLink/@StorageProp` should only be used for true global state; avoid abuse

#### Component Lifecycle
- Timers/listeners created in `aboutToAppear` must be released in `aboutToDisappear`
- Page-level logic should go in `onPageShow/onPageHide`, not component lifecycle hooks
- Avoid time-consuming synchronous operations in lifecycle hooks that block the UI thread

#### ArkUI Declarative Syntax
- Side effects are prohibited in the `build` method (network requests, timers, logging)
- `ForEach`/`LazyForEach` must provide a unique and stable key generation function
- Use `if/else` for conditional rendering; do not use `switch`
- Direct manipulation of component instances outside `build` is prohibited

#### Performance Optimization
- Large lists (>20 items) must use `LazyForEach` instead of `ForEach`
- Creating new objects, closures, or style-returning functions inside `build` is prohibited (causes unnecessary child component rebuilds)
- Complex calculation results should be cached via `@Watch` to avoid repeated calculations on every render
- Image resources should have a caching strategy to avoid repeated loading

#### Resource Access Standards
- Hard-coded strings are prohibited; use `$r('app.string.key')` to support internationalization
- Images must use `$r('app.media.icon')` or `$rawfile('path')`; hard-coded paths are prohibited
- Colors/dimensions should use `$r('app.color.primary')` and other resource references to support theme switching

#### Component Communication
- Parent→child: use `@Prop`/`@Link`; child→parent: use callback `onEvent` pattern
- Cross-component: use `@Provide/@Consume`; global state: use `AppStorage`
- Avoid using `AppStorage` to pass component-local state

#### General TypeScript Standards
- Disable `any` (add comments explaining why when necessary); disable `var`, use `let`/`const`
- Disable `==`/`!=`, use `===`/`!==`
- Async functions must have try-catch error handling and provide user-friendly error messages
- Prefer async/await; prohibit callback hell; use `Promise.all` for independent async operations
- Perform null checks when accessing values or destructuring to avoid null pointer exceptions

#### Security
- User input must be validated (length, format, range); prohibit direct concatenation into SQL or command strings
- Sensitive information (keys, passwords, tokens) must not be logged or uploaded
- Network requests must use HTTPS and validate certificates
