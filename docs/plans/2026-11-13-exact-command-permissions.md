# Exact command permission rules

Goal: allow saving Windows verbatim paths without treating their question mark as a wildcard.

Owner: capabilities parses and evaluates rules; coding proposes and persists project rules. No runtime ownership or kernel changes. Invalid rules grant nothing; deny rules and sensitive-path checks remain authoritative.

1. Add `BashExact(<JSON string>)` with literal whole-command comparison. Existing `Bash(...)` wildcard semantics remain unchanged. Exact grants never authorize a different command through whitespace normalization or prefix matching.
2. Propose the new format and validate it in the existing locked, atomic settings writer. Preserve legacy exact saves.
3. Test Windows prefixes, wildcard literals, quoted whitespace, appended commands, deny precedence and propose/save/reload. Run affected crate tests, formatting and the compile gate.

This is a new persisted rule spelling. Older readers do not interpret it as a Bash grant. No Windows filesystem containment changes are included.
