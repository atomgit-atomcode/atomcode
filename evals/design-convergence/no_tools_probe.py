#!/usr/bin/env python3
"""Check native tool opt-out using an isolated home and a local fake provider.

No real provider, credentials, user session, or user file is used. A successful
probe exits zero; a contract violation or timeout exits nonzero.
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

FIXTURE_MARKER = "OFFLINE_FIXTURE_MUST_NOT_BE_READ"
PROMPT = "Only reply received."


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--attempt-tool", action="store_true")
    parser.add_argument("--with-tools", action="store_true")
    args = parser.parse_args()
    binary = args.binary.resolve()
    records = []
    lock = threading.Lock()
    attempted = False
    fixture_path = ""

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            nonlocal attempted
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            with lock:
                records.append({
                    "messages": len(body.get("messages", [])),
                    "tool_names": [
                        tool.get("function", {}).get("name", tool.get("name"))
                        for tool in body.get("tools", [])
                    ],
                    "fixture_visible": FIXTURE_MARKER in json.dumps(body),
                })
                main_request = any(
                    message.get("role") == "user"
                    and PROMPT in str(message.get("content", ""))
                    for message in body.get("messages", [])
                )
                invoke = args.attempt_tool and main_request and not attempted and body.get("stream")
                if invoke:
                    attempted = True
            if body.get("stream"):
                if invoke:
                    delta = {"tool_calls": [{
                        "index": 0,
                        "id": "offline-read",
                        "function": {
                            "name": "read_file",
                            "arguments": json.dumps({"file_path": fixture_path}),
                        },
                    }]}
                    reason = "tool_calls"
                else:
                    delta, reason = {"content": "received"}, "stop"
                event = {"id": "offline-probe", "choices": [{
                    "index": 0, "delta": delta, "finish_reason": reason,
                }]}
                payload = ("data: " + json.dumps(event) + "\n\ndata: [DONE]\n\n").encode()
                content_type = "text/event-stream"
            else:
                payload = json.dumps({
                    "id": "offline-probe",
                    "model": body.get("model"),
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "received"},
                        "finish_reason": "stop",
                    }],
                }).encode()
                content_type = "application/json"
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="atomcode-no-tools-probe-") as temp:
            root = Path(temp)
            home, project = root / "home", root / "project"
            home.mkdir(mode=0o700)
            project.mkdir()
            fixture = project / "fixture.txt"
            fixture.write_text(FIXTURE_MARKER)
            fixture_path = str(fixture)
            config = (
                'default_provider="probe"\n[providers.probe]\ntype="openai"\n'
                'model="probe"\napi_key="offline-probe-dummy"\n'
                f'base_url="http://127.0.0.1:{server.server_port}/v1"\n'
                '[coding]\nmax_rounds=2\n[telemetry]\nenabled=false\n'
            )
            (home / "config.toml").write_text(config)
            env = {
                **os.environ,
                "ATOMCODE_HOME": str(home),
                "NO_PROXY": "127.0.0.1,localhost",
                "no_proxy": "127.0.0.1,localhost",
            }
            command = [str(binary), "-C", str(project), "--dev", "--no-telemetry"]
            if not args.with_tools:
                command.append("--no-tools")
            command += ["--output-format", "jsonl", "-p", PROMPT]
            try:
                process = subprocess.run(command, env=env, capture_output=True, timeout=45)
                result = {"status": "completed", "exit_code": process.returncode}
            except subprocess.TimeoutExpired:
                result = {"status": "timeout", "exit_code": None}
            with lock:
                captured = [dict(record) for record in records]
            empty = bool(captured) and all(not record["tool_names"] for record in captured)
            visible = any(record["fixture_visible"] for record in captured)
            passed = (
                result["exit_code"] == 0
                and bool(captured)
                and empty == (not args.with_tools)
                and (not args.attempt_tool or attempted)
                and (not args.attempt_tool or visible == args.with_tools)
            )
            result.update({
                "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                "requested_no_tools": not args.with_tools,
                "requests": captured,
                "empty_catalog": empty,
                "attempted_tool": attempted,
                "fixture_reached_model": visible,
                "passed": passed,
            })
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(result, ensure_ascii=False))
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
