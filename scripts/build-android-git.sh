#!/usr/bin/env bash
# Build an Android `git` and drop it into `jniLibs/` so the phone has real git.
#
# Why a binary instead of a Rust implementation: `gix 0.90` (what this workspace
# pins) has no push — gitoxide's push is still unimplemented — and `gix-protocol`
# only speaks fetch. Ship-to-PC is half of what "interop with the desktop" means,
# so the phone needs the real git.
#
# Android 10+ refuses to execute files in an app's data dir, but files installed
# in the APK's `lib/<abi>/` (the `nativeLibraryDir`) stay executable. That is the
# one place a bundled binary can live, and the packaging rule is `lib*.so` — so
# `git` ships as `libgit.so`, `git-remote-https` as `libgit-remote-https.so`, and
# the Rust host writes thin `/system/bin/sh` wrappers under
# `<app data>/bin` that exec them (see `crates/atomcode-android/src/host.rs`).
#
# Runs on a Linux host (or WSL). On Windows:
#   wsl -d Ubuntu -e bash scripts/build-android-git.sh
#
# Requires (Linux): curl, tar, make, perl, a C toolchain from the LINUX Android
# NDK. The Windows NDK that `sdkmanager` installs ships only
# `prebuilt/windows-x86_64`, so WSL needs its own copy:
#   curl -LO https://dl.google.com/android/repository/android-ndk-r26b-linux.zip
#   unzip android-ndk-r26b-linux.zip -d ~/android-git-build/ndk
#
# Env:
#   ANDROID_NDK_LINUX_HOME  explicit Linux NDK path (wins)
#   ATOMCODE_ANDROID_GIT_BUILD_DIR  build tree (default ~/android-git-build)
#   ANDROID_ABI             arm64-v8a only for now (the other ABIs are not built)
set -euo pipefail
cd "$(dirname "$0")/.."

REPO="$PWD"
WORK="${ATOMCODE_ANDROID_GIT_BUILD_DIR:-$HOME/android-git-build}"
SRC="$WORK/src"
PREFIX="$WORK/prefix"
ABI="${ANDROID_ABI:-arm64-v8a}"
API="${ATOMCODE_ANDROID_API:-26}"
TARGET="aarch64-linux-android"

OPENSSL_VERSION="${OPENSSL_VERSION:-3.0.16}"
CURL_VERSION="${CURL_VERSION:-8.11.1}"
GIT_VERSION="${GIT_VERSION:-2.47.1}"
JQ_VERSION="${JQ_VERSION:-1.7.1}"
RG_VERSION="${RG_VERSION:-15.2.0}"
RUSTPYTHON_VERSION="${RUSTPYTHON_VERSION:-0.6.0}"
LIBFFI_VERSION="${LIBFFI_VERSION:-3.4.6}"

case "$ABI" in
  arm64-v8a)
    NDK_TRIPLE="aarch64-linux-android"
    OPENSSL_TARGET="android-arm64"
    ;;
  *)
    echo "error: ABI $ABI is not wired up (arm64-v8a only)" >&2
    exit 1
    ;;
esac
TARGET="$NDK_TRIPLE"

if [ "$(uname -s)" != "Linux" ]; then
  echo "error: this script cross-compiles on Linux; on Windows run it through WSL:" >&2
  echo "  wsl -d Ubuntu -e bash scripts/build-android-git.sh" >&2
  exit 1
fi

# --- NDK -------------------------------------------------------------------
has_linux_toolchain() {
  [ -d "$1/toolchains/llvm/prebuilt/linux-x86_64/bin" ]
}

ndk="${ANDROID_NDK_LINUX_HOME:-}"
if [ -z "$ndk" ] && [ -n "${ANDROID_NDK_HOME:-}" ] && has_linux_toolchain "$ANDROID_NDK_HOME"; then
  ndk="$ANDROID_NDK_HOME"
fi
if [ -z "$ndk" ] && [ -n "${ANDROID_HOME:-}" ] && [ -d "$ANDROID_HOME/ndk" ]; then
  for candidate in $(ls "$ANDROID_HOME/ndk" | sort -rV); do
    if has_linux_toolchain "$ANDROID_HOME/ndk/$candidate"; then
      ndk="$ANDROID_HOME/ndk/$candidate"
      break
    fi
  done
fi
if [ -z "$ndk" ] && [ -d "$WORK/ndk" ]; then
  ndk="$(find "$WORK/ndk" -maxdepth 2 -name 'android-ndk-*' -type d | sort -V | tail -1)"
