#!/usr/bin/env bash
# Report-only benchmark harness.
#
# Usage:
#   ./scripts/bench.sh                    # library metrics, 5 iterations
#   BENCH_ITERATIONS=10 ./scripts/bench.sh
#   BENCH_E2E=1 ./scripts/bench.sh        # also time `atomcode --tui --demo/--audit`
#   BENCH_OUT=target/bench/mine ./scripts/bench.sh
#
# Outputs `<base>.json` and `<base>.md` under `target/bench/` by default.
# This script never gates CI and never adds a dependency. See
# docs/debugging/benchmarks.md for what the numbers mean.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

ITERATIONS="${BENCH_ITERATIONS:-5}"
SHA="$(git rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"
BASE="${BENCH_OUT:-target/bench/core-${SHA}}"
# `cargo bench` runs the target with its cwd set to the package root
# (`crates/atomcode-capabilities`), so a relative --out would land in the
# package's target/ tree, not the repository's. Resolve it here.
case "$BASE" in
  /*|[A-Za-z]:/*) ;;
  *) BASE="$REPO/$BASE" ;;
esac

ARGS=(--iterations "$ITERATIONS" --out "$BASE")

if [ "${BENCH_E2E:-0}" = "1" ]; then
  BIN="${BENCH_ATOMCODE_BIN:-target/release/atomcode}"
  if [ ! -x "$BIN" ]; then
    echo "building $BIN for the process-level metrics (this can take a while)..."
    cargo build -p atomcode-cli --release
  fi
  if [ -x "$BIN" ]; then
    case "$BIN" in
      /*|[A-Za-z]:/*) export BENCH_ATOMCODE_BIN="$BIN" ;;
      *) export BENCH_ATOMCODE_BIN="$REPO/$BIN" ;;
    esac
    ARGS+=(--e2e)
  else
    echo "skipping --e2e: $BIN is not executable" >&2
  fi
fi

cargo bench -p atomcode-capabilities --features bench --bench core -- "${ARGS[@]}"

echo
echo "wrote ${BASE}.json and ${BASE}.md"
