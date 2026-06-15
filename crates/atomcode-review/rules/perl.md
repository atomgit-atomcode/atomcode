[Perl] This batch contains Perl code; pay extra attention to:

#### security
- Command injection: `system` / backticks / `open "| cmd"` with interpolated input; 2-arg `open` allowing pipe/`>` injection (use 3-arg `open`)
- Taint issues with external input; `eval` of untrusted strings

#### robustness
- Missing `use strict; use warnings;`; return values of `open`/`close`/`system` unchecked (`or die`)
- `$_` clobbered across nested loops / `map`; list vs scalar context surprises

#### semantics
- `==` (numeric) vs `eq` (string) confusion; autovivification creating unintended hash/array entries; `undef` warnings
