#!/usr/bin/env python3
"""Render runs/<ts>/index.html from summary.json + per-case meta.json files.

Pure stdlib — no jinja2, no external templating.

Usage: render_index.py <run-dir>
"""
import html
import json
import sys
from datetime import datetime
from pathlib import Path
from string import Template


HTML_TMPL = Template("""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>AtomCode Eval — $started_at</title>
<style>
  body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", monospace, sans-serif;
         max-width: 1100px; margin: 1.5em auto; padding: 0 1em; color: #222; }
  h1 { font-size: 1.3em; margin-bottom: 0.2em; }
  .meta { color: #666; font-size: 0.9em; line-height: 1.5; margin-bottom: 1.5em; }
  .meta code { background: #f0f0f0; padding: 0.1em 0.3em; border-radius: 2px; }
  table { border-collapse: collapse; width: 100%; font-size: 0.92em; }
  th, td { text-align: left; padding: 0.4em 0.6em; border-bottom: 1px solid #e8e8e8; }
  th { background: #fafafa; font-weight: 600; }
  td.num { text-align: right; font-variant-numeric: tabular-nums; }
  tr.pass     td.status { color: #2a7; font-weight: 600; }
  tr.fail     td.status { color: #c33; font-weight: 600; }
  tr.denied   td.status { color: #d80; font-weight: 600; }
  tr.timeout  td.status { color: #c63; font-weight: 600; }
  tr.error    td.status { color: #c33; font-weight: 600; }
  tr.invalid  td.status { color: #888; font-weight: 600; font-style: italic; }
  tr.aborted  td.status { color: #888; font-weight: 600; }
  tr.cancelled td.status { color: #888; font-weight: 600; }
  tr.predicted td.status { color: #0c5460; font-weight: 600; }
  tr.resolved  td.status { color: #155724; font-weight: 600; }
  tr.unresolved td.status { color: #721c24; font-weight: 600; }
  a { color: #06c; text-decoration: none; }
  a:hover { text-decoration: underline; }
  .totals { margin-top: 1em; color: #444; }
  .totals strong { color: #222; }
  .badge { display:inline-block; padding:2px 6px; border-radius:3px; font-size:11px; margin-right:4px; }
  .badge.resolved   { background:#d4edda; color:#155724; }
  .badge.unresolved { background:#f8d7da; color:#721c24; }
  .badge.predicted  { background:#d1ecf1; color:#0c5460; }
  .badge.error      { background:#fff3cd; color:#856404; }
  .badge.num        { background:#e9ecef; color:#495057; }
</style>
</head>
<body>
<h1>AtomCode Eval — $started_at_display</h1>
<div class="meta">
  <code>$version_string</code><br>
  binary: <code>$binary_path</code><br>
  sha256: <code>$binary_sha256</code><br>
  duration: $duration_display &middot; concurrency: $concurrency &middot; runner: <code>$runner_version</code>
</div>

<table>
<thead>
  <tr><th>id</th><th>provider</th><th>status</th><th class="num">exit</th><th class="num">wall</th><th class="num">denial</th><th>→</th></tr>
</thead>
<tbody>
$rows
</tbody>
</table>

<div class="totals">
Totals: $case_count cases &middot;
<strong>pass $pass</strong> &middot;
fail $fail &middot;
denied $denied &middot;
timeout $timeout &middot;
cancelled $cancelled &middot;
error $error &middot;
invalid $invalid &middot;
aborted $aborted
</div>
</body>
</html>
""")

ROW_TMPL = Template(
    '  <tr class="$status_class">'
    '<td>$id</td>'
    '<td>$provider</td>'
    '<td class="status">$status</td>'
    '<td class="num">$exit_code</td>'
    '<td class="num">$wall_display</td>'
    '<td class="num">$denial_count</td>'
    '<td>$link</td>'
    '</tr>'
)

