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

# 只读代码,不读散文。
#
# 起因（真实）：这条闸门数的是「上层出现了几个字面装饰符」,而它 grep 整个文件——
# 于是一条引用界面文案的文档注释（`/// 一回合的结尾是 ───── ✓ 完成 ─────`）和一条
# 断言画出来长什么样的测试（`assert!(line.starts_with("[•]"))`）都被算成违规。
# 写文档把闸门弄红一次,这条棘轮的寿命就以天计——仓库自己写过：首跑即红的闸门会被
# 关掉。规则说的是代码不许绕过屏蔽层,那它就该只看代码。
#
# 剥两样:整行注释,以及文件末尾的 `mod tests`。行尾注释不剥——`//` 出现在字符串
# 字面量里是常事,剥了会把真代码剪断,而一个漏判远好过一个假阳性。
code_only() {                    # code_only <files...> → 只剩代码的文本
  local f
  for f in "$@"; do
    awk '/^mod [a-z_]*tests?[[:space:]]*\{/ { exit } { print }' "$f" \
      | grep -vE '^[[:space:]]*(//|/\*|\*)'
  done
}

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
n=$(code_only $PURE | grep -cE "SessionEvent|atomcode_harness|atomcode_kernel")
if [ "$n" != "0" ]; then
  grep -nE "SessionEvent|atomcode_harness|atomcode_kernel" $PURE
  echo "  ✗ 绘制原语引用了领域类型：原语层必须能被另一个产品原样拿走"
  fail=1
else
  echo "  ✓ 干净"
fi

echo "→ 第一层（caps 除外）不得读环境"
n=$(code_only $PURE | grep -cE "std::env|env::var")
if [ "$n" != "0" ]; then
  grep -nE "std::env|env::var" $PURE
  echo "  ✗ 能力必须注入而非探测（同 docs/adr/0008：探测会让判据在错误的机器上永远绿）"
  fail=1
else
  echo "  ✓ 干净"
fi

# 上层：屏蔽层之外不得出现字面装饰字符或操作系统探测。
# el.rs 与 caps.rs 同类：它就是画装饰的那一层，而降级发生在上屏时
# （ansi::encode_with），所以这里写 ┌ 是对的。豁免它，是为了让这条规则说的是
# 「上层不得绕过原语」，而不是「谁都不许画框」——后者会逼原语层也去问 Caps，
# 而在 lay(w) 里它拿不到。
UPPER=$(find "$SRC" -name '*.rs' ! -name caps.rs ! -name surface.rs ! -name el.rs)

echo "→ 屏蔽层之外不得出现字面装饰字符（应走 Caps::g(Glyph::…)）"
n=$(code_only $UPPER | grep -oE '[┌┐└┘─│├┤┬┴┼✓✗⋯▸•]' | wc -l | tr -d ' ')
ratchet literal_glyphs "$n" "字面制表符/状态符，ASCII 终端上会 tofu 且宽度可能错" || fail=1

echo "→ 调色板之外不得写裸颜色（应走 Color::role(Role::…)）"
# 起因：新 TUI 一开始到处写 Color::Ansi(75)、Ansi(236) —— 那既假设了深色终端，
# 也和 tuix 的调色板对不上，而两个前端对「muted 是什么颜色」有两种答案就是两个
# 产品。角色在上屏时才解析成颜色，那里才知道明暗。
PALETTE=$(find "${SRC}" -name '*.rs' ! -name theme.rs ! -name frame.rs ! -name ansi.rs)
n=$(code_only ${PALETTE} | grep -cE 'Color::(Ansi|Rgb)\(')
ratchet raw_colours "${n}" "裸颜色索引，绕过了角色调色板，也绕过了明暗主题" || fail=1

echo "→ 屏蔽层之外不得探测操作系统或终端"
n=$(code_only $UPPER | grep -cE 'cfg!\(target_os|"TERM"|"LANG"|"LC_ALL"|ATOMCODE_ASCII|"NO_COLOR"|"WT_SESSION"')
ratchet os_probes "$n" "直接探测终端/操作系统，绕过了 Caps" || fail=1

if [ $fail = 0 ]; then echo -e "\n分层闸门：通过"; else echo -e "\n分层闸门：未通过"; fi
exit $fail
