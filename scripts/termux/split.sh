#!/usr/bin/env bash
# Split an extracted Termux tree into (a) an APK's jniLibs and (b) a data package.
#
# The split is forced by Android, not by taste:
#
#   * Only a package's `nativeLibraryDir` may be executed (a binary in the app's
#     data directory returns 126), and that directory is filled from the APK's
#     `lib/<abi>/` — so every *executable* must be packaged as `lib<stem>.so`.
#     The platform does not care that it is really `clang`, only that it is an
#     ELF file the loader extracted there.
#   * Shared objects are already named `lib*.so`, so they ride in the same
#     directory and are found through `LD_LIBRARY_PATH` (which outranks the
#     Termux `$PREFIX` RUNPATH baked into every one of these binaries).
#   * Everything that is only *read* — headers, the clang resource directory,
#     sysroot — needs no execution rights and would only bloat the APK, so it
#     becomes a market data package.
#
# Usage:
#   scripts/termux/split.sh <root> <jni-out> <data-out> <manifest>
#
# Manifest (one directive per line, `#` comments allowed):
#   exec <path-in-root> <stem>   # → <jni>/lib<stem>.so   (becomes `bin/<stem>`)
#   data <path-in-root>          # file or directory → <data>/<path>
#   lib  <path-in-root>          # a dlopen()ed .so → <jni>/<basename> (kept as is)
#
# The shared libraries each executable needs are NOT listed: they are read from
# the ELF headers (`DT_NEEDED`) and pulled in recursively, so the manifest says
# what the tool is, not how its packaging happens to be split today.
set -euo pipefail

ROOT=${1:?usage: split.sh <root> <jni-out> <data-out> <manifest>}
JNI=${2:?}
DATA=${3:?}
MANIFEST=${4:?}
[ -d "$ROOT" ] || { echo "error: no such root: $ROOT" >&2; exit 1; }
[ -f "$MANIFEST" ] || { echo "error: no such manifest: $MANIFEST" >&2; exit 1; }