# Per-case detail page. Linked from index.html's row "view" link. Aggregates
# the most useful data on one page so you don't have to dig through several
# raw files to understand a case at a glance.
CASE_DETAIL_TMPL = Template("""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>$case_id — AtomCode Eval</title>
<style>
  body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", monospace, sans-serif;
         max-width: 960px; margin: 1.5em auto; padding: 0 1em; color: #222; }
  h1 { font-size: 1.4em; margin: 0.2em 0; }
  h2 { font-size: 1.0em; margin-top: 1.6em; padding-bottom: 0.2em;
       border-bottom: 1px solid #e8e8e8; color: #444; }
  .back { font-size: 0.88em; color: #888; margin-bottom: 0.6em; }
  .desc { color: #666; font-size: 0.92em; margin-bottom: 1em; }
  .badge { display: inline-block; padding: 0.1em 0.6em; border-radius: 3px;
           font-size: 0.82em; font-weight: 600; vertical-align: middle; }
  .badge.pass     { background: #d4f4e1; color: #1a6f3a; }
  .badge.fail     { background: #fde0e0; color: #a01515; }
  .badge.denied   { background: #fde8c5; color: #8a4d00; }
  .badge.timeout  { background: #fce0c5; color: #8a3d00; }
  .badge.error    { background: #fde0e0; color: #a01515; }
  .badge.invalid  { background: #e8e8e8; color: #666; font-style: italic; }
  .badge.aborted, .badge.cancelled { background: #e8e8e8; color: #666; }
  .badge.resolved   { background: #d4edda; color: #155724; }
  .badge.unresolved { background: #f8d7da; color: #721c24; }
  .badge.predicted  { background: #d1ecf1; color: #0c5460; }
  .badge.num        { background: #e9ecef; color: #495057; font-size: 11px; padding: 2px 6px; border-radius: 3px; }
  table.kv { font-size: 0.88em; border-collapse: collapse; margin-top: 0.4em; }
  table.kv td { padding: 0.15em 0.8em 0.15em 0; vertical-align: top; }
  table.kv td.k { color: #888; min-width: 5em; }
  table.kv code { background: #f0f0f0; padding: 0.05em 0.3em; border-radius: 2px;
                  font-size: 0.95em; }
  pre { background: #f7f7f7; border: 1px solid #e8e8e8; border-radius: 3px;
        padding: 0.6em 0.8em; overflow-x: auto; font-size: 0.85em;
        line-height: 1.45; white-space: pre-wrap; word-wrap: break-word;
        max-height: 28em; }
  pre.empty { color: #888; font-style: italic; }
  p.empty { color: #888; font-style: italic; font-size: 0.88em; }
  ul.files { font-size: 0.88em; padding-left: 1.4em; margin-top: 0.4em; }
  ul.files li { margin: 0.18em 0; }
  ul.raw { font-size: 0.88em; padding-left: 1.4em; margin-top: 0.4em; }
  ul.raw li { margin: 0.18em 0; color: #666; }
  ul.raw a { color: #06c; text-decoration: none; }
  ul.raw a:hover { text-decoration: underline; }
  .file-size { color: #888; font-size: 0.85em; margin-left: 0.4em; }
  .errors { color: #a01515; font-size: 0.88em; }
  .turns { margin-top: 0.6em; }
  .turn { margin: 0.5em 0 1em 0; padding: 0.5em 0.8em; background: #fafafa;
          border: 1px solid #e8e8e8; border-radius: 3px; }
  .turn-header { font-size: 0.85em; color: #555; margin-bottom: 0.3em;
                 font-weight: 600; }
  .turn-header em { color: #888; font-weight: 400; font-style: italic; }
  .turn-header .pill { display: inline-block; background: #e8eef5;
                       color: #1551a8; padding: 0.05em 0.4em; border-radius: 2px;
                       margin-left: 0.4em; font-weight: 600; font-size: 0.95em; }
  ul.tool-calls { font-size: 0.85em; padding-left: 1.2em; margin: 0.3em 0; }
  ul.tool-calls li { margin: 0.15em 0; line-height: 1.5; }
  ul.tool-calls code { background: #fff; border: 1px solid #d0dae8;
                       color: #1551a8; padding: 0.05em 0.4em; border-radius: 2px;
                       font-weight: 600; }
  ul.tool-calls .args { color: #666; font-size: 0.95em; word-break: break-word;
                        margin-left: 0.3em; }
  .turn pre.reply { margin: 0.4em 0 0 0; max-height: 12em; }
  .turn .empty { color: #888; font-size: 0.85em; font-style: italic; }
  pre.diff { background:#f6f8fa; padding:12px; border-radius:4px; font-size:12px; overflow-x:auto; line-height:1.4; }
  pre.diff .diff-head { color:#6f42c1; font-weight:600; }
  pre.diff .diff-hunk { color:#005cc5; font-weight:600; display:block; margin-top:4px; }
  pre.diff .diff-add  { color:#22863a; background:#e6ffec; display:block; }
  pre.diff .diff-del  { color:#b31d28; background:#ffeef0; display:block; }
</style>
</head>
<body>

<div class="back"><a href="../index.html">&larr; back to index</a></div>

<h1>$case_id <span class="badge $status">$status</span></h1>
$description_block

<h2>Run metadata</h2>
<table class="kv">
<tr><td class="k">form</td><td><code>$form</code></td></tr>
<tr><td class="k">provider</td><td><code>$provider</code></td></tr>
<tr><td class="k">exit code</td><td>$exit_code</td></tr>
<tr><td class="k">wall time</td><td>$wall_display</td></tr>
<tr><td class="k">turns</td><td>$turn_count</td></tr>
<tr><td class="k">denials</td><td>$denial_count</td></tr>
<tr><td class="k">started</td><td><code>$started_at</code></td></tr>
<tr><td class="k">ended</td><td><code>$ended_at</code></td></tr>
</table>

$errors_block

<h2>Prompt</h2>
$prompt_block

<h2>Atomcode reply (stdout)</h2>
$stdout_block

<h2>Per-turn breakdown</h2>
$turns_block

$cwd_or_patch_block

<h2>Tool timeline (last 25 lines of stderr)</h2>
$stderr_tail_block

<h2>Raw artifacts</h2>
$raw_artifacts_block

</body>
</html>
""")


