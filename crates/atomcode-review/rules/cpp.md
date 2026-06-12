[C++] This batch contains C++ code; pay extra attention to:

#### Typos / Dead Code
- Spelling errors in variable names, constant names, function names at the **declaration site** (do not report at call sites); spelling in log/exception messages that affects readability
- Unreachable code, declared but unreferenced variables, large blocks of commented-out code

#### Smart Pointers
- Prefer `std::unique_ptr` for exclusive resources, `std::shared_ptr` for shared resources
- Avoid using raw pointers to manage dynamic memory; correctly use `std::weak_ptr` to break circular references
- Example: `Widget* w = new Widget(); delete w;` (easy to forget / skipped on exception) → `auto w = std::make_unique<Widget>();`

#### RAII Principle
- Acquire resources in constructors, release in destructors; use stack objects to manage resources, avoid manual resource management

#### STL Containers and Algorithms
- Prefer STL containers over raw arrays; prefer STL algorithms (e.g., `std::transform`) over hand-written loops
- Choose the right container type and understand container performance characteristics

#### auto Keyword
- Use `auto` when types are complex; avoid abusing `auto` for simple types
- Use `auto&` / `const auto&` to avoid unnecessary copies

#### Exception Handling Completeness
- Catch specific exception types instead of `...`; do not silently ignore errors in exception handlers

#### C Interface / Return Values
- Example code, unit tests, and C API wrappers must also check return values; continuing to read output buffers after failure reads uninitialized data or masks errors
- External C interfaces must validate all pointer parameters, length parameters, and `count==0` paths; do not report only the most obvious null pointer

#### const Correctness
- Add `const` to member functions where appropriate; pass parameters by const reference
- Correct const position for pointers/references; use const member variables with caution

#### Other
- Out-of-bounds (array/container index, iterator invalidation), uninitialized variables, dangling references, returning references to local objects, integer overflow / signed-unsigned mixing
- C string/parsing: unbounded `sprintf`/`strcat`, off-by-one, returning success after truncation, `strtol` without checking `endptr`/`errno` should all be reported separately
- If multiple independent defects such as null pointer, overflow, leak, and return-value errors exist in the same function, report them separately; do not keep only the most severe one

#### Size-Type Narrowing
- Buffer sizes and lengths must use `size_t`: receiving `strlen`/`sizeof` results or a buffer-size parameter in `int`/`int32_t`/`uint32_t` silently truncates on large inputs and breaks bounds checks built on it — report the declaration site
