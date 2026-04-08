#!/usr/bin/env python3
"""Parse a case file (form A: single .md / form B: <case>/case.md), validate
its TOML frontmatter, and emit one line of JSON describing the case.

Always exits 0 — even on validation errors. Bash callers should read the
"ok" key in the JSON to decide. This lets the runner collect all errors at
once instead of dying on the first bad case.

Usage:
    python3 parse_case.py <path-to-case-file>

Output:
    {"ok": true, "id": "...", "form": "A", ...}             # success
    {"ok": false, "errors": [...], "case_path": "..."}      # validation failure
"""
import json
import re
import sys
import tomllib
from pathlib import Path

ID_RE = re.compile(r"^[a-zA-Z0-9_-]+$")
FRONTMATTER_DELIM = "+++"


def parse_frontmatter(text: str):
    """Return (frontmatter_str, body_str). Raises ValueError if malformed.

    Handles:
    - Standard '+++\\n<toml>\\n+++\\n<body>'
    - Trailing-no-newline '+++\\n<toml>\\n+++' (body is empty)
    - Empty frontmatter '+++\\n+++\\n<body>' (toml is empty -> validation errors)
    """
    if not text.startswith(FRONTMATTER_DELIM + "\n"):
        raise ValueError("file must start with '+++' frontmatter delimiter")
    rest = text[len(FRONTMATTER_DELIM) + 1:]

    # Special case: empty frontmatter — closing '+++' immediately follows opening.
    if rest.startswith(FRONTMATTER_DELIM + "\n"):
        return ("", rest[len(FRONTMATTER_DELIM) + 1:])
    if rest == FRONTMATTER_DELIM or rest == FRONTMATTER_DELIM + "\n":
        return ("", "")

    # Standard case: search for newline-prefixed closing delimiter.
    end_idx = rest.find("\n" + FRONTMATTER_DELIM + "\n")
    if end_idx == -1:
        # Trailing-no-newline case: closing '+++' is the last token in the file.
        end_idx = rest.find("\n" + FRONTMATTER_DELIM)
        if end_idx == -1 or rest[end_idx + 1 + len(FRONTMATTER_DELIM):].strip():
            raise ValueError("frontmatter has no closing '+++' delimiter")
        # Body is whatever follows '\n+++' — typically empty or whitespace.
        fm_text = rest[:end_idx]
        body = rest[end_idx + 1 + len(FRONTMATTER_DELIM):]
        return fm_text, body

    # Standard case: skip past '\n+++\n' (1 newline + 3 plusses + 1 newline = 5 chars).
    fm_text = rest[:end_idx]
    body = rest[end_idx + 2 + len(FRONTMATTER_DELIM):]  # +2 = "\n" + "\n", +3 = "+++"
    return fm_text, body


def detect_form(case_path: Path):
    """Decide form A vs B based on whether case_path is a top-level .md or <dir>/case.md.

    Returns (form_letter, seed_dir_or_none).
    """
    if case_path.name == "case.md":
        seed = case_path.parent / "seed"
        return ("B", seed if seed.is_dir() else None)
    return ("A", None)


def expected_id(case_path: Path, form: str) -> str:
    if form == "A":
        return case_path.stem
    return case_path.parent.name


def validate(meta: dict, body: str, case_path: Path, form: str, seed_dir):
    """Returns list of error strings (empty if valid)."""
    errs = []

    # id
    case_id = meta.get("id")
    if not case_id:
        errs.append("required field 'id' is missing or empty")
    else:
        if not isinstance(case_id, str):
            errs.append("'id' must be a string")
        else:
            if not ID_RE.match(case_id):
                errs.append("'id' has invalid charset (allowed: alpha-numeric, underscore, hyphen)")
            exp = expected_id(case_path, form)
            if case_id != exp:
                if form == "A":
                    errs.append(f"'id' must match filename: expected '{exp}', got '{case_id}'")
                else:
                    errs.append(f"'id' must match dirname: expected '{exp}', got '{case_id}'")

    # provider (OPTIONAL — defaults to empty string, meaning "use config's
    # default_provider" when the runner invokes atomcode). If present, must
    # be a non-empty string.
    provider = meta.get("provider")
    if provider is not None:
        if not isinstance(provider, str):
            errs.append("'provider' must be a string if set")
        elif provider == "":
            errs.append("'provider' must not be an empty string (omit the field instead)")

    # timeout_secs
    timeout = meta.get("timeout_secs", 120)
    if not isinstance(timeout, int) or isinstance(timeout, bool) or timeout <= 0:
        errs.append("'timeout_secs' must be a positive integer")

    # tags
    tags = meta.get("tags", [])
    if not isinstance(tags, list) or not all(isinstance(t, str) for t in tags):
        errs.append("'tags' must be a list of strings")

    # seed_files
    seed_files = meta.get("seed_files", {})
    if seed_files and form == "B":
        errs.append("inline 'seed_files' is not allowed in form B (use seed/ directory instead)")
    if not isinstance(seed_files, dict):
        errs.append("'seed_files' must be a table")
    else:
        for k, v in seed_files.items():
            if not isinstance(k, str):
                errs.append(f"seed_files key must be a string, got {type(k).__name__}")
                continue
            if not isinstance(v, str):
                errs.append(f"seed_files['{k}']: value must be a string content")
                continue
            if k.startswith("/"):
                errs.append(f"seed_files['{k}']: absolute path not allowed (must be relative)")
            if ".." in Path(k).parts:
                errs.append(f"seed_files['{k}']: parent traversal '..' not allowed")

    return errs


def main():
    if len(sys.argv) != 2:
        print(json.dumps({"ok": False, "errors": ["usage: parse_case.py <case-file>"], "case_path": None}))
        return 0

    case_path = Path(sys.argv[1]).absolute()
    if not case_path.is_file():
        print(json.dumps({"ok": False, "errors": [f"file not found: {case_path}"], "case_path": str(case_path)}))
        return 0

    text = case_path.read_text(encoding="utf-8")

    try:
        fm_text, body = parse_frontmatter(text)
    except ValueError as e:
        print(json.dumps({
            "ok": False,
            "errors": [f"frontmatter parse error: {e}"],
            "case_path": str(case_path),
        }))
        return 0

    try:
        meta = tomllib.loads(fm_text)
    except tomllib.TOMLDecodeError as e:
        print(json.dumps({
            "ok": False,
            "errors": [f"frontmatter TOML parse error: {e}"],
            "case_path": str(case_path),
        }))
        return 0

    form, seed_dir = detect_form(case_path)
    errs = validate(meta, body, case_path, form, seed_dir)
    if errs:
        print(json.dumps({"ok": False, "errors": errs, "case_path": str(case_path)}))
        return 0

    out = {
        "ok": True,
        "id": meta["id"],
        "form": form,
        "provider": meta.get("provider", ""),
        "description": meta.get("description", ""),
        "timeout_secs": meta.get("timeout_secs", 120),
        "tags": meta.get("tags", []),
        "seed_files": meta.get("seed_files", {}),
        "prompt_body": body,
        "case_path": str(case_path),
        "seed_dir": str(seed_dir) if seed_dir else None,
    }
    print(json.dumps(out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
