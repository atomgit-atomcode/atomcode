#!/usr/bin/env python3
"""Fetch SWE-bench Verified dataset from HuggingFace and cache locally.

First run:
    1. Loads the dataset at main branch (latest commit)
    2. Prints suggested revision SHA
    3. Interactively asks user to accept (y/N)
    4. On accept: writes revision to manifest.toml + caches dataset.json
    5. On reject: aborts, user must run again or edit manifest manually

Subsequent runs:
    - Reads revision from manifest.toml
    - Loads dataset at that exact revision
    - Refreshes cache/dataset.json if it's stale

Flags:
    --accept-suggested-revision   Skip interactive prompt, auto-write suggested revision
    --force                        Re-fetch and overwrite cache even if up-to-date
"""
import argparse
import json
import sys
from pathlib import Path

# tomllib (read) is stdlib in 3.11+. tomli_w (write) is not stdlib, so we
# write manifest.toml manually with a minimal string replacement.
try:
    import tomllib
except ImportError:
    print("error: requires Python 3.11+ (tomllib)", file=sys.stderr)
    sys.exit(2)


def _import_load_dataset():
    """Deferred import so `--help` works without the `datasets` package installed."""
    try:
        from datasets import load_dataset
        return load_dataset
    except ImportError:
        print("error: `datasets` package not installed. Run: pip install datasets", file=sys.stderr)
        sys.exit(2)

HERE = Path(__file__).resolve().parent
MANIFEST_PATH = HERE / "manifest.toml"
CACHE_DIR = HERE / "cache"
DATASET_CACHE = CACHE_DIR / "dataset.json"


def read_manifest() -> dict:
    """Read manifest.toml into a dict."""
    with open(MANIFEST_PATH, "rb") as f:
        return tomllib.load(f)


def write_revision_to_manifest(revision: str) -> None:
    """Minimal string-replace rewrite of the revision line. Preserves comments and layout."""
    text = MANIFEST_PATH.read_text(encoding="utf-8")
    lines = text.splitlines(keepends=True)
    out = []
    in_dataset = False
    replaced = False
    for line in lines:
        stripped = line.strip()
        if stripped == "[dataset]":
            in_dataset = True
        elif stripped.startswith("[") and in_dataset:
            in_dataset = False
        if in_dataset and stripped.startswith("revision"):
            out.append(f'revision = "{revision}"\n')
            replaced = True
        else:
            out.append(line)
    if not replaced:
        raise RuntimeError("could not find `revision` key in [dataset] section of manifest.toml")
    MANIFEST_PATH.write_text("".join(out), encoding="utf-8")


def dataset_to_list(ds) -> list:
    """Convert HuggingFace Dataset to a plain list of dicts (JSON-serializable)."""
    return [dict(row) for row in ds]


def main() -> int:
    parser = argparse.ArgumentParser(description="Fetch SWE-bench Verified dataset.")
    parser.add_argument("--accept-suggested-revision", action="store_true",
                        help="Skip interactive confirmation, auto-write suggested revision.")
    parser.add_argument("--force", action="store_true",
                        help="Re-fetch and overwrite cache even if it exists.")
    args = parser.parse_args()

    manifest = read_manifest()
    dataset_cfg = manifest.get("dataset", {})
    name = dataset_cfg.get("name", "princeton-nlp/SWE-bench_Verified")
    split = dataset_cfg.get("split", "test")
    revision = dataset_cfg.get("revision", "")

    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    load_dataset = _import_load_dataset()

    if not revision:
        print(f"No revision pinned in manifest.toml. Fetching {name} @ main to determine current SHA...")
        try:
            ds = load_dataset(name, split=split)
        except Exception as e:
            msg = str(e).lower()
            if "401" in msg or "403" in msg or "unauthorized" in msg or "gated" in msg:
                print(f"\nerror: {name} appears to require authentication.", file=sys.stderr)
                print("Fix: run `huggingface-cli login` or set `HF_TOKEN` env var, then retry.", file=sys.stderr)
                return 3
            print(f"error: failed to load dataset: {e}", file=sys.stderr)
            return 4

        # Get the dataset's git SHA from _info.
        suggested = getattr(ds.info, "version", None) or getattr(ds, "_fingerprint", "unknown")
        print(f"\nSuggested manifest revision: {suggested}")
        print(f"Dataset has {len(ds)} instances at this revision.")

        if args.accept_suggested_revision:
            confirmed = True
        else:
            print("\nThis value will be written to manifest.toml and LOCKED. All future")
            print("runs will load the dataset at exactly this revision for reproducibility.")
            response = input("Write to manifest.toml? (y/N): ").strip().lower()
            confirmed = response in ("y", "yes")

        if not confirmed:
            print("Aborted. Re-run with --accept-suggested-revision or edit manifest.toml manually.")
            return 1

        write_revision_to_manifest(suggested)
        revision = suggested
        print(f"Pinned revision = {revision} in manifest.toml")

    # Now load at the pinned revision (or the just-confirmed one).
    if DATASET_CACHE.exists() and not args.force:
        with open(DATASET_CACHE, "r", encoding="utf-8") as f:
            cache = json.load(f)
        if cache.get("revision") == revision:
            print(f"Cache up to date: {len(cache['instances'])} instances @ {revision}")
            return 0

    print(f"Loading {name} split={split} revision={revision} ...")
    try:
        ds = load_dataset(name, split=split, revision=revision)
    except Exception as e:
        print(f"error: failed to load at revision {revision}: {e}", file=sys.stderr)
        return 5

    instances = dataset_to_list(ds)
    DATASET_CACHE.write_text(
        json.dumps({"revision": revision, "instances": instances}, indent=2),
        encoding="utf-8",
    )
    print(f"Wrote {len(instances)} instances to {DATASET_CACHE}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
