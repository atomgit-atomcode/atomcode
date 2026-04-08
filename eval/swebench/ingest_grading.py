#!/usr/bin/env python3
"""Ingest upstream SWE-bench grader output into per-instance meta.json.

The upstream grader writes a JSON report. We read it, find each
instance's case dir in the run, and update meta.json with:

    status:                 "resolved" or "unresolved"
    swebench_resolved:      bool
    swebench_failure_mode:  str or null
    graded_at:              ISO timestamp

Non-swebench meta.json (form != "swebench") is left alone.
"""
import argparse
import json
import sys
from datetime import datetime, timezone
from pathlib import Path


def parse_grader_report(report: dict) -> dict:
    """Extract {instance_id: {resolved, failure_mode}} from the grader's report shape."""
    out = {}
    for iid, info in report.get("report", {}).items():
        out[iid] = {
            "resolved": bool(info.get("resolved")),
            "failure_mode": info.get("failure_mode"),
        }
    return out


def update_meta(meta_path: Path, resolved: bool, failure_mode, now: str) -> None:
    """Update a single meta.json in place."""
    with open(meta_path, "r", encoding="utf-8") as f:
        meta = json.load(f)

    if meta.get("form") != "swebench":
        return  # leave non-swebench metas alone

    meta["status"] = "resolved" if resolved else "unresolved"
    meta["swebench_resolved"] = resolved
    meta["swebench_failure_mode"] = failure_mode if not resolved else None
    meta["graded_at"] = now

    with open(meta_path, "w", encoding="utf-8") as f:
        json.dump(meta, f, indent=2)


def main() -> int:
    parser = argparse.ArgumentParser(description="Ingest grader report → meta.json")
    parser.add_argument("--run-dir", required=True, type=Path)
    parser.add_argument("--grader-report", required=True, type=Path)
    args = parser.parse_args()

    if not args.run_dir.is_dir():
        print(f"error: run dir not found: {args.run_dir}", file=sys.stderr)
        return 2
    if not args.grader_report.exists():
        print(f"error: grader report not found: {args.grader_report}", file=sys.stderr)
        return 2

    with open(args.grader_report, "r", encoding="utf-8") as f:
        report = json.load(f)

    per_instance = parse_grader_report(report)
    if not per_instance:
        print("warning: grader report contains no per-instance results", file=sys.stderr)
        return 0

    now = datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")

    updated = 0
    for instance_id, result in per_instance.items():
        meta_path = args.run_dir / instance_id / "meta.json"
        if not meta_path.exists():
            print(f"warning: no meta.json for {instance_id}, skipping", file=sys.stderr)
            continue
        update_meta(meta_path, result["resolved"], result["failure_mode"], now)
        updated += 1

    print(f"updated {updated} meta.json files")
    return 0


if __name__ == "__main__":
    sys.exit(main())
