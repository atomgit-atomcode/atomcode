#!/usr/bin/env bash
# 每个 crate 的测试都必须能编译。
#
# 为什么要有这一条:2026-09-23 一天之内撞到两处「判据跑不起来,而没有人发现」——
#
#   * `atomcode-host-api`:契约里加了三个变体,测试中逐变体列举的两处 match 没跟着
#     补,`cargo nextest run -p atomcode-host-api` 从那次提交起就编译不过;
#   * `atomcode-capabilities` 的 `session/snapshot.rs`:被测代码把字符串换成了枚举,
#     测试还在对它 `.contains(...)`。
#
# 第二处尤其说明问题:`session` 挂在**非默认 feature** 下,所以
# `cargo nextest run -p atomcode-capabilities`(默认 feature 是 provider + tools)
# **根本不会编译那个文件**。也就是说「按 crate 跑测试」这条规矩,结构上就看不见
# 这类腐烂 —— 不是谁偷懒,是那把尺子量不到。
#
# 更早还有同一种烂法的两例:`ToolMiddleware::after` 签名漂移在
# `kernel/tests/conformance.rs` 里躺了一周并随 v5.0.9 发了出去(见
# `.github/workflows/check.yml` 开头);`capabilities` 的 `append_jsonl_line`
# (AGENTS.md 的 H4)。
#
# **CI 里本来就有这一条,而且是阻塞的**(`check.yml` 的 `cargo check --workspace
# --all-targets`)。它没能挡住上面这些,是因为 CI 只在 push 时跑,而这套流程会在本地
# 连着合好几天才推一次 —— 上面两处腐烂所在的提交,一次都没被推上去过。所以这个脚本
# 不是在 CI 之外另立一套判据,它就是**把那同一条闸门搬到本地、在合之前跑**。
#
# 代价:主检出热态实测 20.7 秒(2026-09-23)。`check` 不做代码生成也不链接,所以它
# 远比 `nextest run --workspace` 便宜 —— 后者会构建 91 个测试二进制、一次吃掉 8.5GB
# 磁盘(AGENTS.md 记的那次把磁盘顶到 99%、target 整个没了)。**这里绝不要改成 run。**
set -uo pipefail
cd "$(dirname "$0")/.."

fail=0

echo "→ 每个 crate 的测试都能编译"
if cargo check --workspace --all-targets >/tmp/atomcode-compile-gate.$$ 2>&1; then
  printf '  \033[32mok\033[0m   每个 crate 的测试都能编译\n'
else
  printf '  \033[31mFAIL\033[0m 有 crate 的测试编译不过:\n'
  grep -E '^error' -A 6 /tmp/atomcode-compile-gate.$$ | head -40
  fail=1
fi
rm -f /tmp/atomcode-compile-gate.$$

# 同一种烂法的第三例,这次是 **feature 藏起来的那一半**:上面那条走的是 workspace 里
# 各 crate 对 `atomcode-capabilities` 要求的 feature 的**并集**(resolver v2 在同一次
# `cargo check --workspace` 里合并它们)——`coding`/`tuix` 开了 session、memory、mcp,
# `cli` 开了 setup,所以挂在这几个 feature 后面的测试其实早就被编到了。编不到的,是
# **没有任何成员会开**的那几个 feature 后面的测试:
#
#   e2e      tests/e2e.rs(打真实 provider)
#   lsp-e2e  src/codeintel/lsp/manager.rs 里的真实 typescript-language-server 判据
#
# `e2e.rs` 就是这样烂掉的:`StreamEvent` 加了 `ResponseModel` / `Malformed` 两个变体
# (`237fc25b3`、`dc456e732`),那个逐变体列举的 `match` 没跟着补,`--features e2e`
# 编译不过 —— 而任何一次 workspace 检查都看不见它。
#
# 列表里留着 session/memory/setup/mcp:它们今天被并集顺带编到,但那是别的 crate 的
# 选择,哪天没人再开就又掉出去了;在这里显式开,比依赖别人碰巧开着稳。**别改成 run**
# —— e2e 与 lsp-e2e 都要真实的外部服务。
echo "→ feature 门控的测试也能编译"
# Compile the opt-in bench target too; never run or threshold its timings here.
FEATURE_GATED='session,memory,setup,e2e,mcp,lsp-e2e,bench'
if cargo check -p atomcode-capabilities --all-targets \
     --features "$FEATURE_GATED" \
     >/tmp/atomcode-feature-gate.$$ 2>&1; then
  printf '  \033[32mok\033[0m   feature 门控的测试也能编译（%s）\n' "$FEATURE_GATED"
