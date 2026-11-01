#!/usr/bin/env bash
# Package a provider's read-only half the way the market expects it: one tarball
# whose root also carries `manifest.json`.
#
# A provider is split in two by Android, not by taste: the programs must live in
# the APK's `nativeLibraryDir` (the only executable place), while everything the
# programs *read* — GOROOT's sources, cmake's module set, node's npm tree, and the
# versioned `.so`s Gradle refuses to package — is a data package. This script is
# the data half's packager; `scripts/build-android-tool-provider.sh` builds the
# APK half.
#
# Usage:
#   bash scripts/build-android-data-package.sh <name> <version> <staged-dir> [link]
#
#   <staged-dir>  what the installer extracts — the package root, i.e. its contents
#                 become `<product>/<link>/…`
#   <link>        the directory name under `<product>/` (default: <name>). The
#                 manifest's `links` map is what the market installer honours; a
#                 provider's data is visible to the app either way, because the
#                 host scans `<product>/*/libs` for loader paths.
#
# Output: dist/<name>-<version>-arm64.tar.gz + its sha256 on stdout.
set -euo pipefail
cd "$(dirname "$0")/.."

name=${1:?usage: build-android-data-package.sh <name> <version> <staged-dir> [link]}
version=${2:?missing version}
staged=${3:?missing staged directory}
link=${4:-$name}

[ -d "$staged" ] || { echo "error: no such staged directory: $staged" >&2; exit 1; }

bundle="dist/$name-$version-arm64.tar.gz"
mkdir -p dist
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp -a "$staged"/. "$work"/

# `entry` is empty on purpose: nothing here is a plugin to `dlopen`. The app links
# `<product>/<link>` at this package's root, which is the layout each tool expects
# by its own convention (GOROOT, a cmake prefix, a node prefix).
cat > "$work/manifest.json" <<EOF
{
  "schema": 1,
  "name": "$name",
  "version": "$version",
  "entry": "",
  "links": { "$link": "" }
}
EOF

tar -czf "$bundle" -C "$work" .
echo "→ $bundle ($(du -h "$bundle" | cut -f1))"
echo "  sha256 $(sha256sum "$bundle" | cut -d' ' -f1)"