# The NDK's llvm-readelf is the one reliable ELF reader on a Windows build host.
find_readelf() {
  [ -n "${READELF:-}" ] && { echo "$READELF"; return; }
  local ndk=${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}
  [ -n "$ndk" ] && [ -d "$ndk" ] || ndk=$(ls -d .android-sdk/ndk/* 2>/dev/null | sort -V | tail -1 || true)
  [ -n "$ndk" ] || return 1
  local host=linux-x86_64
  case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) host=windows-x86_64 ;; Darwin*) host=darwin-x86_64 ;; esac
  local bin="$ndk/toolchains/llvm/prebuilt/$host/bin"
  for name in llvm-readelf readelf; do
    for candidate in "$bin/$name" "$bin/$name.exe"; do
      [ -x "$candidate" ] && { echo "$candidate"; return; }
    done
  done
  return 1
}
READELF=$(find_readelf) || {
  echo "error: no llvm-readelf found (set READELF=/path/to/llvm-readelf)" >&2
  exit 1
}

mkdir -p "$JNI" "$DATA"
report=$(mktemp)
trap 'rm -f "$report"' EXIT

# Where a DT_NEEDED name lives in the extracted tree. Termux keeps its libraries
# in `lib/`; the search is by basename because that is all the loader knows too.
find_lib() {
  local name=$1
  for dir in lib lib/llvm/lib usr/lib; do
    [ -f "$ROOT/$dir/$name" ] && { echo "$ROOT/$dir/$name"; return 0; }
  done
  find "$ROOT" -maxdepth 3 -name "$name" -type f -print -quit 2>/dev/null
}

# Breadth-first over DT_NEEDED so one ELF brings its whole runtime closure.
declare -A libs_seen=()
lib_order=()
collect_libs() {
  local queue=("$@") file name dep
  while [ "${#queue[@]}" -gt 0 ]; do
    file=${queue[0]}; queue=("${queue[@]:1}")
    while read -r name; do
      [ -n "$name" ] || continue
      [ -n "${libs_seen[$name]:-}" ] && continue
      dep=$(find_lib "$name") || true
      if [ -z "$dep" ]; then
        # `libc.so`, `libdl.so`, `liblog.so` … are the platform's, not the package's.
        case "$name" in
          libc.so|libm.so|libdl.so|liblog.so|libz.so|libandroid.so|libstdc++.so) continue ;;
        esac
        echo "error: $file needs \`$name\` and it is not in the tree" >&2
        return 1
      fi
      libs_seen[$name]=1
      lib_order+=("$dep")
      queue+=("$dep")
    done < <("$READELF" -d "$file" 2>/dev/null |
             sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p')
  done
}

execs=()
while read -r kind path stem; do
  case "$kind" in ''|\#*) continue ;; esac
  case "$kind" in
    exec)
      src="$ROOT/$path"
      [ -f "$src" ] || { echo "error: exec not in tree: $path" >&2; exit 1; }
      [ -n "${stem:-}" ] || { echo "error: exec needs a stem: $path" >&2; exit 1; }
      # A stem maps to the command name, so `clang++` is `libclangxx.so` and the
      # shim can link `bin/clang++` at it.
      cp "$src" "$JNI/lib$stem.so"
      chmod +x "$JNI/lib$stem.so"
      execs+=("$stem:$path")
      collect_libs "$src"
      ;;
    data)
      src="$ROOT/$path"
      [ -e "$src" ] || { echo "error: data path not in tree: $path" >&2; exit 1; }
      mkdir -p "$DATA/$(dirname "$path")"
      cp -a "$src" "$DATA/$path"
      ;;
    lib)
      src="$ROOT/$path"
      [ -f "$src" ] || { echo "error: lib not in tree: $path" >&2; exit 1; }
      # A shared object the program `dlopen`s at runtime. It goes into
      # nativeLibraryDir, which is already on LD_LIBRARY_PATH; its own
      # dependencies are still pulled in automatically.
      #
      # Only for a name that already matches `lib*.so`: Gradle packages NOTHING
      # else from jniLibs, so a versioned soname (`libexpat.so.1`) silently
      # disappears from the APK. Those belong in the data package instead.
      cp "$src" "$JNI/$(basename "$path")"
      collect_libs "$src"
      ;;
    *)
      echo "error: unknown manifest directive \`$kind\`" >&2
      exit 1
      ;;
  esac
done < "$MANIFEST"

for lib in "${lib_order[@]}"; do
  name=$(basename "$lib")
  case "$name" in
    lib*.so)
      cp "$lib" "$JNI/$name"
      ;;
    *)
      # A versioned soname (`librhash.so.1`, `libcrypto.so.3` …). Gradle packages
      # ONLY `lib*.so` from jniLibs, so these used to vanish from the APK without a
      # word — measured on device: cmake died with `library "librhash.so.1" not
      # found` although the file was staged. They go into the data tree instead,
      # and the host puts `<data>/libs` on LD_LIBRARY_PATH (loading a shared object
      # from app data is allowed; executing from there is not).
      mkdir -p "$DATA/libs"
      cp "$lib" "$DATA/libs/$name"
      ;;
  esac
done

{
  echo "# generated by scripts/termux/split.sh — $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  for entry in "${execs[@]}"; do
    stem=${entry%%:*}; src=${entry#*:}
    echo "bin/$stem → lib/$stem.so (from $src)"
  done
  echo "libs: $(basename -a "${lib_order[@]}" 2>/dev/null | tr '\n' ' ')"
} > "$JNI/../TERMUX-SPLIT.txt"

echo "→ execs: $(basename -a "${lib_order[@]}" 2>/dev/null | wc -l | tr -d ' ') runtime libs, ${#execs[@]} executables"
echo "→ jniLibs: $JNI ($(du -sh "$JNI" | cut -f1))"
echo "→ data:    $DATA ($(du -sh "$DATA" | cut -f1))"