def esc(s) -> str:
    if s is None:
        return "—"
    return html.escape(str(s))


def fmt_wall(ms) -> str:
    if ms is None:
        return "—"
    secs = ms / 1000.0
    return f"{secs:.1f}s"


def fmt_duration(start: str, end: str) -> str:
    """ISO timestamps to '8m 34s' or '2h 5m 34s' style. Best-effort."""
    try:
        s = datetime.fromisoformat(start.rstrip("Z"))
        e = datetime.fromisoformat(end.rstrip("Z"))
        delta = (e - s).total_seconds()
        h, rem = divmod(int(delta), 3600)
        m, sec = divmod(rem, 60)
        return f"{h}h {m}m {sec}s" if h else f"{m}m {sec}s"
    except Exception:
        return "?"


def render_swebench_badges(meta: dict) -> str:
    """Render compact SWE-bench status + efficiency badge row for index.html / case.html."""
    eff = meta.get("efficiency", {})
    resolved = meta.get("swebench_resolved")
    if resolved is True:
        primary = '<span class="badge resolved">&#x2713; Resolved</span>'
    elif resolved is False:
        mode = meta.get("swebench_failure_mode") or "unresolved"
        primary = f'<span class="badge unresolved">&#x2717; {esc(mode)}</span>'
    elif meta.get("status") == "predicted":
        primary = '<span class="badge predicted">&#x22EF; Predicted</span>'
    else:
        primary = f'<span class="badge error">{esc(meta.get("status", ""))}</span>'

    turns = eff.get("turns", 0)
    ptok = eff.get("prompt_tokens", 0)
    cost = eff.get("estimated_cost_usd", 0.0)
    wall = meta.get("wall_ms", 0) / 1000.0

    badges = [
        primary,
        f'<span class="badge num">{turns} turns</span>',
        f'<span class="badge num">{ptok // 1000}k tok</span>',
        f'<span class="badge num">${cost:.2f}</span>',
        f'<span class="badge num">{wall:.0f}s</span>',
    ]
    return " ".join(badges)


