#!/usr/bin/env python3
"""
AtomCode Datalog Analyzer — Post-hoc evaluation of agent performance.

Scans datalog/*.md files, detects antipatterns, and generates an evaluation
report with issues ranked by severity.

Usage:
    python3 scripts/analyze_datalogs.py /path/to/datalog/
    python3 scripts/analyze_datalogs.py /path/to/datalog/2026-03-25_10-42-58.md
    python3 scripts/analyze_datalogs.py --deep /path/to/datalog/   # Claude Code deep analysis on FAIL turns
"""

import re
import sys
import os
import subprocess
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class Issue:
    severity: str  # HIGH, MEDIUM, LOW, BUG
    rule: str
    message: str
    steps: list = field(default_factory=list)


@dataclass
class TurnAnalysis:
    file: str
    user_prompt: str
    total_steps: int
    duration_secs: float
    issues: list = field(default_factory=list)
    files_read: dict = field(default_factory=dict)   # filename -> count
    files_edited: dict = field(default_factory=dict)  # filename -> count
    tools_used: dict = field(default_factory=dict)    # tool -> count
    bash_commands: list = field(default_factory=list)


def parse_datalog(path: str) -> dict:
    """Parse a datalog .md file into structured data."""
    text = open(path).read()

    # Extract user prompt
    user_match = re.search(r'## User\n```\n(.*?)\n```', text, re.DOTALL)
    user_prompt = user_match.group(1).strip() if user_match else ""

    # Extract stats
    stats_match = re.search(r'\*\*Stats:\*\* (\d+) steps?, ([\d.]+)s', text)
    total_steps = int(stats_match.group(1)) if stats_match else 0
    duration = float(stats_match.group(2)) if stats_match else 0

    # Extract steps
    steps = []
    step_pattern = re.compile(
        r'\*\*Step (\d+)\*\* > (\w[\w ]*?)(?:\s+`(.*?)`)?(?:\s+\((\d+) bytes\))?\n'
        r'(.*?)(?=\n\*\*Step |\n\*\*Response|\n---|\Z)',
        re.DOTALL
    )
    for m in step_pattern.finditer(text):
        step_num = int(m.group(1))
        tool = m.group(2).strip()
        args_preview = m.group(3) or ""
        output_preview = m.group(5).strip()
        steps.append({
            "num": step_num,
            "tool": tool,
            "args": args_preview,
            "output": output_preview,
        })

    # Extract response
    resp_match = re.search(r'\*\*Response:\*\*\n(.*?)(?=\n---|\Z)', text, re.DOTALL)
    response = resp_match.group(1).strip() if resp_match else ""

    return {
        "file": os.path.basename(path),
        "user_prompt": user_prompt,
        "total_steps": total_steps,
        "duration": duration,
        "steps": steps,
        "response": response,
    }


