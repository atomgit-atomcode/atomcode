#!/usr/bin/env python3
"""Report task costs from a finished lead .events file and its co-located team logs."""
import argparse
from collections import Counter, defaultdict, deque
from dataclasses import dataclass, field
import json
from pathlib import Path
import sys


@dataclass
class Turn:
    counts: Counter = field(default_factory=Counter)
    tasks: set = field(default_factory=set)
    sources: set = field(default_factory=set)
    stop: str = "open"
    first_input: str = ""
    requests_by_round: set = field(default_factory=set)
    usage_by_round: set = field(default_factory=set)
    team_calls: dict = field(default_factory=dict)


def read_session(path, turns, lead):
    with path.open(encoding="utf-8") as stream:
        header = json.loads(next(stream))["header"]
        if header["version"] > 15:
            raise ValueError(f"{path.name}: unsupported session version {header['version']}")
        session = header["id"]
        if header.get("inherited"):
            raise ValueError(f"{path.name}: inherited history cannot be billed as new work")
        if lead is None:
            if header.get("parent"):
                raise ValueError("provide a lead session")
            lead = session
        elif header.get("parent") != session.rpartition("/")[0] or not session.startswith(lead + "/"):
            raise ValueError(f"{path.name}: unexpected child identity")
        if session.replace("/", "~") + ".events" != path.name:
            raise ValueError(f"{path.name}: session identity mismatch")
        previous = -1
        for line in stream:
            row = json.loads(line)
            if row["seq"] <= previous:
                raise ValueError(f"{path.name}: sequence is not increasing")
            previous = row["seq"]
            event = row["event"]
            kind = event["kind"]
            key = (session, event["turn"])
            if kind == "turn_start":
                if key in turns:
                    raise ValueError(f"{path.name}: duplicate turn {key[1]}")
                turns[key] = Turn()
            if key not in turns:
                if kind in {"usage", "request_header", "user_message", "turn_end"}:
                    raise ValueError(f"{path.name}: missing turn start")
                continue
            turn = turns[key]
            if kind == "user_message":
                turn.tasks.add(f"{session}#{row['seq']}")
                turn.first_input = turn.first_input or "user"
            elif kind == "injected":
                origin = event["origin"]
                if isinstance(origin, dict) and "peer" in origin:
                    peer = origin["peer"]
                    if peer.get("source_turn") is not None:
                        turn.sources.add((peer["from"], peer["source_turn"]))
                        turn.first_input = turn.first_input or "peer"
                    else:
                        turn.counts["peer_messages_without_source_turn"] += 1
            elif kind == "request_header":
                turn.counts["model_rounds"] += 1
                if not turn.first_input:
                    turn.first_input = "other"
            elif kind == "reply_checkpoint" and event.get("provider_call"):
                turn.counts["provider_calls_observed"] += 1
                turn.requests_by_round.add(event["round"])
            elif kind == "usage":
                usage = event["usage"]
                turn.counts["input_tokens_reported"] += usage["prompt"]
                turn.counts["output_tokens_reported"] += usage["completion"]
                turn.counts["cached_input_tokens_reported"] += usage["cached"]
                turn.counts["usage_records"] += 1
                turn.usage_by_round.add(event["round"])
            elif kind == "tool_started" and event["call"]["name"] == "team":
                call = event["call"]
                try:
                    args = json.loads(call["arguments"])
                except (TypeError, ValueError):
                    continue
                if isinstance(args, dict) and args.get("action") in ("delegate", "tell"):
                    turn.team_calls[call["id"]] = args["action"]
            elif kind == "tool_result_logged":
                action = turn.team_calls.pop(event["call_id"], None)
                if action and not event["is_error"]:
                    turn.counts["delegations" if action == "delegate" else "followups"] += 1
            elif kind == "turn_end":
                turn.stop = event["stop"]
    return header


def report(path):
    turns = {}
    root = read_session(path, turns, None)["id"]
    roles = {root: "lead"}
    for child in sorted(path.parent.iterdir()):
        if child.name.startswith(path.stem + "~") and child.suffix == ".events":
            head = read_session(child, turns, root)
            roles[head["id"]] = (head.get("member") or {}).get("role", "child")

    # Reports folded into the lead's still-running turn can form a cycle with
    # the corresponding delegation. Propagate sets until no new owner appears.
    children = defaultdict(set)
    for key, turn in turns.items():
        for source in turn.sources:
            if source in turns:
                children[source].add(key)
            else:
                turn.tasks.add("unattributed")
        if not turn.sources and not turn.tasks:
            turn.tasks.add("unattributed")
    queue = deque(key for key, turn in turns.items() if turn.tasks)
    while queue:
        source = queue.popleft()
        for child in children[source]:
            added = turns[source].tasks - turns[child].tasks
            if added:
                turns[child].tasks.update(added)
                queue.append(child)

    accounts = defaultdict(Counter)
    sessions = defaultdict(Counter)
    stops = defaultdict(Counter)
    total = Counter()
    detail = []
    for (session, number), turn in sorted(turns.items()):
        owners = sorted(turn.tasks) or ["unattributed"]
        account = owners[0] if len(owners) == 1 else "shared"
        counts = turn.counts
        counts["turns"] = 1
        counts["lead_wakeups"] = int(session == root and turn.first_input == "peer")
        counts["rounds_without_call_marker"] = counts["model_rounds"] - len(turn.requests_by_round)
        counts["rounds_without_reported_usage"] = counts["model_rounds"] - len(turn.usage_by_round)
        counts["open_turns"] = int(turn.stop == "open")
        accounts[account].update(counts)
        sessions[session].update(counts)
        total.update(counts)
        stops[account][turn.stop] += 1
        detail.append(dict(session=session, role=roles[session], turn=number, tasks=owners,
                           stop=turn.stop, **counts))
    return dict(lead=root, totals=total, by_task=dict(accounts),
                by_session={key: dict(role=roles[key], **counts) for key, counts in sessions.items()},
                stop_reasons_by_task=dict(stops), turns=detail)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("lead_events", type=Path)
    args = parser.parse_args()
    try:
        print(json.dumps(report(args.lead_events), indent=2, ensure_ascii=False))
    except (OSError, ValueError, KeyError, TypeError, StopIteration) as error:
        sys.exit(f"Cannot read team task costs: {error}")
