#!/usr/bin/env python3
"""
Context Efficiency Analyzer — parses atomcode datalogs and reports:
1. ctx growth per turn (are we leaking?)
2. compression events (did compact trigger?)
3. cache efficiency estimate (prefix stable?)
4. time vs ctx correlation (bigger ctx = slower?)
5. waste ratio (cold/total)

Usage: python3 ctx-analyzer.py /path/to/datalog/*.md
       python3 ctx-analyzer.py /path/to/datalog/  (scans all .md files)
"""

import re, sys, os
from pathlib import Path

def parse_datalog(filepath):
    """Extract turns from a single datalog file."""
    text = Path(filepath).read_text()

    # Extract metadata
    model_match = re.search(r'model=([^,]+)', text)
    ctx_window_match = re.search(r'ctx_window=(\d+)', text)
    model = model_match.group(1) if model_match else "unknown"
    ctx_window = int(ctx_window_match.group(1)) if ctx_window_match else 0

    # Extract stats
    stats_match = re.search(r'\*\*Stats:\*\* (\d+) turns, (\d+) tool calls, ([\d.]+)s', text)
    total_turns = int(stats_match.group(1)) if stats_match else 0
    total_tools = int(stats_match.group(2)) if stats_match else 0
    total_time = float(stats_match.group(3)) if stats_match else 0

    # Extract per-turn ctx
    turns = []
    ctx_pattern = re.compile(r'_\[ctx: (\d+)tok = sys:(\d+)\+hot:(\d+)\+cold:(\d+)\+ws:(\d+), msgs:(\d+)\]_')
    time_pattern = re.compile(r'_\(([\d.]+)s\)_')

    # Split by turns
    turn_blocks = re.split(r'### Turn \d+', text)[1:]  # skip header

    for i, block in enumerate(turn_blocks):
        ctx_matches = ctx_pattern.findall(block)
        time_matches = time_pattern.findall(block)

        if ctx_matches:
            last_ctx = ctx_matches[-1]  # take last ctx line in turn (after retries)
            total, sys_tok, hot, cold, ws, msgs = [int(x) for x in last_ctx]
            turn_time = float(time_matches[-1]) if time_matches else 0

            turns.append({
                'turn': i + 1,
                'total': total,
                'system': sys_tok,
                'hot': hot,
                'cold': cold,
                'ws': ws,
                'msgs': msgs,
                'time': turn_time,
            })

    return {
        'file': os.path.basename(filepath),
        'model': model,
        'ctx_window': ctx_window,
        'total_turns': total_turns,
        'total_tools': total_tools,
        'total_time': total_time,
        'turns': turns,
    }

def analyze(session):
    """Compute efficiency metrics for a session."""
    turns = session['turns']
    if len(turns) < 2:
        return None

    ctx_window = session['ctx_window']

    # 1. Growth
    first_ctx = turns[0]['total']
    last_ctx = turns[-1]['total']
    growth = last_ctx - first_ctx
    avg_growth = growth / max(len(turns) - 1, 1)

    # 2. Compression events (ctx drops between consecutive turns)
    compressions = []
    for i in range(1, len(turns)):
        delta = turns[i]['total'] - turns[i-1]['total']
        if delta < -500:  # significant drop
            compressions.append({
                'turn': turns[i]['turn'],
                'before': turns[i-1]['total'],
                'after': turns[i]['total'],
                'saved': turns[i-1]['total'] - turns[i]['total'],
            })

    # 3. Cache efficiency (prefix stability)
    # If cold zone changes between turns → prefix changed → cache miss
    cold_changes = 0
    for i in range(1, len(turns)):
        if turns[i]['cold'] != turns[i-1]['cold']:
            cold_changes += 1
    cache_hit_rate = 1.0 - (cold_changes / max(len(turns) - 1, 1))

    # 4. Time vs ctx correlation
    times = [t['time'] for t in turns if t['time'] > 0]
    ctxs = [t['total'] for t in turns if t['time'] > 0]

    # 5. Budget usage
    peak_ctx = max(t['total'] for t in turns)
    budget_usage = peak_ctx / ctx_window if ctx_window > 0 else 0
    over_budget = peak_ctx > ctx_window

    # 6. Cold ratio (waste indicator)
    avg_cold = sum(t['cold'] for t in turns) / len(turns)
    avg_total = sum(t['total'] for t in turns) / len(turns)
    cold_ratio = avg_cold / avg_total if avg_total > 0 else 0

    # 7. Time per 1K ctx tokens
    if ctxs and times:
        time_per_1k = sum(times) / (sum(ctxs) / 1000)
    else:
        time_per_1k = 0

    return {
        'first_ctx': first_ctx,
        'last_ctx': last_ctx,
        'peak_ctx': peak_ctx,
        'growth': growth,
        'avg_growth_per_turn': avg_growth,
        'compressions': compressions,
        'cold_changes': cold_changes,
        'cache_hit_rate': cache_hit_rate,
        'budget_usage': budget_usage,
        'over_budget': over_budget,
        'cold_ratio': cold_ratio,
        'avg_time': sum(times) / len(times) if times else 0,
        'time_per_1k': time_per_1k,
    }

