#!/usr/bin/env python3
"""Count lines / words / chars in one or more files, like a minimal `wc`."""
import argparse
import sys


def count_file(path):
    with open(path, encoding="utf-8") as f:
        text = f.read()
    lines = text.count("\n")
    words = len(text.split())
    chars = len(text)
    return lines, words, chars


def main():
    p = argparse.ArgumentParser(description="count lines/words/chars")
    p.add_argument("paths", nargs="+", help="files to count")
    p.add_argument("--no-total", action="store_true",
                   help="suppress the 'total' row when counting multiple files")
    args = p.parse_args()

    total_l, total_w, total_c = 0, 0, 0
    for path in args.paths:
        try:
            l, w, c = count_file(path)
        except OSError as e:
            print(f"error: {path}: {e}", file=sys.stderr)
            sys.exit(1)
        print(f"{l:8d} {w:8d} {c:8d} {path}")
        total_l += l
        total_w += w
        total_c += c

    if len(args.paths) > 1 and not args.no_total:
        print(f"{total_l:8d} {total_w:8d} {total_c:8d} total")


if __name__ == "__main__":
    main()
