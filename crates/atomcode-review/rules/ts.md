[JS/TS] This batch contains JavaScript/TypeScript code; pay extra attention to:

#### Typos / Dead Code
- Spelling errors in variable names, function names, component names, and Props property names; spelling errors in log/error messages that affect readability
- Never-executed code blocks (always-false branches, code after return), declared but unreferenced variables, large blocks of commented-out code

#### Code Quality
- **Duplicate code**: extractable common logic
- **Hard-coding**: business-related hard-coded strings are prohibited, especially URL paths and business IDs; simple UI copy can be relaxed
- **Variable declarations**: disable `var`, use `let`/`const`
- **Equality comparison**: disable `==`/`!=`, use strict equality `===`/`!==`
- **TS types**: avoid `any`, add comments explaining why when necessary
- **Null checks**: perform null checks when accessing values or destructuring to avoid null pointer exceptions
- **Ternary expressions**: prohibit nested ternaries

#### React Best Practices
- **Hooks rules**: only call at top level, only call inside React functions
- **State management**: place state at the appropriate level, avoid unnecessary state lifting
- **Side effects**: correctly handle dependencies and cleanup in useEffect
- **Performance optimization**: use React.memo/useMemo/useCallback reasonably (based on profiling, avoid premature optimization)
- **Render side effects**: strictly prohibit side effects during render (API calls, DOM operations)
- **Inline styles**: avoid inline `style`, except for dynamic styles
- **Inner components**: do not declare new components inside a component; use render methods instead (e.g., `renderItem`, not `<Item/>`)

#### Frontend State / Lifecycle
- When a component unmounts, clean up `setInterval` / `setTimeout` / `requestAnimationFrame` / DOM event listeners / store subscriptions to avoid leaks or repeated execution
- Report when an overlay/panel continues to run in rAF or polling loops after closing, causing sustained CPU consumption
- Shallow copy of state followed by in-place modification of inner arrays/objects (e.g., `{...state}` then `push/shift`) breaks immutable updates and old-state isolation
- Reactive fields in Lit/Vue/React not declared as reactive/state but modified in-place with `push`/etc. may not trigger re-rendering
- Stop functions such as `stopPolling` / `dispose` / `disconnect` must reset timer/subscription handles to support subsequent restarts

#### Async Handling
- **Error handling**: async functions must have error handling and provide user-friendly error messages
- **Prefer async/await**: prefer async/await over Promise, prohibit callback hell
- **Async in loops**: distinguish independent async (use `Promise.all` in parallel) from dependent async (sequential), prefer `Promise.all`
- Promises for writing storage, importing data, batch processing updates must be awaited; do not prematurely resolve causing callers to see false success

#### Security
- **XSS protection**: user input must be properly escaped
- **innerHTML safety**: prohibit using innerHTML to directly insert user input; use textContent or perform XSS protection
- **Code injection**: strictly prohibit `eval()`, `Function()` constructor, string-form setTimeout/setInterval
- **Dangerous methods**: disable `document.write()`
- **Sensitive information**: check whether API keys / sensitive data are exposed
- **Prototype chain safety**: prohibit modifying native object prototypes (Array.prototype, Object.prototype)