def render_row(meta: dict) -> str:
    case_id = meta.get("id") or "?"
    status = meta.get("status", "?")
    link = '<a href="./{}/case.html">view</a>'.format(esc(case_id))

    if meta.get("form") == "swebench":
        # For swebench rows, replace exit/wall/denial columns with a badge span.
        badges = render_swebench_badges(meta)
        return (
            f'  <tr class="{esc(status)}">'
            f'<td>{esc(case_id)}</td>'
            f'<td>{esc(meta.get("provider"))}</td>'
            f'<td class="status">{esc(status)}</td>'
            f'<td colspan="3">{badges}</td>'
            f'<td>{link}</td>'
            f'</tr>'
        )

    return ROW_TMPL.substitute(
        status_class=esc(status),
        id=esc(case_id),
        provider=esc(meta.get("provider")),
        status=esc(status),
        exit_code=esc(meta.get("exit_code")),
        wall_display=esc(fmt_wall(meta.get("wall_ms"))),
        denial_count=esc(meta.get("denial_count", 0)),
        # Link to the per-case detail page generated by render_case_detail.
        # Both pass and invalid cases get a detail page — invalid cases show
        # the parser errors instead of stdout/cwd info.
        link=link,
    )


def _safe_read(path: Path, max_bytes: int = 64 * 1024) -> str:
    """Read a file as text, capping at max_bytes. Returns empty string on
    any error (file missing, decode error, etc.). Used for rendering raw
    artifact contents into the case detail HTML."""
    try:
        with open(path, "rb") as f:
            data = f.read(max_bytes + 1)
        truncated = len(data) > max_bytes
        text = data[:max_bytes].decode("utf-8", errors="replace")
        if truncated:
            text += "\n\n[... truncated, see raw file for full content ...]"
        return text
    except Exception:
        return ""


def _list_cwd_files(cwd_dir: Path, max_files: int = 50) -> str:
    """Render an HTML <ul> listing files under cwd/, sorted by relative path.
    Skips well-known build/cache directories that aren't useful in a glance
    view (target/, node_modules/, __pycache__/, .git/) — they're still
    visible via the raw 'cwd/' link in the artifacts section. Returns an
    italic placeholder if cwd/ is empty or missing."""
    if not cwd_dir.is_dir():
        return '<p class="empty"><em>cwd/ does not exist</em></p>'

    SKIP_DIRS = {"target", "node_modules", "__pycache__", ".git", ".next", "dist", "build"}
    files = []
    for p in cwd_dir.rglob("*"):
        # Skip if any path part is a build/cache dir.
        if any(part in SKIP_DIRS for part in p.relative_to(cwd_dir).parts):
            continue
        if p.is_file():
            try:
                size = p.stat().st_size
            except OSError:
                size = 0
            files.append((p.relative_to(cwd_dir), size))
        if len(files) > max_files:
            break

    if not files:
        return '<p class="empty"><em>(no files — atomcode produced nothing in cwd/)</em></p>'

    files.sort(key=lambda x: str(x[0]))
    items = []
    for rel, size in files:
        rel_str = str(rel)
        size_str = f"{size:,} B" if size < 1024 else f"{size / 1024:.1f} KB"
        items.append(
            f'<li><a href="./cwd/{esc(rel_str)}">{esc(rel_str)}</a>'
            f'<span class="file-size">{esc(size_str)}</span></li>'
        )
    truncated_note = ""
    if len(files) > max_files:
        truncated_note = f'<li><em>(... more than {max_files} files; see raw cwd/ link)</em></li>'
    return '<ul class="files">' + "".join(items) + truncated_note + "</ul>"


