#!/usr/bin/env bash
# 公开仓库的 Cargo.lock 里不得出现私有 overlay 的三个依赖。
#
# 起因（本机会反复污染，2026-09-15 实测）：`crates/atomcode-codingplan-crypto/`
# 在公开仓库里是只有签名没有实现的 stub，而本机放的是实现版——它的 Cargo.toml 声明了
# hmac / sha2 / hkdf / zeroize / subtle。根 Cargo.toml 的 `workspace.members = ["crates/*"]`
# 是 glob，把它收成了 member，cargo 解析 workspace 时把这些依赖写进 lock。
#
# 真正的根因是 `atomcode-auth` 那条 optional path 依赖：Cargo.lock 记的是所有 optional
# 依赖的解析结果，不管 feature 开不开，所以只要那条 path 依赖还在，它们就必然落进 lock。
# （`[workspace.exclude]` 挡不住——实测它只说这个目录不是 member，挡不住那条 path 依赖。
# 别再照着它试第二遍。）
#
# 判据只数这三个名字：仓库版 lock 里本来就有合法的 `subtle` / `zeroize` 各 1 个，
# 别把它们加进判据（2026-10-12 实测踩过一次假阳性）。
#
# 本机那侧的止血是 `git update-index --skip-worktree Cargo.lock`（见
# docs/development-verification-notes.md）。它挡住的是「工作树操作」，守门要挪到
# 提交边界——也就是这个脚本：它查的是 **index 里那份**（= 提交进去的那份），
# 所以无论本机有没有设标志位都管用。
#
#   bash gates/cargo-lock.sh              # 查 index（提交进去的那份）
#   bash gates/cargo-lock.sh --worktree   # 查磁盘上的那份（本机有 overlay 时它必然是脏的）
set -uo pipefail
# 仓库根默认是本脚本的上一级；`CARGO_LOCK_REPO` 用来让阴性对照指向临时仓库。
cd "${CARGO_LOCK_REPO:-$(dirname "$0")/..}"

FORBIDDEN=(hkdf hmac zeroize_derive)

target=index
case "${1:-}" in
  --worktree) target=worktree ;;
  "") ;;
  *)
    echo "usage: bash gates/cargo-lock.sh [--worktree]" >&2
    exit 2
    ;;
esac

if [ "$target" = index ]; then
  if ! git rev-parse --git-dir >/dev/null 2>&1; then
    echo "  ✗ 不在 git 仓库里，查不了 index 那份 lock" >&2
    exit 1
  fi
  if ! git cat-file -e :Cargo.lock 2>/dev/null; then
    echo "  ✗ index 里没有 Cargo.lock" >&2
    exit 1
  fi
  source_desc="index（提交进去的那份）"
  lock_text=$(git show :Cargo.lock)
else
  if [ ! -f Cargo.lock ]; then
    echo "  ✗ 没有 Cargo.lock" >&2
    exit 1
  fi
  source_desc="工作树（磁盘上的那份）"
  lock_text=$(cat Cargo.lock)
fi

dirty=""
for pkg in "${FORBIDDEN[@]}"; do
  # 精确匹配整行：`name = "zeroize_derive"` 不该被 `name = "zeroize"` 命中，
  # 反过来也不该——所以锚定行尾。
  n=$(printf '%s\n' "$lock_text" | grep -c "^name = \"${pkg}\"$" || true)
  if [ "$n" -gt 0 ]; then
    dirty="${dirty} ${pkg}(${n})"
  fi
done

if [ -n "$dirty" ]; then
  echo "  ✗ Cargo.lock（${source_desc}）里有私有 overlay 的依赖:${dirty}"
  echo "     这些包只该存在于闭源 overlay，构建公开仓库时不该进 lock。"
  echo "     修法：用干净基线重来（git show <干净 commit>:Cargo.lock > Cargo.lock），"
  echo "     再把该提交之后**合法的**依赖变化补回。不要用 cargo update 或"
  echo "     cargo generate-lockfile——它们只会把私有依赖写回来。"
  echo "     背景与实测：docs/development-verification-notes.md 的「Cargo.lock 与私有 overlay」。"
  exit 1
fi

echo "  ✓ Cargo.lock（${source_desc}）里没有私有依赖"
exit 0
