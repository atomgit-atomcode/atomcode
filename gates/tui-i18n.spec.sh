#!/usr/bin/env bash
# project-local: gates/tui-i18n.sh 的阴性对照。
#
# 一个只在合格对象上跑绿的判据没有鉴别力。这里造违规 fixture 断言它判红，造合规的
# 断言它判绿；fixture 全部密封在 mktemp 里，不读真实仓库的任何状态。
set -uo pipefail
cd "$(dirname "$0")/.."
GATE="$PWD/gates/tui-i18n.sh"
fail=0

run() {                                   # run <dir> → 退出码
  TUI_I18N_SRC="$1/src" TUI_I18N_BASELINE="$1/baseline" \
    TUI_I18N_TABLES="$1/tables" bash "$GATE" >/dev/null 2>&1
}

# 两张表的 fixture：`said` 那条规则读的是它们。
tables() {                                # tables <dir> <product-zh> <product-en> <screen-zh> <screen-en>
  local d="$1"
  mkdir -p "$d/tables/product" "$d/tables/screen"
  printf 'Msg::A => "%s".into(),\n' "$2" > "$d/tables/product/zh_cn.rs"
  printf 'Msg::A => "%s".into(),\n' "$3" > "$d/tables/product/en.rs"
  printf 'Msg::B => "%s".into(),\n' "$4" > "$d/tables/screen/zh_cn.rs"
  printf 'Msg::B => "%s".into(),\n' "$5" > "$d/tables/screen/en.rs"
}

fixture() {                               # fixture → 一棵合规的最小 src 树
  local d; d=$(mktemp -d)
  mkdir -p "$d/src"
  cat > "$d/src/panel.rs" <<'RS'
use crate::i18n::{t, Msg};
fn draw() -> String {
    t(Msg::Something).into_owned()
}
RS
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
d=$(fixture); check "只走词表的屏幕" pass "$d"

echo "=== 阴性对照：每种违规都必须判红 ==="

# 首跑冻结基线，再越过它——棘轮的判红条件是「比基线多」。
d=$(fixture); run "$d" >/dev/null
printf 'fn oops() -> &%sstatic str { "已中断" }\n' "'" >> "$d/src/panel.rs"
check "生产代码里写死一句中文" fail "$d"

d=$(fixture); run "$d" >/dev/null
printf 'fn oops() -> String { format!("第 {} 轮", 3) }\n' >> "$d/src/panel.rs"
check "写死在 format! 里" fail "$d"

d=$(fixture); run "$d" >/dev/null
printf 'fn oops() -> &%sstatic str { "已中断" }\n' "'" > "$d/src/another.rs"
check "新文件里写死" fail "$d"

echo "=== 不该误报的：这些必须仍然判绿 ==="

# 测试里的中文断言不算：判据要断言屏幕说了什么，而它就是中文。
d=$(fixture); run "$d" >/dev/null
cat >> "$d/src/panel.rs" <<'RS'
#[cfg(test)]
mod tests {
    #[test]
    fn it_says_so() {
        assert_eq!(super::draw(), "已中断");
    }
}
RS
check "测试模块里的中文" pass "$d"

# 注释里的中文不算：这条闸门只看字面量。上一条同形的闸门
# （gates/tui-layers.sh）因为 grep 整行，被一条文档注释判红过一次。
d=$(fixture); run "$d" >/dev/null
printf '// 这一行是注释,里面有中文,而且提到了 "已中断" 这个词\n' >> "$d/src/panel.rs"
printf 'fn fine() {} // 行尾注释:已中断\n' >> "$d/src/panel.rs"
check "注释里的中文" pass "$d"

# 豁免的两个文件不算,理由写在闸门里。
d=$(fixture); run "$d" >/dev/null
printf 'const FIXTURE: &str = "中文也要能画";\n' > "$d/src/conformance.rs"
printf 'const DEMO: &str = "再帮我看看 crates/ 的结构";\n' > "$d/src/launch.rs"
check "被豁免的 fixture 文件" pass "$d"

# 降下来要被接受,并且把基线改小——棘轮只能往一个方向走。
d=$(fixture)
printf 'fn oops() -> &%sstatic str { "已中断" }\n' "'" >> "$d/src/panel.rs"
run "$d" >/dev/null                        # 基线冻在 1
printf 'fn oops2() {}\n' > "$d/src/panel.rs"   # 清干净
if run "$d" && grep -q '^hardcoded_cjk=0$' "$d/baseline"; then
  echo "  ✓ 减少一处会把基线降下来"
else
  echo "  ✗ 减少一处没有把基线降下来：$(cat "$d/baseline")"
  fail=1
fi

echo "=== 第二条：同一句话不许写两遍 ==="

# 中英都撞上 = 同一句话写了两遍。
d=$(fixture); tables "$d" "允许一次" "Allow once" "允许一次" "allow once"
run "$d" >/dev/null                        # 冻基线（两张表都还只有一条时是 1）
tables "$d" "允许一次" "Allow once" "允许一次" "allow once"
printf 'Msg::C => "拒绝".into(),\n' >> "$d/tables/product/zh_cn.rs"
printf 'Msg::C => "Deny".into(),\n' >> "$d/tables/product/en.rs"
printf 'Msg::D => "拒绝".into(),\n' >> "$d/tables/screen/zh_cn.rs"
printf 'Msg::D => "deny".into(),\n' >> "$d/tables/screen/en.rs"
check "两张表各写了一遍同一句" fail "$d"

# 只有中文撞上、英文不同 = 同形不同义，不该判红。
d=$(fixture); tables "$d" "计划" "Plan" "计划" "the plan for this turn"
check "同形不同义（英文不同）" pass "$d"

# 单个字不算一句话：连接号、标点之类。
d=$(fixture); tables "$d" "、" ", " "、" ", "
check "单字连接号" pass "$d"

if [ $fail = 0 ]; then echo -e "\n阴性对照：通过"; else echo -e "\n阴性对照：未通过"; fi
exit $fail
