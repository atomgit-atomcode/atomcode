#!/usr/bin/env bash
# project-local: 屏幕说的每一句话都要在词表里，不许写死在渲染代码中间。
#
# 起因（真实）：新前端从头到尾把中文写死在代码里，`/language` 改的是产品那张表，
# 屏幕不跟——于是一个英文用户把语言切成 en，欢迎块变成英文，状态栏、命令帮助、
# 审批面板仍然整屏中文。这件事编译得过、测试全绿，只有真人打开才看得见。
#
# 这条闸门数的是「生产代码里还有几处写死的 CJK 字符串字面量」。判据是字面量，不是
# 整行：`// ✓ 是宽字符` 这种注释不算，`"已中断"` 算。
#
# 存量走棘轮：只能降不能升。首跑冻结当前计数。
set -uo pipefail
cd "$(dirname "$0")/.."

# 可被阴性对照重定向到密封 fixture——一条只能对真实仓库跑的闸门，无法被证明它会判红。
SRC=${TUI_I18N_SRC:-"crates/atomcode-tui/src crates/atomcode-cli/src"}
BASE=${TUI_I18N_BASELINE:-gates/tui-i18n.baseline}
fail=0

# 豁免，每条都写清为什么——豁免没有理由就是把闸门关掉。
#
#   conformance.rs  这些中文是**被画的对象**，不是说给人听的话：它们存在就是为了
#                   证明渲染器能处理 CJK 宽度、能在长中文命令上正确截断。翻译它们
#                   等于删掉这些 fixture 要检查的性质。
#   launch.rs       `--audit` / `--demo` 那一帧的样例输入，同上。
EXEMPT_FILES='conformance\.rs|launch\.rs'

report=$(python3 - "$SRC" "$EXEMPT_FILES" <<'PY'
import re, sys, pathlib

roots = sys.argv[1].split()
exempt = re.compile(sys.argv[2])
LIT = re.compile(r'"((?:[^"\\]|\\.)*)"')
CJK = re.compile(r'[一-鿿]')

def production(path):
    """Lines with `#[cfg(test)]` items removed by brace matching."""
    lines = path.read_text().split('\n')
    skip = [False] * len(lines)
    i = 0
    while i < len(lines):
        if re.match(r'\s*#\[cfg\(test\)\]', lines[i]):
            depth, j, opened = 0, i, False
            while j < len(lines):
                depth += lines[j].count('{') - lines[j].count('}')
                if '{' in lines[j]:
                    opened = True
                skip[j] = True
                if opened and depth <= 0:
                    break
                j += 1
            i = j + 1
            continue
        i += 1
    return [(n + 1, l) for n, l in enumerate(lines) if not skip[n]]

hits = []
for root in roots:
    for p in sorted(pathlib.Path(root).rglob('*.rs')):
        if exempt.search(p.name):
            continue
        for n, line in production(p):
            s = line.strip()
            if s.startswith('//'):
                continue
            for m in LIT.finditer(line):
                if CJK.search(m.group(1)):
                    hits.append(f"{p}:{n}: {s[:100]}")
                    break
print(len(hits))
for h in hits[:40]:
    print("    " + h)
PY
)
count=$(printf '%s\n' "$report" | head -1)
where=$(printf '%s\n' "$report" | tail -n +2)

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
    [ -n "$where" ] && printf '%s\n' "$where"
    return 1
  fi
  if [ "$count" -lt "$base" ]; then
    local t; t=$(mktemp); grep -v "^$name=" "$BASE" > "$t"; printf '%s=%s\n' "$name" "$count" >> "$t"; mv "$t" "$BASE"
    echo "  ✓ $name: ${count}（基线从 $base 降到 ${count}）"
    return 0
  fi
  echo "  ✓ $name: ${count}（持平基线）"
}

echo "→ 屏幕与它的启动器不得写死中文字面量（应走 i18n::t(Msg::…)）"
ratchet hardcoded_cjk "$count" "写死的中文,/language 改不动它" || fail=1

# 第二条：同一句话不许写两遍。
#
# 起因：两个前端是一个产品的两种渲染。屏幕有了自己的词表之后，最容易发生的事是把
# 产品表里已有的那句话在屏幕表里再写一遍——于是「允许一次」在一个前端叫「允许一
# 次」，在另一个前端叫「允许一下」，而两边都没有错。规则是：产品表已经有的，屏幕
# 表读它（`crate::i18n::product::t`），不重写。
#
# 判据是**中英两边都重合**：只有中文撞上不算——「计划」在一处是待办、在另一处是
# 订阅，英文分别是 plan 和 Plan，那是同形不同义，各留各的才对。两种语言都说同一句
# 话，才是同一句话写了两遍。比较忽略大小写：`Deny` 和 `deny` 是一句话。
TABLES=${TUI_I18N_TABLES:-crates/atomcode-i18n/src}
if [ -d "$TABLES/product" ] && [ -d "$TABLES/screen" ]; then
  echo "→ 同一句话不许在两张表里各写一遍"
  dupes=$(python3 - "$TABLES" <<'PY'
import re, sys, pathlib
root = pathlib.Path(sys.argv[1])
LIT = re.compile(r'"((?:[^"\\]|\\.)*)"')
CJK = re.compile(r'[一-鿿]')

def said(dirname):
    """{variant: (zh, en)} for every arm that renders one literal."""
    def one(path):
        out = {}
        for line in path.read_text().split('\n'):
            s = line.strip()
            if not s.startswith('Msg::'):
                continue
            m = LIT.search(line)
            if m:
                out[s.split('=>')[0].strip()] = m.group(1)
        return out
    zh = one(root / dirname / 'zh_cn.rs')
    en = one(root / dirname / 'en.rs')
    pairs = {}
    for variant, text in zh.items():
        # One character is a joiner or a mark, not a sentence.
        if CJK.search(text) and len(text) > 1 and variant in en:
            pairs[(text, en[variant].lower())] = variant
    return pairs

a = said('product')
b = said('screen')
both = sorted(set(a) & set(b))
print(len(both))
for key in both[:20]:
    print("    %r: %s / %s" % (key[0], a[key], b[key]))
PY
)
  n=$(printf '%s\n' "$dupes" | head -1)
  rest=$(printf '%s\n' "$dupes" | tail -n +2)
  if ! ratchet said_twice "$n" "同一句中文在两张表里各写了一遍"; then
    fail=1
    [ -n "$rest" ] && printf '%s\n' "$rest"
  fi
fi

if [ $fail = 0 ]; then echo -e "\ni18n 闸门：通过"; else echo -e "\ni18n 闸门：未通过"; fi
exit $fail
