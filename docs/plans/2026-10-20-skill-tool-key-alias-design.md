# Skill tool declaration spelling

Baseline: `release/v5.2.4` at `5fce39a61`; tracked files initially clean.

The capabilities skill parser owns frontmatter decoding. Harness skill permissions
remains the sole owner of turn-scoped grants; no runtime, protocol, persistence or
permission matching changes are needed.

- Use canonical `allowed-tools` in the bundled setup skill.
- Accept the exact `allowed_tools` alias through the same list parser and validation.
- Reject repeated declarations, including mixed spellings, through the existing
  skill-load error path. Do not merge or choose between conflicting grants.
- Keep unrelated frontmatter extensible; do not warn on every unknown metadata key.
- Clarify preauthorization semantics in the reference and document the alias.

Validate both spellings and malformed/duplicate declarations, parse the real seed,
and exercise the alias through actual coding skill invocation, next-turn expiry and
static deny. Run capabilities tests with skills/setup enabled, coding permission
tests, formatting and the compile gate. Existing installed skills benefit from the
parser alias without reinstalling or rewriting user files.