def analyze(data: dict) -> TurnAnalysis:
    """Run all antipattern rules against a parsed datalog."""
    t = TurnAnalysis(
        file=data["file"],
        user_prompt=data["user_prompt"],
        total_steps=data["total_steps"],
        duration_secs=data["duration"],
    )

    steps = data["steps"]
    sleep_count = 0
    bash_cmds = {}  # normalized cmd -> [step_nums]

    for s in steps:
        tool = s["tool"]
        args = s["args"]
        output = s["output"]
        num = s["num"]

        # Track tool usage
        t.tools_used[tool] = t.tools_used.get(tool, 0) + 1

        # Track file reads
        if tool == "Read File":
            # Extract filename from args (after .../
            fname = args.rsplit("/", 1)[-1] if "/" in args else args
            # Remove line range suffix like " L59-139"
            fname = re.sub(r'\s+L\d+-\d+$', '', fname)
            t.files_read[fname] = t.files_read.get(fname, 0) + 1

        # Track file edits
        if tool == "Edit File":
            fname = args.rsplit("/", 1)[-1] if "/" in args else args
            t.files_edited[fname] = t.files_edited.get(fname, 0) + 1

        # Track file writes (overwrites)
        if tool == "Write File":
            fname = args.rsplit("/", 1)[-1] if "/" in args else args
            if "Overwrote" in output or "Overwrote" in args:
                t.issues.append(Issue(
                    "HIGH", "write-existing-file",
                    f"Step {num}: write_file overwrote existing file {fname}",
                    [num],
                ))

        # Track bash commands
        if tool == "Bash":
            cmd = args
            t.bash_commands.append((num, cmd))

            # Sleep detection
            if cmd.startswith("sleep ") or "&& sleep " in cmd or "; sleep " in cmd:
                sleep_count += 1

            # Normalize for repeat detection
            norm = re.sub(r'2>&1|2>/dev/null|> /\S+', '', cmd).strip()
            norm = re.sub(r'\s+', ' ', norm)
            if norm not in bash_cmds:
                bash_cmds[norm] = []
            bash_cmds[norm].append(num)

            # Bash reading files
            first_word = cmd.split()[0] if cmd else ""
            if first_word in ("cat", "head", "tail", "sed", "awk", "wc"):
                t.issues.append(Issue(
                    "LOW", "bash-read-file",
                    f"Step {num}: used bash '{first_word}' instead of read_file/grep tool",
                    [num],
                ))

            # Check for foreground server start
            if ("run dev" in cmd or "spring-boot:run" in cmd or "flask run" in cmd) \
                    and "nohup" not in cmd and "&" not in cmd:
                t.issues.append(Issue(
                    "MEDIUM", "foreground-server",
                    f"Step {num}: started server in foreground (should use nohup/&)",
                    [num],
                ))

        # Edit failures
        if tool == "Edit File" and ("old_string not found" in output or output.startswith("x ")):
            t.issues.append(Issue(
                "MEDIUM", "edit-failed",
                f"Step {num}: edit_file failed (old_string not found)",
                [num],
            ))

        # Rename failures
        if "Failed to rename" in output:
            t.issues.append(Issue(
                "BUG", "rename-failed",
                f"Step {num}: atomic rename failed (dev server file lock?)",
                [num],
            ))

        # Loop/block detection
        if "BLOCKED" in output:
            t.issues.append(Issue(
                "WARN", "blocked",
                f"Step {num}: tool call blocked by loop detection",
                [num],
            ))
        if "Loop in cleanup" in output or "force-terminated" in output.lower():
            t.issues.append(Issue(
                "HIGH", "force-terminated",
                f"Step {num}: turn force-terminated due to loop",
                [num],
            ))

    # --- Aggregate rules ---

    # Re-read same file 3+ times
    for fname, count in t.files_read.items():
        if count >= 3:
            t.issues.append(Issue(
                "HIGH", "re-read",
                f"'{fname}' read {count} times (expected: 1-2)",
            ))
        elif count >= 2:
            t.issues.append(Issue(
                "LOW", "re-read",
                f"'{fname}' read {count} times",
            ))

    # Same file edited 3+ times
    for fname, count in t.files_edited.items():
        if count >= 3:
            t.issues.append(Issue(
                "HIGH", "multi-edit",
                f"'{fname}' edited {count} times (should be 1-2 comprehensive edits)",
            ))

    # Sleep polling
    if sleep_count >= 2:
        t.issues.append(Issue(
            "MEDIUM", "sleep-loop",
            f"{sleep_count} sleep commands this turn (polling antipattern)",
        ))

    # Repeated bash commands
    for cmd, step_nums in bash_cmds.items():
        if len(step_nums) >= 3:
            t.issues.append(Issue(
                "HIGH", "repeated-command",
                f"Same command executed {len(step_nums)} times at steps {step_nums}: '{cmd[:60]}'",
                step_nums,
            ))
        elif len(step_nums) >= 2:
            t.issues.append(Issue(
                "LOW", "repeated-command",
                f"Same command executed {len(step_nums)} times at steps {step_nums}: '{cmd[:60]}'",
                step_nums,
            ))

    # No final verification
    if t.total_steps > 3:
        last_tools = [s["tool"] for s in steps[-3:]] if len(steps) >= 3 else []
        has_verify = any(t == "Bash" for t in last_tools)
        if not has_verify and t.total_steps > 5:
            t.issues.append(Issue(
                "MEDIUM", "no-final-verify",
                "No build/curl verification in the last 3 steps",
            ))

    # Read but not edited (wasted reads)
    if t.files_edited:
        read_not_edited = set(t.files_read.keys()) - set(t.files_edited.keys())
        # Filter out common reference files
        ignore = {"index.ts", "api.ts", "router", "App.vue", "style.css"}
        wasted = [f for f in read_not_edited if f not in ignore and t.files_read.get(f, 0) >= 2]
        if wasted:
            t.issues.append(Issue(
                "LOW", "read-not-edited",
                f"Files read but never edited: {', '.join(wasted)}",
            ))

    # Step count check
    prompt_lower = t.user_prompt.lower()
    if any(kw in prompt_lower for kw in ["启动", "start", "install", "安装"]):
        expected = 4
    elif any(kw in prompt_lower for kw in ["修复", "fix", "bug", "改", "调整"]):
        expected = 6
    elif any(kw in prompt_lower for kw in ["实现", "功能", "feature", "创建"]):
        expected = 20
    else:
        expected = 10

    if t.total_steps > expected * 2:
        t.issues.append(Issue(
            "HIGH", "step-overrun",
            f"{t.total_steps} steps (expected ~{expected} for this task type, 2x over)",
        ))
    elif t.total_steps > expected * 1.5:
        t.issues.append(Issue(
            "MEDIUM", "step-overrun",
            f"{t.total_steps} steps (expected ~{expected}, 1.5x over)",
        ))

    # Sort issues: HIGH > MEDIUM > LOW > BUG > WARN
    severity_order = {"HIGH": 0, "BUG": 1, "MEDIUM": 2, "WARN": 3, "LOW": 4}
    t.issues.sort(key=lambda i: severity_order.get(i.severity, 5))

    return t


