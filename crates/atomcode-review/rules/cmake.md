[CMake] This batch contains CMake files; pay extra attention to:

#### correctness
- Missing `cmake_minimum_required` / project version; unquoted variables that may contain spaces or be empty (arg-count bugs)
- `file(GLOB ...)` for sources (misses new files, breaks incremental builds); hardcoded absolute paths
- Global `include_directories` / `add_definitions` where `target_*` scoped commands are preferred

#### robustness
- Missing dependency/version checks (`find_package(... REQUIRED)`); build-type-specific flags missing; cache-variable misuse