def _render_pre(content: str, empty_msg: str) -> str:
    """Render a <pre> block with content, or an italic placeholder if empty."""
    if not content or not content.strip():
        return f'<pre class="empty">{esc(empty_msg)}</pre>'
    return f"<pre>{esc(content)}</pre>"


def _stderr_tail(stderr_text: str, n: int = 25) -> str:
    """Return the last n non-empty lines of stderr — useful for capturing
    the tail of the tool-call timeline at a glance."""
    if not stderr_text:
        return ""
    lines = [ln for ln in stderr_text.splitlines() if ln.strip()]
    return "\n".join(lines[-n:])


def _load_turns(log_dir: Path) -> list:
    """Pair request/response files in home/logs/ into turn dicts.
    Each entry: {n, prompt_tokens, duration_ms, text, tool_calls, had_response}.
    Files are sorted lexicographically — the timestamp prefix in the filename
    means this is also chronological order, with each request followed by its
    response. Orphan requests (turn started, response never written — e.g.
    timeout/crash mid-call) are kept as final entries with had_response=False."""
    if not log_dir.is_dir():
        return []
    files = sorted(log_dir.glob("*.json"))
    pairs = []
    pending_req = None
    for f in files:
        if f.name.endswith("_response.json"):
            if pending_req is not None:
                pairs.append((pending_req, f))
                pending_req = None
            # else: orphan response with no preceding request — skip
        else:
            if pending_req is not None:
                # previous request never got a response — flush as half-pair
                pairs.append((pending_req, None))
            pending_req = f
    if pending_req is not None:
        pairs.append((pending_req, None))

    out = []
    for n, (req_f, resp_f) in enumerate(pairs, start=1):
        try:
            with open(req_f, encoding="utf-8") as fh:
                req = json.load(fh)
        except Exception:
            req = {}
        resp = {}
        if resp_f is not None:
            try:
                with open(resp_f, encoding="utf-8") as fh:
                    resp = json.load(fh)
            except Exception:
                pass
        out.append({
            "n": n,
            "prompt_tokens": req.get("estimated_tokens", 0),
            "duration_ms": resp.get("duration_ms"),
            "text": resp.get("text", "") or "",
            "tool_calls": resp.get("tool_calls", []) or [],
            "had_response": resp_f is not None,
        })
    return out


def _format_tool_args(args_str: str, max_len: int = 180) -> str:
    """Compact representation of a tool call's arguments for the per-turn view.
    Tries to parse as JSON and emit 'key=value key=value …' (truncating any
    huge values like file content blocks). Falls back to a truncated raw
    string on parse failure. Output is plain text — caller must HTML-escape."""
    if not args_str:
        return ""
    try:
        d = json.loads(args_str)
        if isinstance(d, dict):
            parts = []
            for k, v in d.items():
                if isinstance(v, (dict, list)):
                    v_str = json.dumps(v, ensure_ascii=False)
                else:
                    v_str = str(v)
                # Aggressively cap individual values (file content can be huge)
                if len(v_str) > 80:
                    v_str = v_str[:80] + "…"
                # Replace newlines so the args stay on one line in the bullet
                v_str = v_str.replace("\n", "\\n")
                parts.append(f"{k}={v_str}")
            joined = " ".join(parts)
            if len(joined) > max_len:
                joined = joined[:max_len] + "…"
            return joined
    except (json.JSONDecodeError, ValueError):
        pass
    s = str(args_str).replace("\n", "\\n")
    return s[:max_len] + ("…" if len(s) > max_len else "")


