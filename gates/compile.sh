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

if [ $fail = 0 ]; then
  echo -e "\n\033[32m编译闸门:通过\033[0m"
else
  echo -e "\n\033[31m编译闸门:未通过\033[0m"
fi
exit $fail
