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
  /*|[A-Za-z]:/*|[A-Za-z]:\\*) ;;
  *) BASE="$REPO/$BASE" ;;
esac

ARGS=(--iterations "$ITERATIONS" --out "$BASE")

if [ "${BENCH_E2E:-0}" = "1" ] || [ -n "${BENCH_ATOMCODE_BIN:-}" ]; then
  if [ -n "${BENCH_ATOMCODE_BIN:-}" ]; then
    BIN="$BENCH_ATOMCODE_BIN"
    export BENCH_BINARY_SOURCE="external"
  else
    # Always ask Cargo to build this working tree, even if a binary exists.
    # Force the host target so a cross-compilation config cannot select a
    # non-runnable binary. An explicit target-dir keeps path discovery honest.
    HOST="$(rustc -vV | sed -n 's/^host: //p')"
    [ -n "$HOST" ] || { echo "could not determine the rustc host" >&2; exit 1; }
    BUILD_DIR="${CARGO_TARGET_DIR:-target}"
    case "$BUILD_DIR" in
      /*|[A-Za-z]:/*|[A-Za-z]:\\*) ;;
      *) BUILD_DIR="$REPO/$BUILD_DIR" ;;
    esac
    cargo build -p atomcode --bin atomcode --release --target "$HOST" --target-dir "$BUILD_DIR"
    BIN="$BUILD_DIR/$HOST/release/atomcode"
    case "$HOST" in *-windows-*) BIN="$BIN.exe" ;; esac
    export BENCH_BINARY_SOURCE="working_tree"
  fi
  case "$BIN" in
    /*|[A-Za-z]:/*|[A-Za-z]:\\*) ;;
    *) BIN="$REPO/$BIN" ;;
  esac
  if [ ! -x "$BIN" ]; then
    echo "cannot run requested benchmark binary: $BIN" >&2
    exit 1
  fi
  export BENCH_ATOMCODE_BIN="$BIN"
  ARGS+=(--e2e)
fi

cargo bench -p atomcode-capabilities --features bench --bench core -- "${ARGS[@]}"

echo
echo "wrote ${BASE}.json and ${BASE}.md"
