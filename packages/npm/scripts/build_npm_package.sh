#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────
# AtomCode npm package build script
# Usage:  ./scripts/build_npm_package.sh <version>
# Example: ./scripts/build_npm_package.sh 4.23.3 --dry-run
#          ./scripts/build_npm_package.sh 4.23.3
# ──────────────────────────────────────────────────────────────────────
set -euo pipefail

B="https://api.atomgit.com/api/v5"
ATOMGIT_TOKEN="${ATOMGIT_TOKEN:-}"

# ── version auto-detection (same helpers as packages/homebrew/scripts/package-tar-gz.sh) ──
et(){
    command -v curl &>/dev/null || return 1
    command -v jq &>/dev/null && return 0
    j=jq-macos-amd64; [[ $(uname -m) == arm64 ]] && j=jq-macos-arm64
    g="https://github.com/jqlang/jq/releases/download/jq-1.7.1/$j"
    d=$(mktemp -d) || return 1; p=$d/jq
    for u in "${ATOMGIT_JQ_URL:-}" "$g" "https://ghfast.top/$g"; do
        [[ $u ]] || continue
        curl -fsSL --connect-timeout 40 --retry 3 "$u" -o "$p" || continue
        s=$(stat -f%z "$p" 2>/dev/null || echo 0)
        [[ $s -ge 80000 ]] || { rm -f "$p"; continue; }
        chmod +x "$p" || { rm -f "$p"; continue; }
        "$p" -n . &>/dev/null || { rm -f "$p"; continue; }
        export PATH="$d:$PATH"
        command -v jq &>/dev/null && return 0
    done
    rm -rf "$d"; return 1
}

fct(){ curl -sS -H "PRIVATE-TOKEN: $ATOMGIT_TOKEN" -H "Accept: application/json" \
    "$B/repos/atomgit_atomcode/atomcode/contents/Cargo.toml?ref=main"; }

pvs(){
    local t v
    t=$(jq -er 'select(.type=="file")|.content|gsub("[[:space:]]";"")|@base64d' <<<"$1")
    v=$(awk 'BEGIN{f=0} index($0,"[workspace.package]")==1{w=1;next} substr($0,1,1)=="["{w=0}
        w&&/^version *=/{if(match($0,/"[^"]+"/)){print substr($0,RSTART+1,RLENGTH-2);f=1;exit}}
        END{exit !f}' <<<"$t")
    [[ $v ]] || exit 1; echo "$v"
}

# ── resolve version ──
if [ $# -ge 1 ] && [[ "$1" != -* ]]; then
    VERSION="$1"
    shift
else
    et || { echo "Error: jq not available"; exit 1; }
    j=$(fct) || { echo "Error: failed to fetch Cargo.toml"; exit 1; }
    jq -e .error_code <<<"$j" &>/dev/null && { echo "Error fetching Cargo.toml: $(echo "$j" | jq -r .message)"; exit 1; }
    VERSION=$(pvs "$j") || { echo "Error: failed to parse version from Cargo.toml"; exit 1; }
fi

# Capture remaining args for npm publish (e.g. --dry-run, --otp=...)
NPM_EXTRA="${*:-}"

NPM_DIR="$(cd "$(dirname "$0")/.." && pwd)"
WORK_DIR=$(mktemp -d)
cleanup() { rm -rf "$WORK_DIR"; }
trap cleanup EXIT

PLATFORMS=(
  "darwin-arm64:darwin:arm64"
  "darwin-x64:darwin:x64"
  "linux-arm64:linux:arm64"
  "linux-x64:linux:x64"
  "win32-x64:win32:x64"
  "ohos-arm64:ohos:arm64"
)

publish_platform() {
  local tag="$1" os="$2" arch="$3"
  local dir="$WORK_DIR/$tag"
  mkdir -p "$dir/bin"

  # generate package.json dynamically — 就几行
  cat > "$dir/package.json" <<EOF
{"name":"@atomgit.com/atomcode","version":"${VERSION}-${tag}","os":["${os}"],"cpu":["${arch}"],"files":["bin/"]}
EOF

  # download binary
  local dl_os="$os"
  [ "$os" = "win32" ] && dl_os="windows"
  local bin_name="atomcode$([ "$os" = "win32" ] && echo ".exe")"
  local url="https://atomgit.com/atomgit_atomcode/atomcode/releases/download/v${VERSION}/atomcode-v${VERSION}-${dl_os}-${arch}$([ "$os" = "win32" ] && echo ".exe")"

  echo "  ↓ downloading ${tag}..."
  local http_code
  if [ -n "$ATOMGIT_TOKEN" ]; then
    http_code=$(curl -fsSL -w '%{http_code}' -H "PRIVATE-TOKEN: $ATOMGIT_TOKEN" --connect-timeout 30 --retry 3 "$url" -o "$dir/bin/$bin_name" 2>/dev/null)
  else
    http_code=$(curl -fsSL -w '%{http_code}' --connect-timeout 30 --retry 3 "$url" -o "$dir/bin/$bin_name" 2>/dev/null)
  fi
  if [ "$http_code" = "404" ]; then
    echo "  ⚠ binary not found for ${tag}, skipping"
    rm -f "$dir/bin/$bin_name"
    return 0
  fi
  chmod +x "$dir/bin/$bin_name"

  # publish
  cd "$dir"
  npm publish --registry=https://registry.npmjs.org/ --access public $NPM_EXTRA
  echo "  ✓ @atomgit.com/atomcode@${VERSION}-${tag}"
}

echo ""
echo "  Publishing @atomgit.com/atomcode v${VERSION}"
echo ""

# 1. publish platform versions
for entry in "${PLATFORMS[@]}"; do
  IFS=: read -r tag os arch <<< "$entry"
  publish_platform "$tag" "$os" "$arch"
done

# 2. publish core
CORE_DIR="$WORK_DIR/core"
mkdir -p "$CORE_DIR/bin"
cp "$NPM_DIR/package.json" "$CORE_DIR/"
cp "$NPM_DIR/bin/atomcode.js" "$CORE_DIR/bin/"
cd "$CORE_DIR"
# Inject version + optionalDependencies dynamically (like Codex does in CI)
node -e "
var fs = require('fs');
var pkg = JSON.parse(fs.readFileSync('package.json', 'utf8'));
pkg.version = '$VERSION';
pkg.optionalDependencies = {
  '@atomgit.com/atomcode-darwin-arm64': 'npm:@atomgit.com/atomcode@$VERSION-darwin-arm64',
  '@atomgit.com/atomcode-darwin-x64': 'npm:@atomgit.com/atomcode@$VERSION-darwin-x64',
  '@atomgit.com/atomcode-linux-arm64': 'npm:@atomgit.com/atomcode@$VERSION-linux-arm64',
  '@atomgit.com/atomcode-linux-x64': 'npm:@atomgit.com/atomcode@$VERSION-linux-x64',
  '@atomgit.com/atomcode-win32-x64': 'npm:@atomgit.com/atomcode@$VERSION-win32-x64',
  '@atomgit.com/atomcode-ohos-arm64': 'npm:@atomgit.com/atomcode@$VERSION-ohos-arm64'
};
fs.writeFileSync('package.json', JSON.stringify(pkg, null, 2) + '\n');
"
npm publish --registry=https://registry.npmjs.org/ --access public $NPM_EXTRA
echo "  ✓ @atomgit.com/atomcode@${VERSION} (core)"
echo ""
echo "  All done!"
