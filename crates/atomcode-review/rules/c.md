[C] This batch contains C code; pay extra attention to:

#### Typos / Dead Code
- Spelling errors in variable names, constant names, and function names at the **declaration site** (do not report at call sites); spelling in log messages that affects readability
- Unreachable code, declared but unreferenced variables

#### malloc/free Pairing
- Every `malloc()` has a corresponding `free()`; do not free the same block twice; set pointer to NULL after free
- Example: `char* buf = malloc(1024);` (forgot to free after use) → add `free(buf); buf = NULL;` after use, and check `malloc` return value for NULL

#### Memory Leaks
- All allocated memory is freed before function exit; also free on error handling paths

#### Buffer Overflow Protection
- Check bounds before array access; use safe functions for string operations; ensure loop boundary conditions are correct
- Example: `strcpy(buffer, user_input)` may overflow → `strncpy(buffer, user_input, sizeof(buffer)-1); buffer[sizeof(buffer)-1]='\0';`

#### Safe String Functions
- Use `strncpy` instead of `strcpy`, `strncat` instead of `strcat`, `snprintf` instead of `sprintf`
- Do not return success after truncation with only a warning logged; callers need an error code to judge
- `strtol` / `strtoul` must check `endptr` and `errno`, otherwise you cannot distinguish valid 0 from parse failure

#### Other
- Null pointer dereference, uninitialized pointer, dangling pointer, array out-of-bounds, off-by-one
- Integer overflow / signed overflow / implicit truncation; unchecked system call / library function return values
- Example code and unit tests must also check return values; continuing to read output buffers after failure causes uninitialized reads or misleads the user
- If multiple independent defects such as null pointer, overflow, leak, and return-value errors exist in the same function, report them separately; do not keep only the most severe one

#### Naming Conventions
- snake_case naming, meaningful variable names, constants in UPPER_CASE

#### Size-Type Narrowing
- Buffer sizes and lengths must use `size_t`: receiving `strlen`/`sizeof` results or a buffer-size parameter in `int`/`int32_t`/`uint32_t` silently truncates on large inputs and breaks bounds checks built on it — report the declaration site
