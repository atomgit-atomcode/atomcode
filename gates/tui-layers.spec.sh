#!/usr/bin/env bash
# project-local: gates/tui-layers.sh 的阴性对照。
#
# 一个只在合格对象上跑绿的判据没有鉴别力。这里对每一条规则各造一个违规
# fixture，断言闸门判红；再造一个合规的，断言判绿。fixture 全部密封在
# mktemp 里，不依赖真实仓库的任何状态。
set -uo pipefail
cd "$(dirname "$0")/.."
GATE="$PWD/gates/tui-layers.sh"
fail=0

run() {                                   # run <fixture-dir> → 退出码
  local dir="$1"
  TUI_SRC="$dir/src" TUI_BASELINE="$dir/baseline" bash "$GATE" >/dev/null 2>&1
}

fixture() {                               # fixture → 一个合规的最小 src 树
  local d; d=$(mktemp -d)
  mkdir -p "$d/src"
  for f in el frame width ansi caps surface; do
    printf '// clean\npub fn nothing() {}\n' > "$d/src/$f.rs"
  done
  printf '// upper layer\npub fn draw() {}\n' > "$d/src/modules.rs"
  echo "$d"
}

check() {                                 # check <名字> <期望 pass|fail> <dir>
  local name="$1" want="$2" dir="$3"
  run "$dir"; local code=$?
  if [ "$want" = pass ] && [ $code = 0 ]; then echo "  ✓ $name"; return 0; fi
  if [ "$want" = fail ] && [ $code != 0 ]; then echo "  ✓ ${name}（如期判红）"; return 0; fi
  echo "  ✗ ${name}：期望 ${want}，实际退出码 $code"
  fail=1
}

echo "=== 正向：合规的必须判绿 ==="
d=$(fixture); check "干净的树" pass "$d"

echo "=== 阴性对照：每种违规都必须判红 ==="

d=$(fixture); printf 'use atomcode_harness::session::SessionEvent;\n' >> "$d/src/el.rs"
check "原语层引用领域类型" fail "$d"

d=$(fixture); printf 'fn f() { let _ = std::env::var("X"); }\n' >> "$d/src/frame.rs"
check "原语层读环境" fail "$d"

# 字面装饰符与 OS 探测走棘轮：先建立基线，再越过它。
d=$(fixture); run "$d" >/dev/null
printf 'const B: &str = "\xe2\x94\x8c\xe2\x94\x80\xe2\x94\x90";\n' >> "$d/src/modules.rs"
check "上层出现字面制表符（超基线）" fail "$d"

d=$(fixture); run "$d" >/dev/null
printf 'fn f() -> bool { cfg!(target_os = "windows") }\n' >> "$d/src/modules.rs"
check "上层探测操作系统（超基线）" fail "$d"

d=$(fixture); run "$d" >/dev/null
printf 'fn f() { let _ = std::env::var("TERM"); }\n' >> "$d/src/modules.rs"
check "上层读 TERM（超基线）" fail "$d"

echo "=== 校准：棘轮允许存量债带着基线上线 ==="
d=$(fixture)
printf 'const B: &str = "\xe2\x94\x8c";\n' >> "$d/src/modules.rs"
check "首跑冻结存量债并通过" pass "$d"
check "同样的债再跑一次仍通过" pass "$d"

echo "=== 校准：屏蔽层自己不受这两条约束 ==="
d=$(fixture)
printf 'const B: &str = "\xe2\x94\x8c";\nfn f() { let _ = std::env::var("TERM"); }\n' >> "$d/src/caps.rs"
check "caps.rs 可以有装饰符和环境读取" pass "$d"

if [ $fail = 0 ]; then echo -e "\n分层闸门的阴性对照：通过"; else echo -e "\n分层闸门的阴性对照：未通过"; fi
exit $fail