fi
if [ -z "$ndk" ] || ! has_linux_toolchain "$ndk"; then
  echo "error: a Linux Android NDK is required." >&2
  echo "  The SDK-managed NDK only carries the host that installed it; on WSL that is" >&2
  echo "  windows-x86_64. Install the Linux NDK and point ANDROID_NDK_LINUX_HOME at it:" >&2
  echo "    curl -LO https://dl.google.com/android/repository/android-ndk-r26b-linux.zip" >&2
  echo "    unzip android-ndk-r26b-linux.zip -d $WORK/ndk" >&2
  exit 1
fi

toolchain="$ndk/toolchains/llvm/prebuilt/linux-x86_64"
bin="$toolchain/bin"
CC="$bin/${TARGET}${API}-clang"
CXX="$bin/${TARGET}${API}-clang++"
AR="$bin/llvm-ar"
RANLIB="$bin/llvm-ranlib"
STRIP="$bin/llvm-strip"
READELF="$bin/llvm-readelf"
for tool in "$CC" "$CXX" "$AR" "$RANLIB" "$STRIP"; do
  [ -e "$tool" ] || { echo "error: missing NDK tool $tool" >&2; exit 1; }
done

echo "→ ABI $ABI (API $API)"
echo "  NDK:   $ndk"
echo "  build: $WORK"

mkdir -p "$SRC" "$PREFIX"

# --- Rust toolchain (for ripgrep) ------------------------------------------
# ripgrep is Rust, so it cross-compiles with cargo + the NDK linker. The
# toolchain is expected on the host (Linux/WSL); if it is missing, say so
# plainly instead of failing deep inside cargo.
if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo is required to build ripgrep (rg)." >&2
  echo "  install Rust with: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
  exit 1
fi
if ! rustup target list --installed 2>/dev/null | grep -qx "$TARGET"; then
  echo "→ adding the Rust target $TARGET"
  rustup target add "$TARGET"
fi
# cargo links through the NDK's clang. The `--target=` link-arg is what the
# NDK wrapper passes; without it the linker looks for the host's libc.
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CC"
# compiler-rt: libffi's closure code calls `__clear_cache`, which bionic does
# not export. `-D__clear_cache=__builtin___clear_cache` only renames the call
# (the AArch64 backend lowers the builtin back to the same libcall), so the
# builtins archive has to be linked in explicitly.
COMPILER_RT="$ndk/toolchains/llvm/prebuilt/$(uname -s | tr 'A-Z' 'a-z' | sed 's/darwin/osx/')-x86_64/lib/clang/17/lib/linux/libclang_rt.builtins-aarch64-android.a"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="-C link-arg=--target=${TARGET}${API} -C link-arg=-L$PREFIX/lib"
if [ -f "$COMPILER_RT" ]; then
  export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="$CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS -C link-arg=$COMPILER_RT"
fi
# `cc`-based build scripts (libffi, psm's assembly) run under cargo too; cc-rs
# looks for `CC_<target with underscores>` and otherwise falls back to a bare
# `clang`, which is not on PATH.
export "CC_$(echo "$TARGET" | tr '-' '_')=$CC"
export "CXX_$(echo "$TARGET" | tr '-' '_')=$CXX"
export "AR_$(echo "$TARGET" | tr '-' '_')=$AR"

fetch() {
  url="$1"
  archive="$2"
  if [ -f "$SRC/$archive" ]; then
    return
  fi
  echo "→ downloading $archive"
  curl -L --fail --retry 3 -o "$SRC/$archive.part" "$url"
  mv "$SRC/$archive.part" "$SRC/$archive"
}

unpack() {
  archive="$1"
  dir="$2"
  if [ -d "$SRC/$dir" ]; then
    return
  fi
  echo "→ unpacking $archive"
  tar -xf "$SRC/$archive" -C "$SRC"
}

NPROC="$(nproc 2>/dev/null || echo 4)"

# --- OpenSSL (static) ------------------------------------------------------
# Android has no OpenSSL in the platform; libcurl's TLS needs one, and a
# dynamic dependency would mean shipping a second `.so`. Static keeps the
# APK's NEEDED list at system libraries only.
fetch "https://github.com/openssl/openssl/releases/download/openssl-${OPENSSL_VERSION}/openssl-${OPENSSL_VERSION}.tar.gz" "openssl-${OPENSSL_VERSION}.tar.gz"
unpack "openssl-${OPENSSL_VERSION}.tar.gz" "openssl-${OPENSSL_VERSION}"

