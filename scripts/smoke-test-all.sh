#!/bin/bash
# shellcheck disable=SC2016 # fake cargo 脚本必须保持为不展开的字面量
# Smoke test for scripts/test-all.sh — 验证编译失败、测试失败、成功三种场景。
# 用法: ./scripts/smoke-test-all.sh
#
# 原理：
#   test-all.sh 通过 $REPORT 文件输出 Markdown 报告，并写入 "test-report.md"。
#   我们在临时目录中运行 test-all.sh，并用一个伪造的 cargo 来模拟三种场景。
#   然后检查脚本退出码、终端输出和 Markdown 报告，确认三种场景都被正确区分。
#
# 三种场景：
#   1. 编译失败：cargo 在编译阶段报 error[E...]，无 "test result:" 行，退出码 101。
#   2. 测试失败：cargo 输出 "test result: FAILED. 0 passed; 3 failed;"，退出码 101。
#   3. 全部成功：cargo 输出 "test result: ok. 5 passed; 0 failed;"，退出码 0。
#
# 旧脚本的核心假阳性是场景 1：cargo 的非零退出码被 `|| true` 吞掉，
# 且编译失败没有 `test result:` 行，最终误报 ALL TESTS PASSED。

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEST_ALL="$SCRIPT_DIR/test-all.sh"
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

PASS=0
FAIL=0

# 运行一个场景：注入一个 fake cargo 到 PATH 前面，然后在临时目录跑 test-all.sh。
# 参数：
#   $1 — 场景名
#   $2 — fake cargo 脚本内容（写到一个文件里）
#   $3 — 期望的 test-all.sh 退出码
#   $4 — 期望终端输出中包含的关键字
#   $5 — 期望 test-report.md 中包含的关键字
run_scenario() {
    local name="$1"
    local fake_cargo_content="$2"
    local expected_exit="$3"
    local expected_keyword="$4"
    local expected_report_keyword="$5"

    local workdir="$TMPDIR/$name"
    mkdir -p "$workdir"

    # test-all.sh 在头部调用 git rev-parse / git branch --show-current，
    # 且脚本带 `set -e`，在非 git 目录中这些命令会失败并让脚本立即退出。
    # 这里在 workdir 初始化一个临时 git 仓库绕过该问题。
    # 注意：用 --no-verify 绕过仓库根的 commitlint hook，否则 commit 会被拒。
    git -C "$workdir" init -q
    git -C "$workdir" config user.email "smoke@test"
    git -C "$workdir" config user.name "smoke"
    git -C "$workdir" commit -q --allow-empty --no-verify -m "chore: init" 2>/dev/null || true

    # 写入 fake cargo
    local fake_bin="$workdir/fakebin"
    mkdir -p "$fake_bin"
    printf '%s\n' "$fake_cargo_content" > "$fake_bin/cargo"
    chmod +x "$fake_bin/cargo"

    # 在 workdir 中运行 test-all.sh，把 fakebin 放到 PATH 最前面。
    # test-all.sh 会写入 ./test-report.md，所以 cwd 必须可写。
    local actual_output
    actual_output=$(cd "$workdir" && PATH="$fake_bin:$PATH" bash "$TEST_ALL" 2>&1)
    local actual_exit=$?

    # 断言 1：退出码符合期望
    if [ "$actual_exit" -ne "$expected_exit" ]; then
        echo "FAIL [$name]: expected exit $expected_exit, got $actual_exit"
        echo "--- output ---"
        echo "$actual_output"
        echo "--- end ---"
        FAIL=$((FAIL + 1))
        return
    fi

    # 断言 2：终端输出中包含期望关键字
    if ! echo "$actual_output" | grep -qF "$expected_keyword"; then
        echo "FAIL [$name]: expected keyword '$expected_keyword' not found in output"
        echo "--- output ---"
        echo "$actual_output"
        echo "--- end ---"
        FAIL=$((FAIL + 1))
        return
    fi

    # 断言 3：报告中的最终状态与终端结果一致
    if ! grep -qF "$expected_report_keyword" "$workdir/test-report.md"; then
        echo "FAIL [$name]: expected keyword '$expected_report_keyword' not found in report"
        echo "--- report ---"
        cat "$workdir/test-report.md"
        echo "--- end ---"
        FAIL=$((FAIL + 1))
        return
    fi

    echo "PASS [$name] (exit=$actual_exit)"
    PASS=$((PASS + 1))
}

