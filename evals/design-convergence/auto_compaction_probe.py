#!/usr/bin/env python3
"""Offline native single-turn automatic compaction at a 512k context window.

A local fake provider reports controlled usage and produces large work messages.
This checks the real runtime, trigger, provider protocol and persisted events;
it is not a DeepSeek tokenizer/model-quality test and never calls a real model.
"""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading

MARKER = "AUTO_COMPACT_LONG_TURN"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--summary-outcome", choices=[
        "success", "error", "empty", "truncated", "disconnected", "oversized"
    ], default="success")
    parser.add_argument("--resume-after", action="store_true")
    parser.add_argument("--work-bytes", type=int, default=180000)
    parser.add_argument("--expect-overflow-fallback", action="store_true")
    parser.add_argument("--expect-overflow-refusal", action="store_true")
    args = parser.parse_args()
    binary = args.binary.resolve()
    records = []
    lock = threading.Lock()
    steps = 0
    summaries = 0
    fixture_dir = ""
    phase = 0

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            nonlocal steps, summaries
            raw = self.rfile.read(int(self.headers["Content-Length"]))
            body = json.loads(raw)
            messages = body.get("messages", [])
            system = str(messages[0].get("content", "")) if messages else ""
            summary = "anchored context summarization assistant" in system
            work = bool(body.get("tools")) and any(
                MARKER in str(message.get("content", "")) for message in messages
            )
            with lock:
                outcome = "success" if phase and args.expect_overflow_refusal else args.summary_outcome
                if summary:
                    summaries += 1
                    text = f"Prior work completed through checkpoint {summaries}. Keep working on {MARKER}."
                    usage = {"prompt_tokens": 1000, "completion_tokens": 30}
                    if outcome == "empty":
                        text = ""
                    elif outcome == "oversized":
                        text = "x" * 70000
                    delta, finish = {"content": text}, "stop"
                    if outcome == "truncated":
                        finish = "length"
                    elif outcome == "disconnected":
                        finish = None
                    kind, ordinal = "summary", summaries
                elif work:
                    steps += 1
                    ordinal = steps
                    # Two high -> low -> high cycles inside ONE user turn.
                    pressure = [100000, 200000, 300000, 460800, 100000,
                                200000, 300000, 460800, 100000, 200000, 100000]
                    usage = {"prompt_tokens": pressure[min(steps-1, len(pressure)-1)],
                             "completion_tokens": 100}
                    if steps <= 10:
                        delta = {"content": f"work {steps}: " + "x" * args.work_bytes,
                                 "tool_calls": [{"index": 0, "id": f"read-{steps}",
                                  "function": {"name": "read_file", "arguments": json.dumps({
                                      "file_path": str(Path(fixture_dir) / f"step-{steps}.txt")})}}]}
                        finish = "tool_calls"
                    else:
                        delta, finish = {"content": "done"}, "stop"
                    kind = "work"
                else:
                    delta, finish = {"content": "ok"}, "stop"
                    usage = {"prompt_tokens": 10, "completion_tokens": 1}
                    kind, ordinal = "auxiliary", 0
                pending = set()
                paired = True
                for message in messages:
                    for call in message.get("tool_calls", []):
                        pending.add(call["id"])
                    if message.get("role") == "tool":
                        call_id = message.get("tool_call_id")
                        if call_id not in pending:
                            paired = False
                        pending.discard(call_id)
                records.append({"kind": kind, "ordinal": ordinal, "phase": phase,
                                "summary_succeeded": kind == "summary" and outcome == "success",
                                "tool_pairing_valid": paired and not pending,
                                "opening_request_present": any(
                                    MARKER + ": keep working until done; use the provided fixture files."
                                    in str(m.get("content", "")) for m in messages
                                ),
                                "first_work_present": any("work 1: " in str(m.get("content", "")) for m in messages),
                                "anchor_count": sum("<!-- atomcode:anchor v1 -->" in str(m.get("content", "")) for m in messages),
                                "request_bytes": len(raw), "messages": len(messages),
                                "reported_prompt_tokens": usage["prompt_tokens"],
                                "tool_count": len(body.get("tools", []))})
            if summary and outcome == "error":
                payload = json.dumps({"error": {"message": "offline summary rejection", "type": "permission_error"}}).encode()
                self.send_response(403)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
                return
            if body.get("stream"):
                event = {"id": f"offline-{kind}-{ordinal}", "choices": [{
                    "index": 0, "delta": delta, "finish_reason": finish}]}
                trailer = {"choices": [], "usage": usage}
                payload = ("data: " + json.dumps(event) + "\n\ndata: " + json.dumps(trailer)
                           + "\n\ndata: [DONE]\n\n").encode()
                if summary and outcome == "disconnected":
                    payload = ("data: " + json.dumps(event) + "\n\n").encode()
                content_type = "text/event-stream"
            else:
                payload = json.dumps({"choices": [{"message": {"role": "assistant",
                    "content": delta.get("content", "ok")}, "finish_reason": "stop"}],
                    "usage": usage}).encode()
                content_type = "application/json"
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    result = {"binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "context_window": 512000, "usage_is_synthetic": True,
              "summary_outcome": args.summary_outcome, "resume_requested": args.resume_after,
              "work_bytes": args.work_bytes, "expect_overflow_fallback": args.expect_overflow_fallback,
              "expect_overflow_refusal": args.expect_overflow_refusal}
    try:
        with tempfile.TemporaryDirectory(prefix="atomcode-auto-compact-") as temp:
            root = Path(temp)
            home, project = root / "home", root / "project"
            home.mkdir(mode=0o700)
            project.mkdir()
            fixture_dir = str(project)
            for step in range(1, 11):
                (project / f"step-{step}.txt").write_text(f"fixture for step {step}")
            (home / "config.toml").write_text(
                'default_provider="probe"\n[providers.probe]\ntype="openai"\n'
                'model="deepseek-v4.1-flash-1017"\napi_key="offline-probe-dummy"\n'
                f'base_url="http://127.0.0.1:{server.server_port}/v1"\n'
                'context_window=512000\nmax_tokens=32768\nretry_max_attempts=1\n'
                '[coding]\nmax_rounds=20\n[telemetry]\nenabled=false\n')
            env = {**os.environ, "ATOMCODE_HOME": str(home),
                   "NO_PROXY": "127.0.0.1,localhost", "no_proxy": "127.0.0.1,localhost"}
            try:
                process = subprocess.run([str(binary), "-C", str(project), "--dev",
                    "--no-telemetry", "--output-format", "jsonl", "-p",
                    MARKER + ": keep working until done; use the provided fixture files."],
                    env=env, capture_output=True, timeout=90)
                result.update(exit_code=process.returncode, status="completed")
            except subprocess.TimeoutExpired:
                result.update(exit_code=None, status="timeout")
            initial_facts = [json.loads(line)["event"] for path in home.rglob("*.events")
                             for line in path.read_text().splitlines()[1:]]
            result["initial_compactions"] = sum(e.get("kind") == "compacted" for e in initial_facts)
            result["context_refusal_reported"] = any(
                e.get("kind") == "turn_end" and "history was retained" in (e.get("error") or "")
                for e in initial_facts
            )
            if args.resume_after and (result["exit_code"] == 0 or args.expect_overflow_refusal):
                phase = 1
                sessions = list(home.rglob("*.events"))
                if len(sessions) == 1:
                    try:
                        resumed = subprocess.run([str(binary), "-C", str(project), "--dev",
                            "--no-telemetry", "--output-format", "jsonl", "--resume", sessions[0].stem,
                            "-p", "AUTO_COMPACT_RESUME_CHECK: continue from saved history."],
                            env=env, capture_output=True, timeout=90)
                        result["resume_exit_code"] = resumed.returncode
                    except subprocess.TimeoutExpired:
                        result["resume_exit_code"] = None
            facts = []
            raw_history_retained = False
            for path in home.rglob("*.events"):
                raw_log = path.read_text()
                raw_history_retained |= "work 1: " in raw_log
                for line in raw_log.splitlines()[1:]:
                    event = json.loads(line)["event"]
                    if event.get("kind") in ["compacted", "user_message", "turn_end"]:
                        facts.append({"kind": event["kind"], "turn": event.get("turn")})
            with lock:
                captured = [dict(record) for record in records]
            cuts = [fact for fact in facts if fact["kind"] == "compacted"]
            users = [fact for fact in facts if fact["kind"] == "user_message"]
            drops = 0
            for index, record in enumerate(captured):
                if record["kind"] != "summary" or not record["summary_succeeded"]:
                    continue
                before = next((r for r in reversed(captured[:index]) if r["kind"] == "work"), None)
                after = next((r for r in captured[index+1:] if r["kind"] == "work"), None)
                if before and after and after["request_bytes"] < before["request_bytes"]:
                    drops += 1
            result.update(requests=captured, facts=facts, compactions=len(cuts),
                          summaries=sum(r["kind"] == "summary" for r in captured),
                          verified_request_reductions=drops, user_turns=len(users))
            work = [r for r in captured if r["kind"] == "work"]
            pairing_ok = bool(work) and all(r["tool_pairing_valid"] for r in work)
            opening_ok = bool(work) and all(r["opening_request_present"] for r in work)
            resume_ok = not args.resume_after or result.get("resume_exit_code") == 0
            if args.expect_overflow_refusal:
                initial_work = [r for r in work if r["phase"] == 0]
                contract_ok = (result["context_refusal_reported"] and result["initial_compactions"] == 0
                               and initial_work[-1]["ordinal"] == 9
                               and all(r["first_work_present"] for r in initial_work[1:])
                               and (not args.resume_after or bool(cuts)))
            elif args.summary_outcome == "success":
                contract_ok = len(cuts) >= 2 and drops >= 2 and work[-1]["anchor_count"] == 1
            elif args.expect_overflow_fallback:
                contract_ok = (bool(cuts) and not work[-1]["first_work_present"]
                               and work[-1]["anchor_count"] == 0 and raw_history_retained)
            else:
                contract_ok = (not cuts and result["summaries"] == 2
                               and all(r["first_work_present"] for r in work[1:]))
            result.update(tool_pairing_valid=pairing_ok, opening_request_retained=opening_ok,
                          raw_history_retained=raw_history_retained,
                          first_work_retained=work[-1]["first_work_present"] if work else False)
            exit_ok = result["status"] == "completed" and (result["exit_code"] == 0 or args.expect_overflow_refusal)
            result["passed"] = (exit_ok and len(users) == (2 if args.resume_after else 1)
                                and pairing_ok and opening_ok and raw_history_retained
                                and resume_ok and contract_ok)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({key: value for key, value in result.items() if key not in ["requests", "facts"]}))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