def _render_turns_section(turns: list) -> str:
    """Render the full per-turn breakdown HTML section. Each turn is a
    bordered box with: header (turn number, prompt tokens, duration), tool
    calls list (if any), and the model's text reply (if non-empty — usually
    only on the final turn)."""
    if not turns:
        return '<p class="empty"><em>(no LLM I/O logs — case did not run atomcode)</em></p>'

    blocks = []
    for t in turns:
        header_parts = [f"Turn {t['n']}"]
        if t["prompt_tokens"]:
            header_parts.append(f"prompt {t['prompt_tokens']:,} tokens")
        if t["duration_ms"] is not None:
            header_parts.append(f"{t['duration_ms'] / 1000:.1f}s")
        if not t["had_response"]:
            header_parts.append("<em>no response (turn aborted?)</em>")
        header_html = ' &middot; '.join(header_parts)

        body_parts = []

        if t["tool_calls"]:
            items = []
            for tc in t["tool_calls"]:
                name = esc(tc.get("name", "?"))
                args_compact = _format_tool_args(tc.get("arguments", ""))
                items.append(
                    f'<li><code>{name}</code>'
                    f'<span class="args">{esc(args_compact)}</span></li>'
                )
            body_parts.append('<ul class="tool-calls">' + "".join(items) + "</ul>")

        if t["text"].strip():
            body_parts.append(f'<pre class="reply">{esc(t["text"])}</pre>')

        if not body_parts:
            body_parts.append('<div class="empty">(no output for this turn)</div>')

        blocks.append(
            f'<div class="turn">'
            f'<div class="turn-header">{header_html}</div>'
            f'{"".join(body_parts)}'
            f'</div>'
        )

    return f'<div class="turns">{"".join(blocks)}</div>'


def render_patch_diff_html(patch_path: Path) -> str:
    """Render a unified git diff as syntax-highlighted HTML (pure manual coloring)."""
    if not patch_path.exists():
        return '<p class="empty">(no patch.diff)</p>'
    try:
        patch = patch_path.read_text(encoding="utf-8", errors="replace")
    except Exception:
        return '<p class="empty">(patch.diff unreadable)</p>'
    if not patch.strip():
        return '<p class="empty">(empty patch)</p>'

    lines = []
    for ln in patch.splitlines():
        escaped = esc(ln)
        if ln.startswith("+++") or ln.startswith("---"):
            lines.append(f'<span class="diff-head">{escaped}</span>')
        elif ln.startswith("@@"):
            lines.append(f'<span class="diff-hunk">{escaped}</span>')
        elif ln.startswith("+"):
            lines.append(f'<span class="diff-add">{escaped}</span>')
        elif ln.startswith("-"):
            lines.append(f'<span class="diff-del">{escaped}</span>')
        else:
            lines.append(escaped)
    return '<pre class="diff">' + "\n".join(lines) + "</pre>"


