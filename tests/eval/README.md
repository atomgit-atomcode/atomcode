# AtomCode Eval — Case Authoring Guide

This directory holds the case set for the batch eval harness. The runner
lives at `scripts/eval/run.sh`. The full design is at
[`docs/superpowers/specs/2026-04-07-batch-eval-harness-design.md`](../../docs/superpowers/specs/2026-04-07-batch-eval-harness-design.md).

## Quick start

```bash
# Run all cases
./scripts/eval/run.sh

# Run one case
./scripts/eval/run.sh --only 001-fizzbuzz

# Override provider for everyone
./scripts/eval/run.sh --provider kimi

# View results
open runs/<latest>/index.html
```

## Two case formats

### Form A — single file (small / no seed / inline seed)

```
tests/eval/cases/
  001-fizzbuzz.md
```

Use form A when:
- No starter files needed, OR
- Just 1-3 small text files inline

The case id must match the filename (without `.md`).

### Form B — directory (multi-file seed / real project mock)

```
tests/eval/cases/
  010-rust-refactor/
    case.md
    seed/                ← copied to cwd/ at run time
      Cargo.toml
      src/main.rs
```

Use form B when:
- You need a multi-file starter project
- Files are large or binary
- You want to edit the seed in your IDE rather than as TOML strings

The case id must match the directory name.

## Frontmatter (TOML)

Every case starts with a `+++` TOML frontmatter block:

```markdown
+++
id = "001-fizzbuzz"          # required, must match filename/dirname
provider = "kimi"            # required

description = "..."          # optional, shown in index.html
timeout_secs = 60            # optional, default 120
tags = ["code-gen", "smoke"] # optional, V1 just displays them

# Form A only — form B uses seed/ directory instead
[seed_files]
"hint.txt" = "useful hint"
"src/main.py" = """
print("placeholder")
"""
+++

your prompt body here, exactly as it would be passed to atomcode -p
```

### Field constraints

- `id`: charset `[a-zA-Z0-9_-]`, must match filename/dirname
- `seed_files` keys: relative paths only, no `..`, no absolute paths
- `seed/` (form B): no symlinks, soft 50MB limit (warning only)

## What `-p` mode can and can't do

**Works fine** (95% of cases):
- Read files (read_file, glob, grep, list_dir, ...)
- Edit existing files (edit_file, search_replace)
- Create new files (create_file on a non-existing path)
- Run normal bash: `cargo build`, `pytest`, `npm install`, `git status`,
  `python script.py`, `curl`, ...
- Multi-step verification flows ("write code, then run it")

**Will be auto-denied** (the model gets a "denied" observation and may
pivot to a workaround, but the case result is degraded):
- `rm -rf`, `rmdir` — use edit_file to write empty content instead
- `git reset --hard`, `git push --force`, `git clean -f`
- `drop table`, `drop database`
- `mkfs`, `format`, `dd if=`, `chmod 777`
- `kill -9` without a numeric PID (`kill -9 12345` is fine)

**See `crates/atomcode-core/src/tool/bash.rs:430-450`** for the full
denylist. Don't write cases whose "correct" solution requires these.

## Triage tips

When a case looks wrong, here's where to look (in order):

1. **`runs/<ts>/<case-id>/meta.json`** — exit_code, status, had_denial,
   wall_ms. If `had_denial: true`, jump straight to step 3.
2. **`runs/<ts>/<case-id>/cwd/`** — what the model actually produced.
   `diff -ru tests/eval/cases/<id>/seed/ runs/<ts>/<id>/cwd/` is great
   for form B cases.
3. **`runs/<ts>/<case-id>/stderr.txt`** — `[tool→ ...]` / `[tool← ...]`
   timeline + any `[approval-denied]` lines. Quick scan of the agent's
   tool calls in time order.
4. **`runs/<ts>/<case-id>/home/logs/*.json`** — the gold mine. Each
   pair is one LLM round-trip with full messages, tool definitions,
   token counts, step number. Open the request file for "what we sent"
   and the matching `*_response.json` for "what we got back".

## Why TOML, not YAML?

The runner is bash + python stdlib only. Python 3.11+ has `tomllib`
built in but no `yaml`. Switching to TOML avoids forcing every user
to `pip install pyyaml`. TOML's `"""..."""` covers multi-line strings
adequately for prompts and seed files. The decision is documented in
`docs/superpowers/plans/2026-04-07-batch-eval-harness.md`
under "Deviations from spec".

## V1 limitations

Things V1 deliberately does NOT do (planned for V1.5+):
- No triage badges (`long-turn`, `repeat-tool`, `token-heavy`, ...)
- No cross-run diff
- No `notes.md` annotation
- No `--rerun-failed`
- No grading (LLM-as-judge or hard assertions)
- No multi-provider matrix
- No multi-turn cases
- No `--dangerous-allow-all` flag (and never will — see CLAUDE.md §3)

When you need any of these, the spec has the design path forward.
