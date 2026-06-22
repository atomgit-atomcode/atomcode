[Shell] This batch contains Shell scripts; pay extra attention to:

#### Security
- Command injection: external input拼接进命令, `eval` abuse, unquoted variables entering command position
- Dangerous operation paths not validated: `rm -rf "$DIR/"` when `$DIR` is empty will delete root; same for mv/cp

#### Robustness
- Unquoted variables causing word splitting / glob expansion (`$var` should be `"$var"`)
- Missing `set -euo pipefail`, errors silently swallowed, undefined variables treated as empty strings
- Pipeline exit codes lost (use `set -o pipefail` or check `PIPESTATUS` when needed)
- `cd` failure not handled before subsequent dangerous commands continue
- Unquoted variables in `[ ]` tests causing syntax errors, `==` vs `=` misuse

#### Other
- Temporary files using fixed names / predictable paths (should use `mktemp`); unquoted command substitutions
- Exit codes not propagated (confusion between `return` inside functions and `exit` in scripts)
