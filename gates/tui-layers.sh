#!/usr/bin/env bash
# project-local: 守 atomcode-tui 的分层，只对本项目有意义，不回流上游。
#
# 分层是一张图，除非有机器守着它。这里守两个方向：
#
#   第一层（绘制原语）不得长出领域知识 —— 一旦 El 里出现 SessionEvent，
#   原语层就不再能被别的产品复用，而这件事发生时没有任何编译错误。
#
#   第二层以上不得直接碰操作系统 —— 一个字面的 ┌ 或一次 TERM 读取，
#   意味着屏蔽层被绕过了，而绕过的后果只在别人的终端上可见。
#
# 存量债走棘轮：首跑冻结当前计数，之后只能降不能升。首跑即红的闸门会被关掉。
set -uo pipefail
cd "$(dirname "$0")/.."

# 可被阴性对照重定向到密封的 fixture：一个只能对真实仓库运行的闸门，
# 无法被证明它会判红。
SRC=${TUI_SRC:-crates/atomcode-tui/src}
BASE=${TUI_BASELINE:-gates/tui-layers.baseline}
fail=0

# 第一层的成员。caps.rs 是探测器本身，允许读环境；surface.rs 是 I/O 屏蔽，
# 允许碰操作系统。其余原语两样都不许。
PURE="$SRC/el.rs $SRC/frame.rs $SRC/width.rs $SRC/ansi.rs"
SHIELD="$SRC/caps.rs $SRC/surface.rs"

ratchet() {                      # ratchet <name> <count> <what>
  local name="$1" count="$2" what="$3" base
  [ -f "$BASE" ] && base=$(grep "^$name=" "$BASE" 2>/dev/null | cut -d= -f2)
  if [ -z "${base:-}" ]; then
    printf '%s=%s\n' "$name" "$count" >> "$BASE"
    echo "  ⊙ $name: 建立基线 ${count}（${what}）"
    return 0
  fi
  if [ "$count" -gt "$base" ]; then
    echo "  ✗ $name: $count 超过基线 $base —— $what"
    return 1
  fi
  if [ "$count" -lt "$base" ]; then
    local t; t=$(mktemp); grep -v "^$name=" "$BASE" > "$t"; printf '%s=%s\n' "$name" "$count" >> "$t"; mv "$t" "$BASE"
    echo "  ✓ $name: ${count}（基线从 $base 降到 ${count}）"
    return 0
  fi
  echo "  ✓ $name: ${count}（持平基线）"
}

echo "→ 第一层不得含领域词汇"
n=$(grep -nE "SessionEvent|atomcode_harness|atomcode_kernel" $PURE 2>/dev/null | wc -l | tr -d ' ')
if [ "$n" != "0" ]; then
  grep -nE "SessionEvent|atomcode_harness|atomcode_kernel" $PURE
  echo "  ✗ 绘制原语引用了领域类型：原语层必须能被另一个产品原样拿走"
  fail=1
else
  echo "  ✓ 干净"
fi

echo "→ 第一层（caps 除外）不得读环境"
n=$(grep -nE "std::env|env::var" $PURE 2>/dev/null | wc -l | tr -d ' ')
if [ "$n" != "0" ]; then
  grep -nE "std::env|env::var" $PURE
  echo "  ✗ 能力必须注入而非探测（同 docs/adr/0008：探测会让判据在错误的机器上永远绿）"
  fail=1
else
  echo "  ✓ 干净"
fi

# 上层：屏蔽层之外不得出现字面装饰字符或操作系统探测。
UPPER=$(find "$SRC" -name '*.rs' ! -name caps.rs ! -name surface.rs)

echo "→ 屏蔽层之外不得出现字面装饰字符（应走 Caps::g(Glyph::…)）"
n=$(grep -oE '[┌┐└┘─│├┤┬┴┼✓✗⋯▸•]' $UPPER 2>/dev/null | wc -l | tr -d ' ')
ratchet literal_glyphs "$n" "字面制表符/状态符，ASCII 终端上会 tofu 且宽度可能错" || fail=1

echo "→ 屏蔽层之外不得探测操作系统或终端"
n=$(grep -nE 'cfg!\(target_os|"TERM"|"LANG"|"LC_ALL"|ATOMCODE_ASCII|"NO_COLOR"|"WT_SESSION"' $UPPER 2>/dev/null | wc -l | tr -d ' ')
ratchet os_probes "$n" "直接探测终端/操作系统，绕过了 Caps" || fail=1

if [ $fail = 0 ]; then echo -e "\n分层闸门：通过"; else echo -e "\n分层闸门：未通过"; fi
exit $fail