else
  printf '  \033[31mFAIL\033[0m feature 门控的测试编译不过（%s）:\n' "$FEATURE_GATED"
  grep -E '^error' -A 6 /tmp/atomcode-feature-gate.$$ | head -40
  fail=1
fi
rm -f /tmp/atomcode-feature-gate.$$

# 量具自身要能判红。没有这一步,一个永远 `exit 0` 的脚本和这个脚本看起来一模一样。
#
# 办法是喂给它一个**一定编译不过**的测试文件,然后要求它说不。放在一个真 crate 的
# `tests/` 下再删掉 —— 这样走的是 `--all-targets` 真正会去编的那条路,而不是另起一个
# 只在这里存在的构造。
echo "→ 闸门自身会判红"
canary="crates/atomcode-host-api/tests/__gate_canary.rs"
cleanup() { rm -f "$canary"; }
trap cleanup EXIT
cat > "$canary" <<'RS'
// 阴性对照用,由 gates/compile.sh 写入并立刻删除。留在树里说明上一次跑被打断了。
#[test]
fn this_must_not_compile() {
    let _: u32 = "gate canary";
}
RS
if cargo check --workspace --all-targets >/dev/null 2>&1; then
  printf '  \033[31mFAIL\033[0m 闸门判不出编译错误 —— 它现在什么也没在挡\n'
  fail=1
else
  printf '  \033[32mok\033[0m   闸门自身会判红\n'
fi
cleanup
trap - EXIT

# 上面那个 canary 只证了默认 feature 那条会判红。这一条证**新门**判红,而且要证
# 的正是它存在的理由:烂代码放在 feature 门后面时,旧门必须仍然绿。
#
# 判据的形状反过来钉 —— 只钉「feature 门那条会红」的话,把两个 feature 写反
# (用默认 feature 去编)也照样全绿。所以这里钉的是**两者的差**:
# 同一份坏代码,旧门看不见、新门看得见。
#
# 坏代码放进一个**新建的、不入库的**文件,跑完删掉,和上面那个 canary 同一种做法。
# 不往已入库的测试文件里追加:那样跑闸门的这一两分钟里它在 `git status` 里是改过
# 的,别的会话一次 `git commit -a` 就会把坏代码提交进去;进程被强杀时它会一直坏在
# 那儿,还没有任何标记说明是闸门留下的。
#
# 红还得是**它**红的:两边的输出都按文件名认一遍。feature 门后面的代码本来就坏着
# 时,新门的红不能算作「判得出」;旧门因别的原因本来就红时,也不能说成「新门没盖住
# 新地方」。
echo "→ feature 门控的闸门自身会判红（旧门对它应当无感）"
feature_canary="crates/atomcode-capabilities/tests/__feature_gate_canary.rs"
feature_out=$(mktemp)
feature_cleanup() { rm -f "$feature_canary" "$feature_out"; }
trap feature_cleanup EXIT
cat > "$feature_canary" <<'RS'
// 阴性对照用,由 gates/compile.sh 写入并立刻删除。留在树里说明上一次跑被打断了。
#![cfg(feature = "e2e")]
#[test]
fn this_must_not_compile_under_a_feature() {
    let _: u32 = "feature gate canary";
}
RS
if cargo check -p atomcode-capabilities --all-targets \
     --features "$FEATURE_GATED" >"$feature_out" 2>&1; then
  printf '  \033[31mFAIL\033[0m feature 门控的闸门判不出编译错误\n'
  fail=1
elif ! grep -q "__feature_gate_canary" "$feature_out"; then
  printf '  \033[31mFAIL\033[0m feature 门报红,但不是阴性对照引起的 —— feature 门后面的代码本来就坏着,这一条判不了\n'
  fail=1
elif cargo check --workspace --all-targets >"$feature_out" 2>&1; then
  printf '  \033[32mok\033[0m   feature 门控的闸门自身会判红（旧门对它无感）\n'
elif grep -q "__feature_gate_canary" "$feature_out"; then
  printf '  \033[31mFAIL\033[0m 旧门也看见了阴性对照 —— 这条新门没盖住新地方\n'
  fail=1
else
  printf '  \033[31mFAIL\033[0m 旧门本身是红的(与阴性对照无关),两者的差判不了\n'
  fail=1
fi
feature_cleanup
trap - EXIT

if [ $fail = 0 ]; then
  echo -e "\n\033[32m编译闸门:通过\033[0m"
else
  echo -e "\n\033[31m编译闸门:未通过\033[0m"
fi
exit $fail
