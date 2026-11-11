#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
    echo "Usage: $0 CLEAN_SOURCE_DIR SCRATCH_BUILD_DIR SWIFT_EXECUTABLE" >&2
    exit 2
fi
source_dir=$1
scratch_dir=$2
swift_bin=$3
script_dir=$(cd -- "$(dirname -- "$0")" && pwd)
expected=e551c13e7082ce4c097234137582fbf4a1bc647a

[[ $(uname -s) == Darwin ]] || { echo 'This experimental backend patch is macOS-only.' >&2; exit 2; }
[[ -x $swift_bin ]] || { echo 'Provide an installed Swift 6.1+ executable.' >&2; exit 2; }
[[ $(git -C "$source_dir" rev-parse HEAD) == "$expected" ]] || {
    echo 'Source must be the pinned Open Computer Use v1.3.0 commit.' >&2
    exit 2
}
[[ -z $(git -C "$source_dir" status --porcelain) ]] || {
    echo 'Source is dirty; preserve it and provide a separate clean checkout. Nothing was reset.' >&2
    exit 2
}
git -C "$source_dir" apply --check --unidiff-zero "$script_dir/desktop-turn-lease.patch"
git -C "$source_dir" apply --unidiff-zero "$script_dir/desktop-turn-lease.patch"
mkdir -p "$scratch_dir"
python3 "$script_dir/build_provenance.py" snapshot --source "$source_dir" \
    --patch "$script_dir/desktop-turn-lease.patch" --output "$scratch_dir/atomcode-source-inputs.json"
"$swift_bin" build --package-path "$source_dir" --scratch-path "$scratch_dir" \
    --disable-sandbox --product OpenComputerUse
"$swift_bin" run --package-path "$source_dir" --scratch-path "$scratch_dir" \
    --disable-sandbox OpenComputerUseLeaseSmoke
sdk_path=$(xcrun --sdk macosx --show-sdk-path)
"${swift_bin%/*}/swiftc" -sdk "$sdk_path" -swift-version 6 \
    "$source_dir/apps/OpenComputerUse/Sources/OpenComputerUse/AppAgentNamespace.swift" \
    "$source_dir/tests/AppAgentNamespaceSmoke/main.swift" \
    -o "$scratch_dir/app-agent-namespace-smoke"
"$scratch_dir/app-agent-namespace-smoke"
bin_dir=$("$swift_bin" build --package-path "$source_dir" --scratch-path "$scratch_dir" --show-bin-path)
python3 "$script_dir/build_provenance.py" finalize --source "$source_dir" \
    --patch "$script_dir/desktop-turn-lease.patch" --snapshot "$scratch_dir/atomcode-source-inputs.json" \
    --backend "$bin_dir/OpenComputerUse" --swift "$swift_bin" --output "$scratch_dir/atomcode-build-provenance.json"
printf '\nBuilt an experimental backend only. No installed App or MCP configuration was changed.\n'
printf 'Run verify_lease.py with --backend pointing to the built OpenComputerUse executable.\n'
printf 'Package a separate development App with package-macos-lease-app.py; no App is installed automatically.\n'
