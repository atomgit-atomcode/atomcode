#!/usr/bin/env bash
# Fetch Android-native Termux packages and unpack them into one root.
#
# Why Termux and not the NDK: the NDK ships *host* binaries (`clang.exe` for
# Windows, `clang` for Linux) — they run on the build machine, not on the phone.
# What the device needs is an Android/bionic build of clang, lld and node, and
# Termux is the maintained source of exactly those. (nodejs-mobile, which the
# current node 18 plugin uses, stops at 18.20.4 and its releases are on GitHub,
# which this machine cannot reach.)
#
# Usage:
#   scripts/termux/fetch.sh <cache-dir> <package>...
#
#   MIRROR=https://packages.termux.dev/apt/termux-main   (default)
#   ARCH=aarch64                                        (default)
#
# Output:
#   <cache-dir>/index/db.tsv      name, filename, sha256, depends (one pass)
#   <cache-dir>/debs/<name>.deb   verified downloads, reused on later runs
#   <cache-dir>/root/             every package's data.tar extracted over one
#                                 another, i.e. a `$PREFIX`-shaped tree
#
# Depends is followed transitively (the reusable runtime libraries), because a
# `libllvm` without `libc++` is a package that cannot load. Alternatives
# (`a | b`) take their first choice and version constraints are dropped: the
# index is the one source of truth for what is actually published.
set -euo pipefail

