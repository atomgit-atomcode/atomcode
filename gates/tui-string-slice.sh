#!/usr/bin/env bash
# project-local: 按字节切字符串,必须自己说明为什么安全。
#
# 起因（真实）：`subject_of` 用 `&text[text.len() - 44..]` 给长路径做缩写。命令里
# 带中文时这个偏移落在字符中间,切片 panic——而它跑在渲染里,整个 TUI 当场消失,
# 屏幕上留不下一个字。一下午杀了四次,人只看到"闪退"。
#
# 规则本来就写下来了:width.rs 的模块头第一句就是"数终端格位的地方去数字节是终端
# 输出损坏最常见的来源"。但它只是一段散文,没有任何东西能判它。这条闸门就是把那段
# 散文变成机器能判的东西。
#
# 它不认识"安全的切片":每一处都得自己说明,
#   #[allow(clippy::string_slice, reason = "…")]
# 说不出理由的,就该改用 width::take_width / take_width_from_end。
set -uo pipefail
cd "$(dirname "$0")/.."

out=$(cargo clippy --no-deps -p atomcode-tui --all-targets -- \
        -A warnings -D clippy::string_slice 2>&1)
if echo "$out" | grep -q "^error\[E\|^error: could not compile.*due to.*previous error" && \
   ! echo "$out" | grep -q "indexing into a string"; then
  echo "  ✗ 这个 crate 现在编译不过,切片无从谈起"
  echo "$out" | grep -E "^error" | head -3 | sed 's/^/    /'
  exit 1
fi
if echo "$out" | grep -q "indexing into a string"; then
  echo "  ✗ 有按字节切字符串的地方没说明为什么安全:"
  echo "$out" | grep -E "^\s+-->" | sort -u | sed 's/^/    /'
  exit 1
fi
echo "  ✓ 没有未说明的字符串字节切片"
