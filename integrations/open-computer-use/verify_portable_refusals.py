#!/usr/bin/env python3
"""Exercise the real portable-build CLI's no-write refusal paths; no Go invocation."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile


def run(source, report):
    source = source.resolve(strict=True)
    verifier = Path(__file__).with_name("verify_portable_builds.py")
    evidence = dict(checks=[], gui_actions=0, target_binaries_executed=False)
    with tempfile.TemporaryDirectory(prefix="ac-portable-refusal-") as temporary:
        root = Path(temporary)
        checkout = root / "source"
        subprocess.run(["git", "clone", "--shared", "--no-checkout", str(source), str(checkout)],
                       check=True, capture_output=True)
        subprocess.run(["git", "-C", str(checkout), "checkout", "--detach",
                        "e551c13e7082ce4c097234137582fbf4a1bc647a"], check=True, capture_output=True)

        def refuse(output, expected):
            result = subprocess.run(["python3", str(verifier), "--source", str(checkout), "--output", str(output)],
                                    capture_output=True, text=True, timeout=20)
            assert result.returncode != 0, result.stdout
            assert expected in result.stderr, result.stderr

        existing = root / "existing"
        existing.mkdir()
        marker = existing / "keep"
        marker.write_bytes(b"preserve this output")
        refuse(existing, "output exists")
        assert list(existing.iterdir()) == [marker] and marker.read_bytes() == b"preserve this output"
        evidence["checks"].append("existing output preserved")

        nested = checkout / "nested-build"
        refuse(nested, "outside the source")
        assert not nested.exists()
        evidence["checks"].append("nested source output refused without writes")

        path = checkout / "apps/OpenComputerUseLinux/main.go"
        original = path.read_bytes()
        path.write_bytes(original + b"\n// refusal fixture\n")
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        output = root / "dirty-output"
        refuse(output, "source must be clean")
        assert not output.exists() and hashlib.sha256(path.read_bytes()).hexdigest() == digest
        evidence["checks"].append("dirty source preserved and refused")

        subprocess.run(["git", "-C", str(checkout), "update-index", "--assume-unchanged",
                        "apps/OpenComputerUseLinux/main.go"], check=True)
        assert not subprocess.check_output(["git", "-C", str(checkout), "status", "--porcelain"])
        output = root / "hidden-output"
        refuse(output, "input differs from pinned commit")
        assert not output.exists() and hashlib.sha256(path.read_bytes()).hexdigest() == digest
        evidence["checks"].append("index-hidden modified bytes refused against committed blob")
        path.write_bytes(original)
        subprocess.run(["git", "-C", str(checkout), "update-index", "--no-assume-unchanged",
                        "apps/OpenComputerUseLinux/main.go"], check=True)
        hooks = root / "empty-hooks"
        hooks.mkdir()
        subprocess.run(["git", "-C", str(checkout), "-c", "core.hooksPath=" + str(hooks),
                        "-c", "commit.gpgsign=false", "-c", "user.name=Acceptance Fixture",
                        "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m",
                        "local wrong-revision refusal fixture"], check=True, capture_output=True)
        output = root / "wrong-revision"
        refuse(output, "pinned upstream commit")
        assert not output.exists()
        evidence["checks"].append("wrong clean revision refused without writes")
    evidence["passed"] = True
    report.write_text(json.dumps(evidence, indent=2) + "\n")
    print("Passed 5 portable-build refusal groups; no Go build or GUI action requested")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    run(args.source, args.report)