# ---------- 场景 1：编译失败 ----------
# 模拟 cargo test 在编译阶段就失败：输出 error[E0308]，无 test result: 行，退出码 101。
# 这是旧脚本最严重的假阳性：PASSED=0/FAILED=0 时会误报 ALL TESTS PASSED。
FAKE_COMPILE_FAIL='#!/bin/bash
if [ "$1" = "test" ]; then
    echo "   Compiling foo v0.1.0" >&2
    echo "error[E0308]: mismatched types" >&2
    echo "  --> src/lib.rs:1:1" >&2
    echo "   |" >&2
    echo "   |     let x: i32 = \"string\";" >&2
    echo "   |                ^^^^^^^^ expected `i32`, found `&str`" >&2
    echo "" >&2
    echo "error: could not compile `foo` due to previous error" >&2
    exit 101
fi
# 非 test 子命令，假装成功
exit 0
'
run_scenario "compile_failure" "$FAKE_COMPILE_FAIL" 1 \
    "BUILD/RUN FAILED" "Status: FAILED (build/run error, no tests counted)"

# ---------- 场景 2：测试失败 ----------
# 模拟 cargo test 编译通过但有测试失败：输出 test result: FAILED，退出码 101。
FAKE_TEST_FAIL='#!/bin/bash
if [ "$1" = "test" ]; then
    echo "   Compiling foo v0.1.0" >&2
    echo "    Finished test [unoptimized + debuginfo] target(s) in 0.1s" >&2
    echo "     Running unittests src/lib.rs" >&2
    echo "" >&2
    echo "running 3 tests" >&2
    echo "test test_a ... FAILED" >&2
    echo "test test_b ... ok" >&2
    echo "test test_c ... ok" >&2
    echo "" >&2
    echo "failures:" >&2
    echo "" >&2
    echo "---- test_a stdout ----" >&2
    echo "thread '"'"'test_a'"'"' panicked at src/lib.rs:5:5:" >&2
    echo "assertion failed: 1 == 2" >&2
    echo "note: run with RUST_BACKTRACE=1 to display a backtrace" >&2
    echo "" >&2
    echo "failures:" >&2
    echo "    test_a" >&2
    echo "" >&2
    echo "test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out" >&2
    exit 101
fi
exit 0
'
run_scenario "test_failure" "$FAKE_TEST_FAIL" 1 \
    "test result: FAILED" "Status: FAILED"

# ---------- 场景 3：全部成功 ----------
# 模拟 cargo test 编译通过且所有测试通过：输出 test result: ok，退出码 0。
FAKE_ALL_PASS='#!/bin/bash
if [ "$1" = "test" ]; then
    echo "   Compiling foo v0.1.0" >&2
    echo "    Finished test [unoptimized + debuginfo] target(s) in 0.1s" >&2
    echo "     Running unittests src/lib.rs" >&2
    echo "" >&2
    echo "running 5 tests" >&2
    echo "test test_a ... ok" >&2
    echo "test test_b ... ok" >&2
    echo "test test_c ... ok" >&2
    echo "test test_d ... ok" >&2
    echo "test test_e ... ok" >&2
    echo "" >&2
    echo "test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out" >&2
    exit 0
fi
exit 0
'
run_scenario "all_pass" "$FAKE_ALL_PASS" 0 \
    "ALL TESTS PASSED" "Status: ALL PASSED"

# ---------- 场景 4：部分 workspace 失败 ----------
# 模拟 `cargo test --workspace`：某个 crate 的测试全过(有 "test result: ok" + "... ok" 行),
# 但另一个 crate 编译失败(error[...]),cargo 整体退出非 0。此时 PASSED>0 且 FAILED==0。
# 校验:退出码 1、终端说明"部分通过但未全部完成"、报告状态不再自相矛盾地写"no tests counted"。
FAKE_PARTIAL_FAIL='#!/bin/bash
if [ "$1" = "test" ]; then
    echo "   Compiling crate-a v0.1.0" >&2
    echo "     Running unittests src/lib.rs (crate-a)" >&2
    echo "" >&2
    echo "running 5 tests" >&2
    echo "test a1 ... ok" >&2
    echo "test a2 ... ok" >&2
    echo "test a3 ... ok" >&2
    echo "test a4 ... ok" >&2
    echo "test a5 ... ok" >&2
    echo "" >&2
    echo "test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out" >&2
    echo "   Compiling crate-b v0.1.0" >&2
    echo "error[E0432]: unresolved import \`crate_b::missing\`" >&2
    echo "error: could not compile \`crate-b\` due to previous error" >&2
    exit 101
fi
exit 0
'
run_scenario "partial_failure" "$FAKE_PARTIAL_FAIL" 1 \
    "BUILD/RUN FAILED" "build did not fully complete"

# ---------- 总结 ----------
echo ""
echo "=== Smoke Test Summary ==="
echo "  Passed: $PASS"
echo "  Failed: $FAIL"
if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
exit 0
