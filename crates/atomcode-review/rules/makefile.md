[Makefile] This batch contains Makefile(s); pay extra attention to:

#### correctness
- Tab vs spaces in recipes (spaces break the rule); missing `.PHONY` for non-file targets causing stale skips
- Each recipe line runs in a separate shell unless `.ONESHELL`; `$` vs `$$` when referencing shell variables
- Missing prerequisites causing incorrect incremental builds; `=` (recursive) vs `:=` (simple) expansion surprises

#### robustness
- Commands not failing the build on error (exit code not propagated); `rm -rf $(VAR)` where `VAR` may be empty
