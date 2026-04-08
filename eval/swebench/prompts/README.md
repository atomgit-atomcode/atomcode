# SWE-bench prompt templates

Prompt templates for the predict phase. Selected via
`manifest.toml` → `[predict] prompt_template = "<name>"`.

The rendered prompt is also recorded in each instance's
`meta.json` under `swebench.prompt_template`, so different runs are
comparable and you can A/B different templates without losing trace.

## How templates are rendered

`render_prompt.py` loads `<name>.md` and calls `str.format(**ctx)`
where `ctx` contains the placeholders below. Missing placeholders
raise `KeyError` (deliberate — fail loud, don't ship broken prompts).

## Available placeholders

| Name | Value | Example |
|---|---|---|
| `{repo}` | `<owner>/<name>` from dataset | `"sympy/sympy"` |
| `{base_commit}` | Full SHA | `"cffd4e0f86fefd4802349a9f9b19ed70934ea354"` |
| `{base_commit_short}` | First 8 chars | `"cffd4e0f"` |
| `{instance_id}` | `<owner>__<name>-<num>` | `"sympy__sympy-20590"` |
| `{problem_statement}` | Issue body (markdown, may contain code blocks) | _varies_ |
| `{hints_text}` | Raw `hints_text` field (may be empty) | _varies_ |
| `{hints_block}` | Conditionally rendered hints section (see below) | _varies_ |

## `{hints_block}` conditional rendering

When `manifest.toml` → `[predict] include_hints = true` AND the
instance has non-empty `hints_text`, `{hints_block}` expands to:

```
\n\n--- HINTS (developer comments from the original PR) ---\n<hints_text>\n--- END HINTS ---
```

Otherwise it's the empty string. This lets templates reference
`{hints_block}` unconditionally without worrying about empty hints
producing dangling section headers.

**Default:** `include_hints = false`. SWE-bench Verified hints often
contain spoilers like "the bug is in foo.py line 42". Not ethical
for benchmarking. Override only when running exploratory experiments.

## Adding a new template

1. Copy `default.md` to `<name>.md`
2. Edit the body
3. Set `manifest.toml` → `prompt_template = "<name>"`
4. Run a 20-instance pilot first: `./run.sh --limit 20 --prompt <name>`
5. Compare to the default run via `eval/runs/<ts>/summary.json` dual-score numbers

Templates currently shipped:

- `default.md` — V1 baseline (minimal exploration guidance, surgical fix rules)

## Red flags (don't ship these)

- **Test file references**: any `test_*.py` paths in the prompt may
  cue the model to modify tests = automatic grader failure.
- **Hints leak**: prompts that repeat `hints_text` verbatim without the
  `include_hints` gate.
- **Repo tree dumps**: embedding a `tree` of the whole repo blows context
  budget; prefer letting the agent explore with list_dir.
- **Budget hints**: telling the agent "you have N turns" changes behavior
  (hurries to edit), making runs non-comparable. Let --max-turns enforce
  the cap silently.
