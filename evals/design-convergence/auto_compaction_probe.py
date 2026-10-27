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
    args = parser.parse_args()
    binary = args.binary.resolve()
    records = []
    lock = threading.Lock()
    steps = 0
    summaries = 0
    fixture_dir = ""

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
                if summary:
                    summaries += 1
                    text = f"Prior work completed through checkpoint {summaries}. Keep working on {MARKER}."
                    usage = {"prompt_tokens": 1000, "completion_tokens": 30}
                    delta, finish = {"content": text}, "stop"
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
                        delta = {"content": f"work {steps}: " + "x" * 180000,
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
                records.append({"kind": kind, "ordinal": ordinal,
                                "request_bytes": len(raw), "messages": len(messages),
                                "reported_prompt_tokens": usage["prompt_tokens"],
                                "tool_count": len(body.get("tools", []))})
            if body.get("stream"):
                event = {"id": f"offline-{kind}-{ordinal}", "choices": [{
                    "index": 0, "delta": delta, "finish_reason": finish}]}
                trailer = {"choices": [], "usage": usage}
                payload = ("data: " + json.dumps(event) + "\n\ndata: " + json.dumps(trailer)
                           + "\n\ndata: [DONE]\n\n").encode()
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
              "context_window": 512000, "usage_is_synthetic": True}
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
            facts = []
            for path in home.rglob("*.events"):
                for line in path.read_text().splitlines()[1:]:
                    event = json.loads(line)["event"]
                    if event.get("kind") in ["compacted", "user_message", "turn_end"]:
                        facts.append({"kind": event["kind"], "turn": event.get("turn")})
            with lock:
                captured = [dict(record) for record in records]
            cuts = [fact for fact in facts if fact["kind"] == "compacted"]
            users = [fact for fact in facts if fact["kind"] == "user_message"]
            drops = 0
            for index, record in enumerate(captured):
                if record["kind"] != "summary":
                    continue
                before = next((r for r in reversed(captured[:index]) if r["kind"] == "work"), None)
                after = next((r for r in captured[index+1:] if r["kind"] == "work"), None)
                if before and after and after["request_bytes"] < before["request_bytes"]:
                    drops += 1
            result.update(requests=captured, facts=facts, compactions=len(cuts),
                          summaries=sum(r["kind"] == "summary" for r in captured),
                          verified_request_reductions=drops, user_turns=len(users))
            result["passed"] = (result["exit_code"] == 0 and len(users) == 1
                                and len(cuts) >= 2 and drops >= 2)
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