MIRROR=${MIRROR:-https://packages.termux.dev/apt/termux-main}
ARCH=${ARCH:-aarch64}
CACHE=${1:?usage: fetch.sh <cache-dir> <package>...}
shift
[ "$#" -gt 0 ] || { echo "error: no packages named" >&2; exit 2; }

IDX_DIR=$CACHE/index
DEB_DIR=$CACHE/debs
ROOT=$CACHE/root
mkdir -p "$IDX_DIR" "$DEB_DIR" "$ROOT"
# `ar x` runs inside a staging directory, so every deb path must be absolute.
CACHE=$(cd "$CACHE" && pwd)
IDX_DIR=$CACHE/index
DEB_DIR=$CACHE/debs
ROOT=$CACHE/root
DB=$IDX_DIR/db.tsv
INDEX=$IDX_DIR/Packages.$ARCH
STAMP=$INDEX.mirror

# The index is per mirror: a cached Packages file from another mirror would make
# the run look like it honoured `MIRROR` while quietly downloading from the old
# one. Re-fetch when the mirror changed, and rebuild the parse with it.
if [ -s "$INDEX" ] && [ "$(cat "$STAMP" 2>/dev/null || true)" != "$MIRROR" ]; then
  rm -f "$INDEX" "$DB"
fi

if [ ! -s "$DB" ]; then
  if [ ! -s "$INDEX" ]; then
    echo "→ index: $MIRROR ($ARCH)"
    curl -fsSL --retry 3 --retry-delay 2 -m 600 -o "$INDEX.part" \
      "$MIRROR/dists/stable/main/binary-$ARCH/Packages"
    mv "$INDEX.part" "$INDEX"
    printf '%s' "$MIRROR" > "$STAMP"
  fi
  # One paragraph per package. Keep the four fields we act on.
  awk -v out="$DB" 'BEGIN { RS = ""; FS = "\n" }
    {
      name = ""; file = ""; sha = ""; dep = "";
      for (i = 1; i <= NF; i++) {
        if ($i ~ /^Package: /)      name = substr($i, 10);
        else if ($i ~ /^Filename: /) file = substr($i, 11);
        else if ($i ~ /^SHA256: /)   sha  = substr($i, 9);
        else if ($i ~ /^Depends: /)  dep  = substr($i, 10);
      }
      if (name != "" && file != "") printf "%s\t%s\t%s\t%s\n", name, file, sha, dep;
    }' "$INDEX" > "$DB"
fi

field() { # field <package> <2=filename|3=sha256|4=depends>
  awk -F'\t' -v p="$1" -v n="$2" '$1 == p { print $n; exit }' "$DB"
}

# Breadth-first over Depends. A package named twice is fetched once.
queue=("$@")
declare -A seen=()
order=()
while [ "${#queue[@]}" -gt 0 ]; do
  pkg=${queue[0]}
  queue=("${queue[@]:1}")
  [ -n "${seen[$pkg]:-}" ] && continue
  seen[$pkg]=1
  if [ -z "$(field "$pkg" 2)" ]; then
    echo "error: \`$pkg\` is not published for $ARCH (check the index)" >&2
    exit 1
  fi
  order+=("$pkg")
  deps=$(field "$pkg" 4)
  [ -n "$deps" ] || continue
  # `a (>= 1), b | c` → a, b. Version constraints are the index's business.
  IFS=',' read -r -a parts <<<"$deps"
  for part in "${parts[@]}"; do
    dep=${part%%|*}
    dep=${dep%%(*}
    dep=$(printf '%s' "$dep" | tr -d '[:space:]')
    [ -n "$dep" ] && [ -z "${seen[$dep]:-}" ] && queue+=("$dep")
  done
done

echo "→ packages: ${order[*]}"

for pkg in "${order[@]}"; do
  deb="$DEB_DIR/$pkg.deb"
  sha=$(field "$pkg" 3)
  if [ -f "$deb" ] && [ -n "$sha" ] &&
     [ "$(sha256sum "$deb" | cut -d' ' -f1)" = "$sha" ]; then
    continue
  fi
  url="$MIRROR/$(field "$pkg" 2)"
  attempt=1
  while :; do
    echo "→ download $pkg (attempt $attempt)"
    # `-C -` resumes: a mirror that drops a 40 MB package mid-transfer should cost
    # the rest of the transfer, not all of it. `--speed-limit/--speed-time` turns a
    # silent stall into a failure instead of a job that hangs until the timeout.
    curl -fsSL --retry 3 --retry-delay 2 --retry-all-errors -C - \
      --speed-limit 4096 --speed-time 60 -m 1800 \
      -o "$deb.part" "$url" || rm -f "$deb.part"
    if [ -f "$deb.part" ]; then
      got=""
      [ -n "$sha" ] && got=$(sha256sum "$deb.part" | cut -d' ' -f1)
      if [ -z "$sha" ] || [ "$got" = "$sha" ]; then
        mv "$deb.part" "$deb"
        break
      fi
      echo "  sha256 mismatch on $pkg (got $got), refetching from scratch" >&2
      rm -f "$deb.part"
    fi
    attempt=$((attempt + 1))
    # A mirror under a flaky link resets a large transfer every few hundred KB;
    # `-C -` makes each attempt cost only what is left, so the useful cap is "stop
    # when it stops advancing", not "stop after four tries". 40 attempts of a
    # resumed 40 MB transfer is generous; a hard failure still ends the run.
    [ "$attempt" -le 40 ] || { echo "error: cannot download $pkg" >&2; exit 1; }
    sleep 2
  done
done

# Unpack. A .deb is an `ar` archive whose `data.tar.*` member holds the tree;
# which compressor that member uses is up to the packager (Termux uses xz, the
# index does not say), so detect instead of assuming.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
EXTRACT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/extract.py"
for pkg in "${order[@]}"; do
  deb="$DEB_DIR/$pkg.deb"
  stage="$work/$pkg"
  mkdir -p "$stage"
  ( cd "$stage" && ar x "$deb" )
  data=$(find "$stage" -maxdepth 1 -name 'data.tar*' -print -quit)
  [ -n "$data" ] || { echo "error: $pkg has no data.tar member" >&2; exit 1; }
  # extract.py, not tar: it strips the device path (`data/data/com.termux/
  # files/usr`) and materialises symlinks, which Windows cannot create for a
  # target that has not been written yet.
  python3 "$EXTRACT" "$data" "$ROOT"
  rm -rf "$stage"
done

echo "→ root: $ROOT ($(du -sh "$ROOT" | cut -f1))"