if [ ! -f "$PREFIX/lib/libssl.a" ]; then
  echo "→ building openssl $OPENSSL_VERSION"
  # OpenSSL's `android-arm64` target looks for the *unversioned*
  # `aarch64-linux-android-clang`, which NDK r23+ stopped shipping (only
  # `…-android26-clang` exists). A symlink next to the versioned wrapper is
  # enough, and it has to live inside the NDK: the target validates that the
  # compiler it finds resolves under `$ANDROID_NDK_ROOT`.
  if [ ! -e "$bin/${NDK_TRIPLE}-clang" ]; then
    ln -sf "${NDK_TRIPLE}${API}-clang" "$bin/${NDK_TRIPLE}-clang"
    ln -sf "${NDK_TRIPLE}${API}-clang++" "$bin/${NDK_TRIPLE}-clang++"
  fi
  (
    # The generated Makefile records the compiler by bare name, so the NDK bin
    # dir has to stay on PATH for `make`, not just for `Configure`.
    export PATH="$bin:$PATH"
    cd "$SRC/openssl-${OPENSSL_VERSION}"
    if [ ! -f Makefile ]; then
      ANDROID_NDK_ROOT="$ndk" ./Configure "$OPENSSL_TARGET" \
        -D__ANDROID_API__="$API" \
        no-shared no-tests no-engine no-legacy \
        --prefix="$PREFIX" --openssldir="$PREFIX/ssl"
    fi
    make -j"$NPROC" build_sw
    make install_sw >/dev/null
  )
fi

# --- libcurl (static, HTTP only) ------------------------------------------
fetch "https://curl.se/download/curl-${CURL_VERSION}.tar.xz" "curl-${CURL_VERSION}.tar.xz"
unpack "curl-${CURL_VERSION}.tar.xz" "curl-${CURL_VERSION}"

if [ ! -f "$PREFIX/lib/libcurl.a" ]; then
  echo "→ building curl $CURL_VERSION"
  (
    cd "$SRC/curl-${CURL_VERSION}"
    if [ ! -f Makefile ]; then
      ./configure \
        --host="$TARGET" \
        CC="$CC" CXX="$CXX" AR="$AR" RANLIB="$RANLIB" \
        --prefix="$PREFIX" \
        --with-openssl="$PREFIX" \
        --with-zlib \
        --enable-static --disable-shared \
        --disable-ldap --disable-ldaps --disable-manual --disable-alt-svc \
        --disable-ntlm-wb --disable-threaded-resolver \
        --without-brotli --without-zstd --without-libidn2 --without-libpsl \
        --without-libssh2 --without-nghttp2 --without-nghttp3 --without-ngtcp2
    fi
    make -j"$NPROC"
    make install >/dev/null
  )
fi

# --- git ------------------------------------------------------------------
fetch "https://mirrors.edge.kernel.org/pub/software/scm/git/git-${GIT_VERSION}.tar.gz" "git-${GIT_VERSION}.tar.gz"
unpack "git-${GIT_VERSION}.tar.gz" "git-${GIT_VERSION}"

# Android executes only files in the APK's lib dir, and only `lib*.so` names get
# packaged there; git looks `git-remote-https` up by name. The patch maps a
# failed name lookup onto `<exec-path>/lib<name>.so`, so a helper can live under
# a name Gradle will ship.
if ! grep -q android_lib_program "$SRC/git-${GIT_VERSION}/run-command.c"; then
  echo "→ patching git for Android lib names"
  patch -d "$SRC/git-${GIT_VERSION}" -p1 --forward < "$REPO/scripts/patches/git-android-lib-lookup.patch"
fi

