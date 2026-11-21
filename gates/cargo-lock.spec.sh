#!/usr/bin/env bash
# 阴性对照：cargo-lock.sh 真能判红，也真能放过不该算的。
# 在 mktemp 里造一份 lock，量具对着它跑。
set -uo pipefail
cd "$(dirname "$0")/.."

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail=0

# 这道门查的是 git index 里那份，所以判红测试要真的动一个 index。
# 用一个临时仓库，不碰本仓库的 index。
repo="$tmp/repo"
mkdir -p "$repo"
git -C "$repo" init -q
git -C "$repo" config user.name ci
git -C "$repo" config user.email ci@example.com

check() {                      # check <期望退出码> <说明>
  local want="$1" what="$2" got
  ( cd "$repo" && CARGO_LOCK_REPO="$repo" bash "$GATE" ) >"$tmp/out" 2>&1
  got=$?
  if [ "$got" = "$want" ]; then
    printf '  \033[32mok\033[0m   %s\n' "$what"
  else
    printf '  \033[31mFAIL\033[0m %s（退出码 %s，期望 %s）\n' "$what" "$got" "$want"
    sed 's/^/        /' "$tmp/out"
    fail=1
  fi
}

put_lock() { printf '%s\n' "$1" >"$repo/Cargo.lock"; git -C "$repo" add Cargo.lock; }

GATE="$PWD/gates/cargo-lock.sh"

clean_lock='[[package]]
name = "zeroize"
version = "1.8.1"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "subtle"
version = "2.6.1"
source = "registry+https://github.com/rust-lang/crates.io-index"'

# ── 不该算的 ──
put_lock "$clean_lock"
check 0 "合法的 subtle / zeroize 不算（它们本来就该在 lock 里）"

put_lock '[[package]]
name = "zeroize_derive_x"
version = "0.1.0"'
check 0 "名字前缀相同的包（zeroize_derive_x）不算"

put_lock '[[package]]
name = "myhkdf"
version = "0.1.0"'
check 0 "名字里含 hkdf 但不是 hkdf 的包不算"

put_lock '[[package]]
name = "atomcode-codingplan-crypto"
version = "0.1.0"
dependencies = [
 "hmac",
]'
check 0 "依赖列表里提到 hmac 但没有那个包条目，不算（判据数的是包条目）"

# ── 该算的 ──
for pkg in hkdf hmac zeroize_derive; do
  put_lock "[[package]]
name = \"$pkg\"
version = \"0.12.1\""
  check 1 "$pkg 判红"
done

# ── 工作树那份也判红 ──
put_lock "$clean_lock"
printf '[[package]]\nname = "hkdf"\nversion = "0.12.1"\n' >>"$repo/Cargo.lock"
got=0
( cd "$repo" && bash "$OLDPWD/gates/cargo-lock.sh" --worktree ) >"$tmp/out" 2>&1 || got=$?
if [ "$got" = 1 ]; then
  printf '  \033[32mok\033[0m   --worktree 查磁盘上那份，脏的判红\n'
else
  printf '  \033[31mFAIL\033[0m --worktree 查磁盘上那份（退出码 %s，期望 1）\n' "$got"
  sed 's/^/        /' "$tmp/out"
  fail=1
fi

exit $fail
