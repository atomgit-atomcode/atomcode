#!/usr/bin/env bash
# Script orchestration regression only: Cargo and rustc are stubs, no builds.
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d "${TMPDIR:-/tmp}/atomcode bench script.XXXXXX")"
fixture="$(cd "$fixture" && pwd)"
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/scripts" "$fixture/bin"
cp "$REPO/scripts/bench.sh" "$fixture/scripts/bench.sh"
export BENCH_TEST_LOG="$fixture/calls"
cat > "$fixture/bin/git" <<'STUB'
#!/usr/bin/env bash
printf 'fixture-sha\n'
STUB
cat > "$fixture/bin/rustc" <<'STUB'
#!/usr/bin/env bash
printf 'host: test-host\n'
STUB
cat > "$fixture/bin/cargo" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$BENCH_TEST_LOG"
mode="$1"; shift
target=''; dir=''; out=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    --target) target="$2"; shift ;;
    --target-dir) dir="$2"; shift ;;
    --out) out="$2"; shift ;;
    -p) [ "$2" != atomcode-cli ] || exit 41; shift ;;
  esac
  shift
done
if [ "$mode" = build ]; then
  mkdir -p "$dir/$target/release"
  printf '#!/bin/sh\nprintf fixture\\n\n' > "$dir/$target/release/atomcode"
  chmod +x "$dir/$target/release/atomcode"
else
  if [ "${BENCH_E2E:-0}" = 1 ] || [ -n "${BENCH_ATOMCODE_BIN:-}" ]; then
    [ -x "$BENCH_ATOMCODE_BIN" ]
    printf 'binary=%s source=%s\n' "$BENCH_ATOMCODE_BIN" "$BENCH_BINARY_SOURCE" >> "$BENCH_TEST_LOG"
  fi
  mkdir -p "$(dirname "$out")"
  printf '{}\n' > "$out.json"
  printf 'fixture\n' > "$out.md"
fi
STUB
chmod +x "$fixture/bin/"*
export PATH="$fixture/bin:$PATH"
unset BENCH_ATOMCODE_BIN BENCH_E2E BENCH_OUT BENCH_BINARY_SOURCE
bash "$fixture/scripts/bench.sh" >/dev/null
[ -f "$fixture/target/bench/core-fixture-sha.json" ]
# Existing default artifacts must still go through Cargo on every invocation.
export BENCH_E2E=1 CARGO_TARGET_DIR="$fixture/custom target"
bash "$fixture/scripts/bench.sh" >/dev/null
bash "$fixture/scripts/bench.sh" >/dev/null
[ "$(grep -c '^build ' "$BENCH_TEST_LOG")" = 2 ]
grep -Fq "binary=$CARGO_TARGET_DIR/test-host/release/atomcode source=working_tree" "$BENCH_TEST_LOG"
# An explicit external executable is not rebuilt, even outside BENCH_E2E mode.
unset BENCH_E2E
export BENCH_ATOMCODE_BIN="custom target/test-host/release/atomcode" BENCH_OUT='reports/custom'
bash "$fixture/scripts/bench.sh" >/dev/null
[ "$(grep -c '^build ' "$BENCH_TEST_LOG")" = 2 ]
grep -Fq "binary=$fixture/$BENCH_ATOMCODE_BIN source=external" "$BENCH_TEST_LOG"
[ -f "$fixture/reports/custom.json" ]
export BENCH_ATOMCODE_BIN="$fixture/missing"
if bash "$fixture/scripts/bench.sh" >"$fixture/error" 2>&1; then
  echo 'missing explicit executable was accepted' >&2; exit 1
fi
grep -q 'cannot run requested benchmark binary' "$fixture/error"
export BENCH_ATOMCODE_BIN='C:\missing.exe'
if bash "$fixture/scripts/bench.sh" >"$fixture/error" 2>&1; then
  echo 'missing Windows executable was accepted' >&2; exit 1
fi
grep -Fq 'binary: C:\missing.exe' "$fixture/error"
echo 'bench script: rebuild, package, host target, custom paths, external and missing binary checks passed'