def print_report(session, metrics):
    if not metrics:
        print(f"  {session['file']}: too few turns to analyze")
        return

    print(f"\n{'='*70}")
    print(f"  {session['file']}")
    print(f"  model={session['model']}  ctx_window={session['ctx_window']}")
    print(f"  {session['total_turns']}T / {session['total_time']:.0f}s / {session['total_tools']} tools")
    print(f"{'='*70}")

    # Context
    print(f"\n  Context:")
    print(f"    Start: {metrics['first_ctx']:,} tok")
    print(f"    Peak:  {metrics['peak_ctx']:,} tok", end="")
    if metrics['over_budget']:
        print(f"  [!!! OVER BUDGET by {metrics['peak_ctx'] - session['ctx_window']:,}]", end="")
    print()
    print(f"    End:   {metrics['last_ctx']:,} tok")
    print(f"    Growth: +{metrics['growth']:,} tok ({metrics['avg_growth_per_turn']:.0f}/turn)")
    print(f"    Budget usage: {metrics['budget_usage']:.0%} of {session['ctx_window']:,}")

    # Compression
    print(f"\n  Compression:")
    if metrics['compressions']:
        for c in metrics['compressions']:
            print(f"    T{c['turn']}: {c['before']:,} -> {c['after']:,} (saved {c['saved']:,})")
    else:
        print(f"    No compression events (good for cache)")

    # Cache
    print(f"\n  Cache estimate:")
    print(f"    Cold zone changes: {metrics['cold_changes']}")
    print(f"    Estimated cache hit rate: {metrics['cache_hit_rate']:.0%}")
    print(f"    Avg cold ratio: {metrics['cold_ratio']:.1%} of context")

    # Speed
    print(f"\n  Speed:")
    print(f"    Avg turn time: {metrics['avg_time']:.1f}s")
    print(f"    Time per 1K ctx: {metrics['time_per_1k']:.2f}s")

    # Verdict
    print(f"\n  Verdict:", end=" ")
    issues = []
    if metrics['over_budget']:
        issues.append("OVER BUDGET")
    if metrics['cache_hit_rate'] < 0.5:
        issues.append("LOW CACHE HIT")
    if metrics['avg_growth_per_turn'] > 1500:
        issues.append("FAST GROWTH")
    if metrics['cold_ratio'] > 0.3:
        issues.append("HIGH COLD RATIO")

    if not issues:
        print("OK")
    else:
        print(", ".join(issues))

def main():
    if len(sys.argv) < 2:
        print("Usage: python3 ctx-analyzer.py <datalog_dir_or_files>")
        sys.exit(1)

    files = []
    for arg in sys.argv[1:]:
        p = Path(arg)
        if p.is_dir():
            files.extend(sorted(p.glob("*.md")))
        elif p.is_file():
            files.append(p)

    if not files:
        print("No datalog files found")
        sys.exit(1)

    print(f"Analyzing {len(files)} datalog(s)...")

    for f in files:
        try:
            session = parse_datalog(f)
            metrics = analyze(session)
            print_report(session, metrics)
        except Exception as e:
            print(f"  {f.name}: parse error: {e}")

if __name__ == "__main__":
    main()
