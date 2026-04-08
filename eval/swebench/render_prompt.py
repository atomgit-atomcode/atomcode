#!/usr/bin/env python3
"""Render a SWE-bench prompt template with instance-specific values.

Reads instance JSON from stdin, renders the named template from
`prompts/<name>.md`, prints the final prompt string to stdout.

Usage:
    echo '{...instance json...}' | render_prompt.py --template default --include-hints

Flags:
    --template <name>         Template name under prompts/ (default: default)
    --include-hints           Render {hints_block} with hints_text if non-empty
    --no-include-hints        Force {hints_block} to empty string (default)
"""
import argparse
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
PROMPTS_DIR = HERE / "prompts"


def build_hints_block(hints_text: str, include_hints: bool) -> str:
    """Conditionally render the hints section.

    Returns empty string when include_hints is False or hints_text is empty.
    Otherwise returns the full block with leading blank line.
    """
    if not include_hints:
        return ""
    trimmed = (hints_text or "").strip()
    if not trimmed:
        return ""
    return (
        "\n\n--- HINTS (developer comments from the original PR) ---\n"
        f"{trimmed}\n"
        "--- END HINTS ---"
    )


def render(template: str, instance: dict, include_hints: bool) -> str:
    """Render the named template with instance values. Raises on missing fields."""
    template_path = PROMPTS_DIR / f"{template}.md"
    if not template_path.exists():
        raise FileNotFoundError(f"template not found: {template_path}")

    required_keys = ("instance_id", "repo", "base_commit", "problem_statement")
    for key in required_keys:
        if key not in instance:
            raise KeyError(f"instance missing required field: {key}")

    base_commit = instance["base_commit"]
    ctx = {
        "instance_id": instance["instance_id"],
        "repo": instance["repo"],
        "base_commit": base_commit,
        "base_commit_short": base_commit[:8],
        "problem_statement": instance["problem_statement"],
        "hints_text": instance.get("hints_text", "") or "",
        "hints_block": build_hints_block(instance.get("hints_text", ""), include_hints),
    }

    body = template_path.read_text(encoding="utf-8")
    return body.format(**ctx)


def main() -> int:
    parser = argparse.ArgumentParser(description="Render SWE-bench prompt template.")
    parser.add_argument("--template", default="default")
    parser.add_argument("--include-hints", dest="include_hints", action="store_true")
    parser.add_argument("--no-include-hints", dest="include_hints", action="store_false")
    parser.set_defaults(include_hints=False)
    args = parser.parse_args()

    try:
        instance = json.load(sys.stdin)
    except json.JSONDecodeError as e:
        print(f"error: invalid instance JSON on stdin: {e}", file=sys.stderr)
        return 2

    try:
        out = render(args.template, instance, args.include_hints)
    except (KeyError, FileNotFoundError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 2

    sys.stdout.write(out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