echo "→ building git $GIT_VERSION"
(
  cd "$SRC/git-${GIT_VERSION}"
  make clean >/dev/null 2>&1 || true
  # Static archives are named explicitly: `-lcurl` would find a host lib if one
  # were ever on the search path, and the link order is curl → ssl → crypto → z.
  #
  # `NO_GECOS_IN_PWENT`: bionic answers `getpwuid()` with a passwd entry whose
  # `pw_gecos` is NULL, and git's `copy_gecos` dereferences it — the crash lands
  # in `git checkout`/`clone` the first time an identity is needed. git's own
  # `config.mak.uname` sets this under its `Android` platform case; building as
  # plain Linux misses it.
  #
  # `NO_PTHREADS`: bionic has no `pthread_setcancelstate` at API 26, and git's
  # atfork guard calls it. `NO_EXPAT`: only WebDAV `http-push` needs it — smart
  # HTTP clone/pull/push run through `remote-curl`, which does not.
  # `NEEDS_LIBRT=`: git's Linux defaults add `-lrt`; bionic has `clock_gettime`
  # in libc and ships no librt.
  # `make install` re-runs the build, so it needs the exact same variables — a
  # second `make` with a different command line would rebuild `help.o` without
  # the curl include path and fail.
  make_args=(
    CC="$CC" AR="$AR" RANLIB="$RANLIB"
    CFLAGS="-O2 -fPIE -D__ANDROID_API__=$API"
    LDFLAGS="-pie"
    NO_GETTEXT=1 NO_TCLTK=1 NO_PERL=1 NO_PYTHON=1
    NO_OPENSSL=1 NO_ICONV=1 NO_INSTALL_HARDLINKS=1 NO_CROSS_COMPILE=
    NO_PTHREADS=1 NO_EXPAT=1 NO_GECOS_IN_PWENT=1
    NEEDS_LIBRT=
    CURL_CFLAGS="-I$PREFIX/include"
    CURL_LIBCURL="$PREFIX/lib/libcurl.a $PREFIX/lib/libssl.a $PREFIX/lib/libcrypto.a -lz"
    CURL_LDFLAGS="$PREFIX/lib/libcurl.a $PREFIX/lib/libssl.a $PREFIX/lib/libcrypto.a -lz"
    prefix="$PREFIX"
  )
  make -j"$NPROC" "${make_args[@]}" all
  make "${make_args[@]}" install >/dev/null
)

# --- jq (JSON processor) ----------------------------------------------------
# Android's system image has no `jq`, and there is no package manager to get
# one. jq is a small C program whose only dependency is oniguruma (bundled in
# its `modules/` dir), so it cross-compiles with the NDK alone.
fetch "https://github.com/jqlang/jq/releases/download/jq-${JQ_VERSION}/jq-${JQ_VERSION}.tar.gz" \
      "jq-${JQ_VERSION}.tar.gz"
unpack "jq-${JQ_VERSION}.tar.gz" "jq-${JQ_VERSION}"

if [ ! -f "$PREFIX/lib/libonig.a" ]; then
  echo "→ building oniguruma (jq's regex engine)"
  (
    cd "$SRC/jq-${JQ_VERSION}/modules/oniguruma"
    autoreconf -i >/dev/null 2>&1 || true
    ./configure \
      --host="$TARGET" --prefix="$PREFIX" \
      --disable-shared --enable-static \
      CC="$CC" AR="$AR" RANLIB="$RANLIB"
    make -j"$NPROC"
    make install >/dev/null
  )
fi

if [ ! -f "$PREFIX/bin/jq" ]; then
  echo "→ building jq $JQ_VERSION"
  (
    cd "$SRC/jq-${JQ_VERSION}"
    autoreconf -fi >/dev/null 2>&1 || true
    ./configure \
      --host="$TARGET" --prefix="$PREFIX" \
      --disable-shared --enable-static \
      --with-oniguruma="$PREFIX" \
      CC="$CC" CXX="$CXX" AR="$AR" RANLIB="$RANLIB"
    make -j"$NPROC"
    make install >/dev/null
  )
fi

# --- ripgrep (fast, gitignore-aware search) ---------------------------------
# The system has `grep` but no `rg`; ripgrep is Rust, so it cross-compiles with
# cargo + the NDK linker. `rg` is the tool an agent reaches for when it wants
# to search a tree and honour .gitignore.
if [ ! -f "$PREFIX/bin/rg" ]; then
  echo "→ building ripgrep $RG_VERSION"
  (
    cd "$SRC"
    cargo install ripgrep --version "$RG_VERSION" --locked \
      --target "$TARGET" --root "$PREFIX" --no-default-features --features 'pcre2,simd'
    # cargo puts the binary under <root>/bin.
  )
fi

