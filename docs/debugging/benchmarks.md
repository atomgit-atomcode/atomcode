# Benchmark harness

The benchmark harness records medians for a few stable paths. Timing results do
not gate CI. It does not prove a release is fast. It gives a same-machine before/after
number for a change.

Run it through the script:

```sh
./scripts/bench.sh
```

The script writes `target/bench/core-<sha>.json` and `.md`. It runs five
iterations by default.

```sh
BENCH_ITERATIONS=10 ./scripts/bench.sh
BENCH_OUT=target/bench/my-change ./scripts/bench.sh
```

`BENCH_OUT` is resolved from the repository root, even when `cargo bench` runs
the target from the package directory.

The harness needs no network, no model and no real terminal.

## Metrics

| Metric | What it measures |
|---|---|
| `sse_feed` | Decode 20,000 synthetic chat-completion SSE lines in 512-line chunks. |
| `file_index_first_walk` | One complete gitignore-aware `FileIndex` walk over 2,000 files. |
| `grep_cold` | First `GrepTool` run over the fixture tree. |
| `grep_warm` | Repeated `GrepTool` run with a warm page cache. |
| `read_file_window` | Read 200 lines at offset 50,000 from an 8 MiB file. The tool still reads the whole file; the metric tracks that cost. |
| `session_list` | `SessionManager::list` over 1,000 event sessions. |
| `session_resume` | `open_for_resume` over one 20,000-event session. It reads and projects the whole log. |

The fixtures are generated in a temporary directory for each run. They are not
committed.

## Process-level metrics

Set `BENCH_E2E=1` to add `--tui --demo` and `--tui --audit` timings:

```sh
BENCH_E2E=1 ./scripts/bench.sh
```

These numbers include process startup. They depend on the terminal, the OS page
cache, the CPU governor and the machine load. Compare them only on the same
machine, and only as a trend. Do not use them as a pass/fail threshold.

A non-zero exit, a spawn failure or empty stdout drops the whole process-level
metric. The harness prints the exit code and stderr. It never reports a failed
run as a fast sample.

The script always runs `cargo build -p atomcode --bin atomcode --release`
for the current working tree when E2E is requested without an explicit binary.
It selects the rustc host target and writes under
`<CARGO_TARGET_DIR or target>/<host>/release/atomcode` (`.exe` on Windows),
so an existing artifact or cross-compilation setting cannot select an old or
non-runnable default binary. Cargo still reuses unchanged build artifacts.

Set `BENCH_ATOMCODE_BIN` to time an existing binary instead; this explicitly
skips the build and enables process-level metrics. Relative paths are resolved
from the repository root. A missing executable is an error.

JSON reports include `process_binary` (path, SHA-256 and source: `working_tree`
or `external`); Markdown includes the same identity. This is captured outside
the timed region. The report's `git_sha` identifies the harness checkout, not
the commit that produced an externally supplied executable. Working-tree builds
can also include uncommitted changes; use clean checkouts for commit comparisons.

## How to read a result

1. Run the harness on the base commit and on the change commit.
2. Use the same machine, the same profile and the same iteration count.
3. Compare the medians, not one sample.
4. Keep the fixture sizes unchanged. A different fixture is a different metric.

A release build is the default for `cargo bench`. A debug build is marked in the
report. Use debug numbers only for relative comparisons.

## Design constraints

- The harness has no benchmark-framework dependency. It measures medians itself,
  so `Cargo.lock` does not change.
- `benches/core.rs` is guarded by `required-features = ["bench"]`. The normal
  `cargo check --workspace --all-targets` gate does not compile it.
- The `bench` feature turns on `provider`, `tools` and `session`. It is not part
  of any default build.
- The harness is report-only. Do not add a blocking CI threshold.
- Add a metric only when it has a fixed fixture and a result that can be
  compared on one machine.

## Adding a metric

1. Add a `bench_*` function in `crates/atomcode-capabilities/benches/core.rs`.
2. Create the fixture in a `TempTree`.
3. Call `measure(iterations, ...)` and return a `Sample`.
4. Add one row to the table above.

## Verification

`bash scripts/bench.spec.sh` checks script orchestration with stub build tools,
including paths with spaces, forced rebuilds, custom target directories and
explicit external binaries. It does not build or benchmark AtomCode.

Compile the opt-in target explicitly with
`cargo check -p atomcode-capabilities --features bench --bench core`;
the default workspace check does not enable it. The feature phase of
`bash gates/compile.sh` compiles it without running it or gating on timings.
