#!/usr/bin/env python3
"""Local build measurements, not a publisher-signed attestation."""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import subprocess

PIN = "e551c13e7082ce4c097234137582fbf4a1bc647a"
INPUTS = ["apps/OpenComputerUse/Sources/OpenComputerUse", "packages/OpenComputerUseKit/Sources/OpenComputerUseKit",
          "packages/OpenComputerUseKit/Tests/DesktopLeaseSmoke", "tests/AppAgentNamespaceSmoke"]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def snapshot(source, patch):
    commit = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    if commit != PIN:
        raise ValueError("source is not the pinned upstream commit")
    files = {"Package.swift": digest(source / "Package.swift")}
    for directory in INPUTS:
        paths = sorted((source / directory).rglob("*.swift"))
        if not paths:
            raise ValueError("missing build inputs: " + directory)
        for path in paths:
            files[str(path.relative_to(source))] = digest(path)
    return dict(upstream_commit=commit, source_patch_sha256=digest(patch), inputs=files)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["snapshot", "finalize"])
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--patch", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--snapshot", type=Path)
    parser.add_argument("--backend", type=Path)
    parser.add_argument("--swift", type=Path)
    args = parser.parse_args()
    current = snapshot(args.source, args.patch)
    if args.action == "finalize":
        if not args.snapshot or not args.backend or not args.swift:
            parser.error("finalize needs --snapshot, --backend and --swift")
        if current != json.loads(args.snapshot.read_text()):
            raise ValueError("source or patch changed during build; no verified provenance written")
        current.update(schema_version=1, executable_sha256=digest(args.backend),
                       swift=subprocess.check_output([str(args.swift), "--version"], text=True).strip(),
                       sdk=subprocess.check_output(["/usr/bin/xcrun", "--sdk", "macosx", "--show-sdk-version"], text=True).strip(),
                       os_version=platform.mac_ver()[0], architecture=platform.machine(),
                       scope="local source/binary measurements after product and smoke checks; not a signed release attestation")
    args.output.write_text(json.dumps(current, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
