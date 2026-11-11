#!/usr/bin/env python3
"""Run refusal checks against the actual packager; all outputs are test-owned."""
import argparse
import json
from pathlib import Path
import subprocess
import sys
import tempfile


def run(backend, manifest, report):
    script = Path(__file__).with_name("package-macos-lease-app.py")
    with tempfile.TemporaryDirectory(prefix="ac-package-check-", dir="/tmp") as root:
        root = Path(root)
        app, provenance = root / "Test.app", root / "provenance.json"
        base = [sys.executable, str(script), "--backend", str(backend), "--build-provenance", str(manifest),
                "--output", str(app), "--report", str(provenance)]
        def refused(args, fragment):
            result = subprocess.run(args, capture_output=True, text=True, timeout=15)
            assert result.returncode != 0 and fragment in result.stderr, result.stderr
        # An output cannot be silently overwritten.
        app.mkdir()
        sentinel = app / "sentinel"
        sentinel.write_text("preserve")
        refused(base, "output already exists")
        assert sentinel.read_text() == "preserve" and not provenance.exists()
        sentinel.unlink()
        app.rmdir()
        # A current capability declaration alone is not build provenance.
        stale = root / "wrong-patch.json"
        data = json.loads(manifest.read_text())
        data["source_patch_sha256"] = "0" * 64
        stale.write_text(json.dumps(data))
        bad = base.copy(); bad[bad.index(str(manifest))] = str(stale)
        refused(bad, "current pinned source patch")
        assert not app.exists() and not provenance.exists()
        changed = root / "changed-backend"
        changed.write_bytes(backend.read_bytes() + b"changed")
        changed.chmod(0o700)
        bad = base.copy(); bad[bad.index(str(backend))] = str(changed)
        refused(bad, "binary does not match")
        assert not app.exists() and not provenance.exists()
        provenance.write_text("existing report")
        refused(base, "provenance output already exists")
        assert provenance.read_text() == "existing report" and not app.exists()
    result = dict(passed=True, checks=["existing App preserved", "stale patch manifest refused", "changed binary refused before execution", "existing report preserved"], gui_actions=0)
    report.write_text(json.dumps(result, indent=2) + "\n")
    print("Passed 4 package refusal groups; no App installed or GUI action requested")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backend", type=Path, required=True)
    parser.add_argument("--build-provenance", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    run(args.backend.resolve(strict=True), args.build_provenance.resolve(strict=True), args.report)
