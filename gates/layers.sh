#!/usr/bin/env bash
# Which crate may depend on which (`docs/architecture-target.md` §2, §9).
#
# The five parts have one rule each, and the rules are about *direction*:
#
#   Agent 机制 (kernel, plexus, harness)  no atomcode dependency at all
#   Agent 能力行 (capabilities, review)    may depend on the mechanism
#   Product (coding, today)                mechanism + rows; knows no front end
#   Host (cli, daemon)                     may depend on everything
#   UI (tui)                               the mechanism's seams and protocols;
#                                          knows neither Host nor Product
#
# Written because the directions are invisible at the call site: adding
# `atomcode-host-api` to `atomcode-coding` compiled, ran, and passed every test —
# it was wrong only against a rule nothing enforced. It is checked here rather
# than by reading, because a rule nobody can run is a rule that drifts.
#
# Deliberately NOT a `cargo metadata` graph walk: the direct edges in each
# Cargo.toml are the thing being judged. A transitive edge is not a violation —
# `cli` pulling `kernel` through `coding` is fine.
set -uo pipefail
cd "$(dirname "$0")/.."

fail=0
say() { printf '\033[2m→\033[0m %s\n' "$1"; }
ok() { printf '\033[32m  ok\033[0m   %s\n' "$1"; }
bad() {
  printf '\033[31m  FAIL\033[0m %s\n' "$1"
  printf '         %s\n' "$2"
  fail=1
}

# The direct atomcode dependencies of a crate, one per line.
deps() {
  local manifest="crates/$1/Cargo.toml"
  [ -f "$manifest" ] || { echo "__no_such_crate__"; return; }
  # Dependency tables only: `[features]` names crates too (`atomgit =
  # ["atomcode-capabilities/atomgit"]`), and a feature is not an edge.
  awk '
    /^\[(dependencies|dev-dependencies|build-dependencies|target\..*dependencies)\]/ { on = 1; next }
    /^\[/ { on = 0 }
    on && /^atomcode-[a-z-]+ *=/ { sub(/ *=.*/, ""); print }
  ' "$manifest" | sort -u
}

# `$1` must not depend on any of `$2...`.
forbid() {
  local who="$1"; shift
  local found=""
  local have
  have="$(deps "$who")"
  for banned in "$@"; do
    if printf '%s\n' "$have" | grep -qx "$banned"; then
      found="$found $banned"
    fi
  done
  [ -z "$found" ] && return 0
  echo "$found"
  return 1
}

say "Agent 机制:零 atomcode 内部依赖"
mechanism_bad=0
for crate in atomcode-kernel atomcode-plexus; do
  have="$(deps "$crate" | grep -v '^__' || true)"
  if [ -n "$have" ]; then
    bad "Agent 机制:零 atomcode 内部依赖" \
      "$crate 依赖了:$(echo $have) —— 机制不许依赖任何 atomcode crate(§2.1)"
    mechanism_bad=1
  fi
done
[ "$mechanism_bad" = 0 ] && ok "Agent 机制:零 atomcode 内部依赖"

# `harness` is the third crate of the mechanism and today it breaks the same
# rule: 18 files reach into `atomcode-capabilities` (§9 lists it among the known
# mixed layers, "harness 含 UI 行与 launch"). Recorded as a debt with a number
# rather than asserted, so it can only shrink — the same shape as the criterion
# ratchet.
#
# What the number counts is every direct atomcode edge, `plexus` and `kernel`
# included — they are the mechanism itself, so the floor is 2 rather than 0.
# 2026-09-18: 6 → 5, `host.rs` moved out to `atomcode-tree-host` because a host
# answering the front-end contracts is not the mechanism's job.
say "harness 欠的混层债只能变小"
harness_debt="$(deps atomcode-harness | grep -v '^__' | wc -l | tr -d ' ')"
harness_budget="$(cat gates/harness-layer.baseline 2>/dev/null || echo 99)"
if [ "$harness_debt" -gt "$harness_budget" ]; then
  bad "harness 欠的混层债只能变小" \
    "harness 现在有 $harness_debt 条 atomcode 依赖,基线是 $harness_budget —— 机制层该是 0(§2.1)"
else
  ok "harness 欠的混层债只能变小($harness_debt ≤ $harness_budget)"
fi

say "UI 不认 Host,也不认 Product"
if out="$(forbid atomcode-tui atomcode-coding atomcode)"; then
  ok "UI 不认 Host,也不认 Product"
else
  bad "UI 不认 Host,也不认 Product" \
    "tui 依赖了:$out —— 屏幕不该认识宿主或产品(§2.5)"
fi

say "Product 不认前端,也不认宿主"
if out="$(forbid atomcode-coding atomcode-host-api atomcode-tui atomcode)"; then
  ok "Product 不认前端,也不认宿主"
else
  bad "Product 不认前端,也不认宿主" \
    "coding 依赖了:$out —— 前端契约与宿主装配都在它上面(§2.3)"
fi

say "契约零实现:host-api 只依赖 kernel"
have="$(deps atomcode-host-api | grep -v '^__' || true)"
if [ "$have" = "atomcode-kernel" ]; then
  ok "契约零实现:host-api 只依赖 kernel"
else
  bad "契约零实现:host-api 只依赖 kernel" \
    "host-api 依赖了:$(echo $have) —— 契约一旦依赖它的某个实现,就不再是契约"
fi

if [ "$fail" = 0 ]; then
  printf '\033[32m\n分层闸门: 通过\033[0m\n'
else
  printf '\033[31m\n分层闸门: 失败\033[0m\n'
fi
exit "$fail"
