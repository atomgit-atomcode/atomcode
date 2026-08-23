# DeepSeek V4 Flash Evaluation Harness Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a reproducible paired-concurrent harness that compares the two configured DeepSeek V4 Flash selections through AtomCode and uses Codex for blind judging and the final report.

**Architecture:** Add a repository-local, standard-library Python harness under `evals/deepseek-v4-flash/`. It launches existing AtomCode headless processes in isolated homes and fixtures, persists auditable JSON artifacts, runs case verifiers, anonymizes candidates, invokes `codex exec` for structured judgments, and aggregates results without modifying runtime ownership.

**Tech Stack:** Python 3.7 standard library, JSON, AtomCode headless CLI, Codex CLI, JSONL, Markdown.

---

### Task 1: Define the suite and artifact contracts

**Files:**
- Create: `evals/deepseek-v4-flash/benchmark.json`
- Create: `evals/deepseek-v4-flash/cases/example-model/case.json`
- Create: `evals/deepseek-v4-flash/cases/example-model/prompt.md`
- Create: `evals/deepseek-v4-flash/README.md`

1. Define both immutable selection IDs and expected wire model names.
2. Define timeout, pair concurrency, repetitions, random seed, AtomCode/Codex paths,
   and artifact-retention settings.
3. Define a case schema with tier, prompt, fixture, verifier, timeout, and rubric.
4. Document credential handling, isolation requirements, and the four commands.
5. Validate JSON loading with `python3 -m unittest discover -s evals/deepseek-v4-flash/tests -v`.

### Task 2: Implement config validation and preparation

**Files:**
- Create: `evals/deepseek-v4-flash/eval.py`
- Create: `evals/deepseek-v4-flash/tests/test_eval.py`

1. Write failing tests for suite parsing, case discovery, unsafe paths, and stable
   case/config fingerprints.
2. Implement typed dataclasses and strict validation using only the Python standard
   library.
3. Implement `prepare` to create a run manifest, randomized pair schedule,
   candidate aliases, isolated directories, and immutable case copies.
4. Ensure manifest data contains no credential values.
5. Run the unit tests and confirm all preparation tests pass.

### Task 3: Implement paired concurrent execution

**Files:**
- Modify: `evals/deepseek-v4-flash/eval.py`
- Modify: `evals/deepseek-v4-flash/tests/test_eval.py`

1. Write a fake AtomCode executable that records arguments/environment and returns
   configurable success, delay, stderr, and failure outcomes.
2. Write failing tests proving both candidates launch concurrently, receive distinct
   `ATOMCODE_HOME`/working directories, survive peer failure, and time out cleanly.
3. Implement `run` with an asyncio pair barrier and a semaphore over pairs.
4. Invoke AtomCode with `--provider`, `--config`, `--prompt-file`, `-C`, `--verbose`,
   `--dev`, and `--no-telemetry`; add `-y` only for explicitly trusted agent fixtures.
5. Persist stdout, scrubbed stderr, exit status, monotonic duration, start skew,
   hashes, and classified terminal outcome atomically per run.
6. Run unit tests with the fake executable.

### Task 4: Verify and anonymize artifacts

**Files:**
- Modify: `evals/deepseek-v4-flash/eval.py`
- Modify: `evals/deepseek-v4-flash/tests/test_eval.py`
- Create: `evals/deepseek-v4-flash/prompts/codex-judge.md`

1. Write failing tests for verifier pass/fail/timeout, secret scrubbing, randomized
   A/B mapping, and absence of provider identifiers in judge packets.
2. Run each verifier in its fixture with a bounded timeout and store stdout/stderr.
3. Generate judge packets containing case, rubric, candidate outputs/diffs, and
   machine-verification evidence under randomized A/B names.
4. Reject packets that contain configured selection, account, endpoint, or wire
   model identifiers.
5. Run all unit tests.

### Task 5: Invoke Codex and validate judgments

**Files:**
- Modify: `evals/deepseek-v4-flash/eval.py`
- Modify: `evals/deepseek-v4-flash/tests/test_eval.py`
- Modify: `evals/deepseek-v4-flash/prompts/codex-judge.md`

1. Add fake-Codex tests for valid JSON, fenced JSON, invalid output, non-zero exit,
   timeout, and retry/review classification.
2. Implement `judge` using prompt-on-stdin with `codex exec --json --cd <run> -o
   <file>` and a configurable fixed Codex model.
3. Validate winner, dimension ranges, evidence, critical failures, and confidence;
   never silently coerce malformed scores.
4. Persist raw Codex events separately from validated judgment JSON.
5. Run unit tests.

### Task 6: Aggregate and generate the final report

**Files:**
- Modify: `evals/deepseek-v4-flash/eval.py`
- Create: `evals/deepseek-v4-flash/prompts/codex-report.md`
- Modify: `evals/deepseek-v4-flash/tests/test_eval.py`

1. Write tests for paired differences, percentile calculation, success/error
   distributions, score variance, and deterministic bootstrap confidence intervals.
2. Implement `summarize` to create a provider-aware `summary.json` only after blind
   judgments are finalized.
3. Implement `report` to give Codex summary data and bounded evidence, validate that
   required sections exist, and write `report.md`.
4. Clearly separate observed facts from inferred gateway explanations.
5. Run unit tests and a complete fake-binary smoke test.

### Task 7: Seed the quick suite and document real execution

**Files:**
- Create/modify: `evals/deepseek-v4-flash/cases/**`
- Modify: `evals/deepseek-v4-flash/README.md`

1. Add 20 model cases and eight agent cases with deterministic verifiers.
2. Pin every repository fixture to an explicit commit or content hash.
3. Document provider-resolution preflight and how to inspect actual model identity.
4. Run `prepare`, then a one-case dry run without network credentials.
5. Inspect `git diff --check` and the complete diff; do not include result artifacts.