def render_case_detail(case_dir: Path, meta: dict) -> None:
    """Generate <case_dir>/case.html — a one-page summary of this case's
    run, linked from index.html's row 'view' link. Reads prompt.md, stdout.txt,
    stderr.txt (tail), lists cwd/ files (skipping build caches), and counts
    home/logs/ entries. Always succeeds; failed reads degrade gracefully to
    italic placeholders."""
    case_id = meta.get("id") or case_dir.name
    status = meta.get("status", "?")

    # Description from prompt.md frontmatter, if available. We don't fully
    # parse the TOML here (parse_case.py already did that during dispatch);
    # render_index.py runs offline against artifacts and shouldn't depend
    # on parse_case.py logic. So we just look for a 'description = "..."'
    # line in the frontmatter region. Best-effort.
    description = ""
    prompt_path = case_dir / "prompt.md"
    prompt_text = _safe_read(prompt_path)
    if prompt_text:
        in_frontmatter = False
        for line in prompt_text.splitlines():
            if line.strip() == "+++":
                if in_frontmatter:
                    break
                in_frontmatter = True
                continue
            if in_frontmatter and line.startswith("description"):
                # Parse 'description = "..."' loosely.
                _, _, val = line.partition("=")
                val = val.strip().strip('"').strip("'")
                description = val
                break

    # Load per-turn data from home/logs/. _load_turns handles both complete
    # request/response pairs and orphaned (aborted-mid-call) requests.
    log_dir = case_dir / "home" / "logs"
    turns = _load_turns(log_dir)
    log_count = sum(1 for _ in log_dir.glob("*.json")) if log_dir.is_dir() else 0
    turn_count = len(turns)

    # Description block
    description_block = (
        f'<div class="desc">{esc(description)}</div>' if description else ""
    )

    # Errors block (only for invalid cases)
    errors = meta.get("errors") or []
    if errors:
        items = "".join(f"<li>{esc(e)}</li>" for e in errors)
        errors_block = f'<h2>Validation errors</h2><ul class="errors">{items}</ul>'
    else:
        errors_block = ""

    # Prompt body — read full prompt.md.
    prompt_block = _render_pre(prompt_text, "(no prompt — see raw file)")

    # Stdout
    stdout_text = _safe_read(case_dir / "stdout.txt")
    stdout_block = _render_pre(stdout_text, "(empty stdout — atomcode produced no final reply)")

    # Stderr tail
    stderr_text = _safe_read(case_dir / "stderr.txt")
    stderr_block = _render_pre(_stderr_tail(stderr_text), "(empty stderr)")

    # Per-turn breakdown
    turns_block = _render_turns_section(turns)

    # Branch: swebench cases show patch.diff + swebench metadata instead of cwd tree.
    form = meta.get("form")
    if form == "swebench":
        sw = meta.get("swebench", {})
        # Build swebench-specific metadata block (repo, base_commit, etc.)
        sw_rows = "".join(
            f'<tr><td class="k">{esc(k)}</td><td><code>{esc(str(v))}</code></td></tr>'
            for k, v in [
                ("repo",              sw.get("repo")),
                ("base_commit",       sw.get("base_commit")),
                ("prompt_template",   sw.get("prompt_template")),
                ("include_hints",     sw.get("include_hints")),
                ("dataset_revision",  sw.get("dataset_revision")),
                ("patch_size_bytes",  sw.get("patch_size_bytes")),
            ]
        )
        swebench_meta_block = (
            '<h2>SWE-bench metadata</h2>'
            '<table class="kv">' + sw_rows + '</table>'
        )
        # Efficiency badges at top (rendered after h1 via description_block slot)
        eff_badges = render_swebench_badges(meta)
        if description_block:
            description_block = f'<div class="swbadges">{eff_badges}</div>' + description_block
        else:
            description_block = f'<div class="swbadges">{eff_badges}</div>'

        # patch.diff rendering in place of cwd tree
        patch_html = render_patch_diff_html(case_dir / "patch.diff")
        cwd_or_patch_block = (
            swebench_meta_block
            + '<h2>patch.diff</h2>'
            + patch_html
        )
        raw_artifacts_block = (
            '<ul class="raw">'
            '<li><a href="./patch.diff">patch.diff</a> &mdash; generated patch</li>'
            '<li><a href="./prompt.rendered.txt">prompt.rendered.txt</a> &mdash; rendered prompt sent to LLM</li>'
            '<li><a href="./stdout.txt">stdout.txt</a> &mdash; full atomcode reply</li>'
            '<li><a href="./stderr.txt">stderr.txt</a> &mdash; full -v diagnostic stream</li>'
            '<li><a href="./meta.json">meta.json</a> &mdash; runner metadata</li>'
            '</ul>'
        )
    else:
        cwd_or_patch_block = (
            '<h2>Files in <code>cwd/</code></h2>'
            + _list_cwd_files(case_dir / "cwd")
        )
        raw_artifacts_block = (
            '<ul class="raw">'
            f'<li><a href="./prompt.md">prompt.md</a> &mdash; original case file</li>'
            f'<li><a href="./stdout.txt">stdout.txt</a> &mdash; full atomcode reply</li>'
            f'<li><a href="./stderr.txt">stderr.txt</a> &mdash; full -v diagnostic stream</li>'
            f'<li><a href="./meta.json">meta.json</a> &mdash; runner metadata</li>'
            f'<li><a href="./cwd/">cwd/</a> &mdash; final working directory tree</li>'
            f'<li><a href="./home/logs/">home/logs/</a> &mdash; full LLM I/O ({esc(log_count)} files)</li>'
            '</ul>'
        )

    html_out = CASE_DETAIL_TMPL.substitute(
        case_id=esc(case_id),
        status=esc(status),
        description_block=description_block,
        form=esc(form),
        provider=esc(meta.get("provider") or "(default)"),
        exit_code=esc(meta.get("exit_code")),
        wall_display=esc(fmt_wall(meta.get("wall_ms"))),
        turn_count=esc(turn_count),
        denial_count=esc(meta.get("denial_count", 0)),
        started_at=esc(meta.get("started_at")),
        ended_at=esc(meta.get("ended_at")),
        errors_block=errors_block,
        prompt_block=prompt_block,
        stdout_block=stdout_block,
        turns_block=turns_block,
        cwd_or_patch_block=cwd_or_patch_block,
        stderr_tail_block=stderr_block,
        raw_artifacts_block=raw_artifacts_block,
    )

    (case_dir / "case.html").write_text(html_out, encoding="utf-8")


