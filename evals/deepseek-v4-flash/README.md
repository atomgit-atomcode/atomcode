# DeepSeek V4 Flash paired evaluation

This harness compares the AtomGit and Volcano Engine DeepSeek V4 Flash model
profiles through separate AtomCode headless runtimes. Candidate runs in a pair
start concurrently and never share a writable session or fixture. Runs use
`--ephemeral --output-format jsonl`; model-tier cases additionally use `--no-tools`.

Requirements: Python 3.7+, a built/installed `atomcode`, `codex`, and an existing
AtomCode config containing both selection IDs in `benchmark.json`. Credentials
remain in the normal AtomCode auth/config stores; this directory never copies them
into result artifacts.

```bash
cd evals/deepseek-v4-flash
python3 eval.py prepare
python3 eval.py run --run-dir results/<run-id>
python3 eval.py judge --run-dir results/<run-id>
python3 eval.py report --run-dir results/<run-id>
```

Use `--case smoke-model --repetitions 1` for a cheap preflight. `run` adds
`--dangerously-skip-permissions` only for cases declaring `allow_edits = true`;
use only disposable fixtures for those cases. Set `atomcode_bin` to
`../../target/debug/atomcode` when evaluating the current checkout.

Case directories contain `case.json` and `prompt.md`. Optional `fixture` is copied
for every candidate/repetition. Optional `verify` is an argument array, avoiding
shell interpolation. Results contain reconstructed assistant output, scrubbed
stderr, metadata, verification output, anonymous Codex packets, judgments,
`summary.json`, and the final `report.md`. `events.jsonl` is the authoritative
AtomCode event stream. Do not commit `results/`.