def format_report(analyses: list) -> str:
    """Generate a markdown evaluation report."""
    lines = []
    lines.append("# AtomCode Evaluation Report\n")

    # Summary
    total = len(analyses)
    high_issues = sum(1 for a in analyses if any(i.severity == "HIGH" for i in a.issues))
    avg_steps = sum(a.total_steps for a in analyses) / total if total else 0
    avg_duration = sum(a.duration_secs for a in analyses) / total if total else 0
    total_issues = sum(len(a.issues) for a in analyses)

    lines.append("## Summary\n")
    lines.append(f"| Metric | Value |")
    lines.append(f"|--------|-------|")
    lines.append(f"| Turns analyzed | {total} |")
    lines.append(f"| Turns with HIGH issues | {high_issues}/{total} |")
    lines.append(f"| Total issues found | {total_issues} |")
    lines.append(f"| Avg steps/turn | {avg_steps:.1f} |")
    lines.append(f"| Avg duration/turn | {avg_duration:.0f}s |")
    lines.append("")

    # Issue frequency
    from collections import Counter
    rule_counts = Counter()
    for a in analyses:
        for i in a.issues:
            rule_counts[f"[{i.severity}] {i.rule}"] += 1
    if rule_counts:
        lines.append("## Issue Frequency\n")
        lines.append("| Issue | Count |")
        lines.append("|-------|-------|")
        for rule, count in rule_counts.most_common():
            lines.append(f"| {rule} | {count} |")
        lines.append("")

    # Per-turn details
    lines.append("## Per-Turn Details\n")
    for a in analyses:
        status = "PASS" if not any(i.severity == "HIGH" for i in a.issues) else "FAIL"
        lines.append(f"### {a.file} — {status}")
        lines.append(f"- **Prompt:** \"{a.user_prompt[:80]}\"")
        lines.append(f"- **Steps:** {a.total_steps} | **Duration:** {a.duration_secs:.0f}s")

        if a.files_read:
            reads = ", ".join(f"{f}({c}x)" for f, c in sorted(a.files_read.items(), key=lambda x: -x[1]) if c > 1)
            if reads:
                lines.append(f"- **Re-reads:** {reads}")

        if a.issues:
            lines.append(f"- **Issues ({len(a.issues)}):**")
            for i in a.issues:
                lines.append(f"  - [{i.severity}] {i.rule}: {i.message}")
        else:
            lines.append(f"- No issues detected")
        lines.append("")

    return "\n".join(lines)


