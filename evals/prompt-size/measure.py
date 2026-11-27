#!/usr/bin/env python3
"""Measure what the first request costs before the user's words do anything.

Reads `ATOMCODE_WIRE_DUMP=1` request dumps (`~/.atomcode/wire-dump/*.req.json`)
and breaks one down into system-prompt sections, tool schemas and injected
messages, or compares two dumps section by section.

    python3 measure.py <req.json>
    python3 measure.py <before.req.json> <after.req.json>

Token counts are estimates (CJK ~1.5 chars/token, other text ~3.8 chars/token),
good for comparing two builds, not for billing.
"""
import json
import re
import sys

CJK = re.compile(r"[　-鿿＀-￯]")
SECTION = re.compile(r"^(## [^\n]+|=== [^\n]+ ===|<mcp-server-instructions>)$")


def tokens(text):
    cjk = len(CJK.findall(text))
    return int(cjk / 1.5 + (len(text) - cjk) / 3.8)


def text_of(content):
    return content if isinstance(content, str) else json.dumps(content, ensure_ascii=False)


def sections(system):
    """Split the system prompt at its section headers; the head is `(preamble)`."""
    out, name, lines = {}, "(preamble)", []
    for line in system.split("\n"):
        if SECTION.match(line):
            out[name] = out.get(name, "") + "\n".join(lines)
            name, lines = line.strip("#=< >"), []
        lines.append(line)
    out[name] = out.get(name, "") + "\n".join(lines)
    return out


def breakdown(path):
    with open(path, encoding="utf-8") as f:
        body = json.load(f)
    messages = body.get("messages", [])
    system = "".join(text_of(m["content"]) for m in messages if m.get("role") == "system")
    rows = {}
    for name, text in sections(system).items():
        rows["system/" + name] = tokens(text)
    for tool in body.get("tools", []):
        fn = tool.get("function", tool)
        rows["tool/" + fn.get("name", "?")] = tokens(json.dumps(tool, ensure_ascii=False))
    for m in messages:
        if m.get("role") != "system":
            text = text_of(m["content"])
            # Keyed by how it starts, so an inserted message does not shift the rest.
            head = " ".join(text.split())[:60]
            key = "message/%s %s" % (m.get("role"), head)
            rows[key] = rows.get(key, 0) + tokens(text)
    return rows


def totals(rows):
    out = {}
    for key, value in rows.items():
        group = key.split("/", 1)[0]
        out[group] = out.get(group, 0) + value
    out["total"] = sum(rows.values())
    return out


def main(argv):
    if len(argv) == 2:
        rows = breakdown(argv[1])
        for key, value in sorted(rows.items(), key=lambda kv: -kv[1]):
            print("%7d  %s" % (value, key))
        print()
        for key, value in totals(rows).items():
            print("%7d  %s" % (value, key))
    elif len(argv) == 3:
        before, after = breakdown(argv[1]), breakdown(argv[2])
        changed = [
            (after.get(k, 0) - before.get(k, 0), k)
            for k in set(before) | set(after)
            if after.get(k, 0) != before.get(k, 0)
        ]
        for delta, key in sorted(changed):
            print("%+7d  %s" % (delta, key))
        print()
        tb, ta = totals(before), totals(after)
        for key in tb.keys() | ta.keys():
            print("%7d -> %7d  %s" % (tb.get(key, 0), ta.get(key, 0), key))
    else:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