# --- libffi (RustPython's `ctypes` links it) --------------------------------
# The NDK does not ship libffi, and `rustpython-stdlib`'s ctypes module links
# `-lffi` unconditionally. Without a static copy here the RustPython link dies
# with `ld.lld: error: unable to find library -lffi`.
if [ ! -f "$PREFIX/lib/libffi.a" ]; then
  echo "→ building libffi $LIBFFI_VERSION"
  fetch "https://github.com/libffi/libffi/releases/download/v${LIBFFI_VERSION}/libffi-${LIBFFI_VERSION}.tar.gz" \
        "libffi-${LIBFFI_VERSION}.tar.gz"
  unpack "libffi-${LIBFFI_VERSION}.tar.gz" "libffi-${LIBFFI_VERSION}"
  (
    cd "$SRC/libffi-${LIBFFI_VERSION}"
    ./configure --host="$TARGET" --prefix="$PREFIX" \
      --disable-shared --enable-static \
      CC="$CC" AR="$AR" RANLIB="$RANLIB"
    make -j"$NPROC"
    make install >/dev/null
  )
fi

# --- RustPython (the bundled `python3`) --------------------------------------
# A phone has no Python and no way to install one. RustPython is a Python 3
# interpreter written in Rust, so it cross-compiles with the same cargo + NDK
# linker that already builds ripgrep.
#
# `freeze-stdlib` matters: it bakes the Python standard library into the
# executable, so the APK carries ONE file instead of a binary plus a `Lib/`
# tree that would have to be extracted and laid out at first run. Trade-off is
# binary size (~20 MB) for a self-contained `python3`.
#
# Features are chosen to avoid C toolchain dependencies that do not
# cross-compile cleanly: no `mimalloc` (a C allocator), no `ssl-*` (aws-lc needs
# cmake/nasm on the build host). Everything Python-level — `os`, `sys`,
# `json`, `re`, `subprocess` — is still there.
if [ ! -f "$PREFIX/bin/python3" ]; then
  echo "→ building RustPython (python3) $RUSTPYTHON_VERSION"
  (
    cd "$SRC"
    cargo install rustpython --version "$RUSTPYTHON_VERSION" \
      --target "$TARGET" --root "$PREFIX" \
      --no-default-features \
      --features 'freeze-stdlib,stdio,threading,host_env,importlib'
    # cargo installs it as `<root>/bin/rustpython`; give it the name the world
    # expects so `python3` and `python` resolve.
    mv "$PREFIX/bin/rustpython" "$PREFIX/bin/python3"
    ln -sf python3 "$PREFIX/bin/python"
  )
fi

# --- stage into jniLibs ----------------------------------------------------
dest="$REPO/android/app/src/main/jniLibs/$ABI"
mkdir -p "$dest"

stage() {
  from="$1"
  to="$2"
  if [ ! -f "$from" ]; then
    echo "error: missing build output $from" >&2
    exit 1
  fi
  cp "$from" "$dest/$to"
  "$STRIP" --strip-unneeded "$dest/$to"
}

stage "$PREFIX/bin/git" "libgit.so"
# git shells out to this for https; `git-remote-http` is the same binary in git's
# own install (a hardlink), so it is staged once and both shims point at it.
if [ "$(sha256sum "$PREFIX/libexec/git-core/git-remote-https" | cut -d' ' -f1)" != \
     "$(sha256sum "$PREFIX/libexec/git-core/git-remote-http" | cut -d' ' -f1)" ]; then
  echo "warning: git-remote-http differs from git-remote-https; staging both" >&2
  stage "$PREFIX/libexec/git-core/git-remote-http" "libgit-remote-http.so"
fi
stage "$PREFIX/libexec/git-core/git-remote-https" "libgit-remote-https.so"

# The extra command-line tools. Android has no package manager, so these are
# the only way the phone gets them: they ship in the library directory under
# `lib*.so` names (Gradle only packages those) and the host links them into
# `<data>/bin` under their plain names.
stage "$PREFIX/bin/jq" "libjq.so"
stage "$PREFIX/bin/rg" "librg.so"
# curl is already built as a dependency of git; it is staged as a tool too.
stage "$PREFIX/bin/curl" "libcurl.so"
# Python 3 (RustPython). Its stdlib is frozen in, so one file is the whole
# interpreter — no `Lib/` tree to extract on the device.
stage "$PREFIX/bin/python3" "libpython3.so"

echo "→ staged into $dest"
for f in libgit.so libgit-remote-https.so libgit-remote-http.so libjq.so librg.so libcurl.so libpython3.so; do
  [ -f "$dest/$f" ] || continue
  printf '  %-26s %s\n' "$f" "$(du -h "$dest/$f" | cut -f1)"
  "$READELF" -d "$dest/$f" | sed -n 's/.*NEEDED.*\[\(.*\)\]/    needs \1/p'
done

echo "→ done: bash scripts/build-android.sh --apk next"
