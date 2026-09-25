#!/usr/bin/env bash
# project-local: gates/tui-string-slice.sh 的阴性对照。
#
# 一个只在合格对象上跑绿的判据没有鉴别力。这里造一个密封在 mktemp 里的最小 crate:
# 没说明理由的字节切片必须判红,说明了的必须判绿,编译不过的也必须判红。
set -uo pipefail
cd "$(dirname "$0")/.."
GATE="$PWD/gates/tui-string-slice.sh"
fail=0

fixture() {                               # fixture <lib.rs 内容> → crate 目录
  local d; d=$(mktemp -d)
  mkdir -p "$d/src"
  cat > "$d/Cargo.toml" <<'TOML'
[package]
name = "string-slice-fixture"
version = "0.0.0"
edition = "2021"

[workspace]
TOML
  printf '%s\n' "$1" > "$d/src/lib.rs"
  echo "$d"
}

check() {                                 # check <名字> <期望 pass|fail> <dir>
  local name="$1" want="$2" dir="$3"
  CARGO_TARGET_DIR="$dir/target" TUI_STRING_SLICE_MANIFEST="$dir/Cargo.toml" \
    bash "$GATE" >/dev/null 2>&1
  local code=$?
  rm -rf "$dir"
  if [ "$want" = pass ] && [ $code = 0 ]; then echo "  ✓ $name"; return 0; fi
  if [ "$want" = fail ] && [ $code != 0 ]; then echo "  ✓ ${name}（如期判红）"; return 0; fi
  echo "  ✗ ${name}：期望 ${want}，实际退出码 $code"
  fail=1
}

echo "=== 阴性对照：没说明的切片必须判红 ==="
# 就是那个杀了 TUI 四次的形状。
check "len() - N 的尾切" fail "$(fixture 'pub fn subject_of(text: &str) -> &str { &text[text.len() - 44..] }')"
check "测试代码里的切片也算" fail "$(fixture '#[cfg(test)]
mod tests {
    #[test]
    fn t() { assert_eq!(&"abc"[..1], "a"); }
}')"
check "编译不过" fail "$(fixture 'pub fn broken( -> {}')"

echo "=== 正向：没有切片、或说明了理由的，必须判绿 ==="
check "按字符截" pass "$(fixture 'pub fn head(text: &str) -> String { text.chars().take(3).collect() }')"
check "带理由的放行" pass "$(fixture '#[allow(clippy::string_slice, reason = "`find` of ASCII `/` is a char boundary")]
pub fn tail(text: &str) -> &str {
    match text.find(char::from(47u8)) { Some(at) => &text[at + 1..], None => text }
}')"

if [ $fail = 0 ]; then echo -e "\n阴性对照：通过"; else echo -e "\n阴性对照：未通过"; fi
exit $fail