def main():
    if len(sys.argv) != 2:
        print("usage: render_index.py <run-dir>", file=sys.stderr)
        return 2

    run_dir = Path(sys.argv[1])
    summary = json.loads((run_dir / "summary.json").read_text(encoding="utf-8"))

    # Collect per-case meta.json files in id-sorted order.
    case_metas = []
    for case_dir in sorted(run_dir.iterdir()):
        if not case_dir.is_dir():
            continue
        meta_path = case_dir / "meta.json"
        if not meta_path.is_file():
            continue
        try:
            case_metas.append(json.loads(meta_path.read_text(encoding="utf-8")))
        except json.JSONDecodeError:
            continue

    rows = "\n".join(render_row(m) for m in case_metas) if case_metas else \
        '  <tr><td colspan="7" style="text-align:center;color:#888">no cases</td></tr>'

    totals = summary.get("totals", {})
    started = summary.get("started_at", "?")
    ended = summary.get("ended_at", "?")
    atom = summary.get("atomcode", {})

    html_out = HTML_TMPL.substitute(
        started_at=esc(started),
        started_at_display=esc(started),
        version_string=esc(atom.get("version_string")),
        binary_path=esc(atom.get("binary_path")),
        binary_sha256=esc(atom.get("binary_sha256")),
        duration_display=esc(fmt_duration(started, ended)),
        concurrency=esc(summary.get("concurrency")),
        runner_version=esc(summary.get("runner_version")),
        rows=rows,
        case_count=esc(summary.get("case_count")),
        **{k: esc(totals.get(k, 0)) for k in ("pass", "fail", "denied", "timeout", "cancelled", "error", "invalid", "aborted")},
    )

    (run_dir / "index.html").write_text(html_out, encoding="utf-8")

    # Generate per-case detail pages (linked from each row's "view" link).
    # Iterate the same case_metas we already collected; for each, find the
    # corresponding case_dir and render its case.html.
    for meta in case_metas:
        case_id = meta.get("id")
        if not case_id:
            continue
        case_dir = run_dir / case_id
        if not case_dir.is_dir():
            continue
        try:
            render_case_detail(case_dir, meta)
        except Exception as e:
            # Detail rendering is best-effort — log to stderr but don't
            # fail the whole run. The index.html link will 404 for that
            # case but everything else still works.
            print(f"warning: failed to render case detail for {case_id}: {e}", file=sys.stderr)

    return 0


if __name__ == "__main__":
    sys.exit(main())