DEEP_ANALYSIS_PROMPT = """You are evaluating an AI coding agent called "atomcode".
Below is a log of one turn: the user's request and every tool call the agent made.
Also included are rule-based issues already detected.

Your job:
1. Compare with how Claude Code would handle the same task (fewer steps, better decisions).
2. Identify the ROOT CAUSE of each failure (model reasoning? framework limitation? prompt issue?).
3. Suggest ONE specific, actionable improvement to atomcode's framework or system prompt.
4. Rate overall quality: A (Claude Code level), B (acceptable), C (needs work), D (failure).

Be concise. Output format:

## Grade: X
## Claude Code would: (2-3 sentences)
## Root causes: (bullet list)
## Suggested fix: (one specific change)

---

### Rule-based issues already found:
{issues}

### Full datalog:
{datalog}
"""


def deep_analyze(analysis: TurnAnalysis, datalog_path: str) -> str:
    """Call claude -p for deep analysis of a FAIL turn."""
    datalog_content = open(datalog_path).read()
    issues_text = "\n".join(f"- [{i.severity}] {i.rule}: {i.message}" for i in analysis.issues)

    prompt = DEEP_ANALYSIS_PROMPT.format(
        issues=issues_text or "(none)",
        datalog=datalog_content,
    )

    try:
        result = subprocess.run(
            ["claude", "-p", prompt],
            capture_output=True,
            text=True,
            timeout=120,
        )
        if result.returncode == 0 and result.stdout.strip():
            return result.stdout.strip()
        else:
            return f"(claude -p failed: {result.stderr.strip()[:200]})"
    except FileNotFoundError:
        return "(claude CLI not found — install Claude Code to enable deep analysis)"
    except subprocess.TimeoutExpired:
        return "(claude -p timed out after 120s)"


def main():
    deep_mode = "--deep" in sys.argv
    args = [a for a in sys.argv[1:] if not a.startswith("--")]

    if not args:
        print("Usage: python3 analyze_datalogs.py [--deep] <path-to-datalog-dir-or-file>")
        sys.exit(1)

    target = args[0]
    files = []

    if os.path.isdir(target):
        files = sorted(Path(target).glob("*.md"))
    elif os.path.isfile(target):
        files = [Path(target)]
    else:
        print(f"Error: {target} not found")
        sys.exit(1)

    analyses = []
    file_paths = {}  # analysis -> original file path
    for f in files:
        try:
            data = parse_datalog(str(f))
            if data["total_steps"] == 0:
                continue
            analysis = analyze(data)
            analyses.append(analysis)
            file_paths[id(analysis)] = str(f)
        except Exception as e:
            print(f"Warning: failed to parse {f}: {e}", file=sys.stderr)

    if not analyses:
        print("No valid datalogs found.")
        sys.exit(0)

    report = format_report(analyses)
    print(report)

    # Deep analysis on FAIL turns
    if deep_mode:
        fail_turns = [a for a in analyses if any(i.severity == "HIGH" for i in a.issues)]
        if not fail_turns:
            print("\n## Deep Analysis\nNo FAIL turns to analyze.")
            return

        print(f"\n## Deep Analysis (Claude Code)\n")
        print(f"Analyzing {len(fail_turns)} FAIL turns...\n")

        for a in fail_turns:
            path = file_paths.get(id(a), "")
            print(f"### {a.file}")
            print(f"**Prompt:** \"{a.user_prompt[:80]}\" | **Steps:** {a.total_steps}\n")
            result = deep_analyze(a, path)
            print(result)
            print()


if __name__ == "__main__":
    main()
