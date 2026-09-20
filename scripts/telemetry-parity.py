#!/usr/bin/env python3
"""Two builds, one scripted session, the same telemetry — or a diff that says why.

The wire goldens (`crates/atomcode-telemetry/tests/golden/wire/`) pin the SHAPE
of each event, and the criteria in `atomcode-coding`, `atomcode-tui` and
`atomcode-cli` pin that each one is emitted. Neither can see what a whole
process actually puts on the wire after a real session: which events fire, how
many, in what order, with what envelope. That is what this is for.

It runs two binaries against the same fake model and the same prompt, collects
what each POSTs to a local telemetry endpoint, normalises away the parts that
must differ (ids, timestamps, durations, paths), and diffs.

    scripts/telemetry-parity.py --old ~/.local/bin/atomcode --new target/release/atomcode

Nothing leaves the machine: both the model and the telemetry endpoint are
sockets on 127.0.0.1, and `ATOMCODE_HOME` points at a scratch directory, so
neither run can see the real config, sessions or queue.

## What the old binary can and cannot be an oracle for

`mcp_connect` was dropped from the product in `f296e6e2` (2026-07-24) and no
release since emits it, so no shipped binary can be its oracle — it is covered
by the wire golden and by `a_failed_mcp_connection_is_metered` instead. Every
other event is fair game.

`use_command` needs a terminal: it is reported by a front end, not by a
headless run, so `--headless-only` (the default) will not see it. That half is
pinned by `atomcode-tui/src/command.rs`, `atomcode-cli/src/tui_command_meter.rs`
and `atomcode-tuix/tests/use_command_oracle.rs`.
"""

import argparse
import gzip
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

# ---- the fake model --------------------------------------------------------

# One scripted session: a round that calls a tool, then a round that answers.
# Rich enough that `llm_chat` carries usage and `tool_call` fires at all, and
# short enough that a run is a couple of seconds.
# How long the fake model takes to answer. An instant model is not a realistic
# input: `duration_ms` is milliseconds, `as_millis()` truncates, and a
# sub-millisecond round trip reports `0` on one build and `2` on another purely
# because they start their clocks a few instructions apart. That difference is
# noise for any real model and unreadable as a signal. Answering slowly enough
# to be measurable turns `duration_ms` back into something the diff can judge:
# both builds must report a duration in the same ballpark as this.
MODEL_DELAY_S = 0.05

SCRIPT = [
    {
        "tool_calls": [
            {
                "id": "call_1",
                "type": "function",
                "function": {"name": "bash", "arguments": '{"command":"echo parity"}'},
            }
        ]
    },
    {"content": "done"},
]


def sse(obj):
    return f"data: {json.dumps(obj)}\n\n".encode()


class Handler(BaseHTTPRequestHandler):
    # Shared across both runs; reset between them by the driver.
    collected = []
    round_index = [0]
    lock = threading.Lock()

    def log_message(self, *_):
        pass  # the driver prints what matters

    def _read_body(self):
        length = int(self.headers.get("content-length", 0))
        raw = self.rfile.read(length) if length else b""
        if self.headers.get("content-encoding") == "gzip":
            raw = gzip.decompress(raw)
        return raw

    def do_POST(self):
        if self.path.startswith("/telemetry"):
            body = self._read_body()
            with Handler.lock:
                for line in body.decode("utf-8", "replace").splitlines():
                    if line.strip():
                        Handler.collected.append(json.loads(line))
            self.send_response(200)
            self.send_header("content-length", "0")
            self.end_headers()
            return

        # The model. Answer with whichever scripted round is next; repeat the
        # last one if the agent asks for more than the script has, so a build
        # that takes an extra round cannot hang the run.
        self._read_body()
        with Handler.lock:
            i = min(Handler.round_index[0], len(SCRIPT) - 1)
            Handler.round_index[0] += 1
        step = SCRIPT[i]

        time.sleep(MODEL_DELAY_S)
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        base = {
            "id": "chatcmpl-parity",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "parity",
        }
        if "tool_calls" in step:
            delta = {"role": "assistant", "tool_calls": []}
            for n, call in enumerate(step["tool_calls"]):
                delta["tool_calls"].append({"index": n, **call})
            self.wfile.write(sse({**base, "choices": [{"index": 0, "delta": delta}]}))
            finish = "tool_calls"
        else:
            self.wfile.write(
                sse(
                    {
                        **base,
                        "choices": [
                            {"index": 0, "delta": {"role": "assistant", "content": step["content"]}}
                        ],
                    }
                )
            )
            finish = "stop"
        self.wfile.write(
            sse(
                {
                    **base,
                    "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
                    # So `llm_chat` carries real numbers rather than zeroes.
                    "usage": {
                        "prompt_tokens": 1000,
                        "completion_tokens": 20,
                        "total_tokens": 1020,
                        "prompt_tokens_details": {"cached_tokens": 512},
                    },
                }
            )
        )
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


# ---- normalising -----------------------------------------------------------

# Every field that MUST differ between two runs of two builds. Dropping them is
# what makes the rest comparable; each one is dropped for a stated reason, so a
# field that starts differing for a NEW reason still shows up as a diff.
VOLATILE_ENVELOPE = {
    "device_id",  # per install
    "launch_id",  # per process
    "session_id",  # per session
    "turn_id",  # per turn
    "ts",  # wall clock
    "app_version",  # the whole point is that they differ
    "account_id",  # whoever is signed in
    "repo_origin",  # the scratch dir is not a repo; kept out to be sure
}
# Compared by order of magnitude rather than dropped: with a model that takes
# `MODEL_DELAY_S` to answer, "both report tens of milliseconds" is a real
# assertion, and "one of them reports zero" is a real finding.
MAGNITUDE_EVENT = {"duration_ms"}

VOLATILE_EVENT = {
    "error_data",  # carries paths, messages and durations
    # Token accounting: the two builds send different prompts (the system
    # prompt is not the thing under test), so the exact numbers cannot match.
    # Their PRESENCE is what matters, and `shape()` keeps that.
    "input_tokens",
    "output_tokens",
    "cached_tokens",
    "context_window",
    "system_tokens",
    "tool_def_tokens",
    "tool_result_tokens",
    "message_tokens",
    "messages_count",
    "tool_calls_count",
}


def shape(record):
    """What must be identical: which event, and every non-volatile field.

    A dropped numeric field is replaced by whether it was present and whether
    it was non-zero, so "stopped reporting tokens" is still a diff even though
    "reported 1013 instead of 1007" is not.
    """
    out = {}
    for key, value in sorted(record.items()):
        if key in VOLATILE_ENVELOPE:
            continue
        if key in MAGNITUDE_EVENT and isinstance(value, (int, float)):
            # 0, 1-9ms, 10-99ms, … — same bucket means the two builds measure
            # the same span; a build that stopped measuring drops to `0ms`.
            out[key] = f"{10 ** len(str(int(value)).lstrip('-')) // 10}ms+" if value else "0ms"
            continue
        if key in VOLATILE_EVENT:
            if isinstance(value, (int, float)):
                out[key] = "<zero>" if value == 0 else "<nonzero>"
            else:
                out[key] = "<present>"
            continue
        out[key] = value
    return out


def summarise(records):
    lines = []
    for record in records:
        lines.append(json.dumps(shape(record), sort_keys=True, ensure_ascii=False))
    return lines


# ---- running one build -----------------------------------------------------

CONFIG = """\
default_provider = "parity"

[providers.parity]
type = "openai"
model = "parity"
api_key = "not-a-secret"
base_url = "http://127.0.0.1:{port}/v1"
context_window = 128000

[telemetry]
enabled = true
endpoint = "http://127.0.0.1:{port}/telemetry"
"""


def run_one(binary, port, prompt, keep):
    Handler.collected.clear()
    Handler.round_index[0] = 0

    work = Path(tempfile.mkdtemp(prefix="atomcode-parity-"))
    home = work / "home"
    project = work / "project"
    home.mkdir()
    project.mkdir()
    (project / "README.md").write_text("parity probe\n")
    config = home / "config.toml"
    config.write_text(CONFIG.format(port=port))

    env = dict(os.environ)
    env["ATOMCODE_HOME"] = str(home)
    # Belt and braces: the config already points telemetry at the local
    # collector, and this makes it impossible for a build that reads the
    # config differently to reach the real endpoint.
    env["ATOMCODE_TELEMETRY_ENDPOINT"] = f"http://127.0.0.1:{port}/telemetry"
    env["ATOMCODE_PROXY_MODE"] = "no_proxy"
    env.pop("ATOMCODE_TELEMETRY", None)
    env.pop("DO_NOT_TRACK", None)

    cmd = [
        str(binary),
        "-p",
        prompt,
        "-C",
        str(project),
        "-y",  # the scripted tool call must not stop on an approval prompt
        "--dev",  # no self-update in the middle of a measurement
        "--config",
        str(config),
    ]
    started = time.time()
    proc = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=180)
    elapsed = time.time() - started

    # The shutdown drain is bounded; give the collector a moment to see it.
    deadline = time.time() + 5
    while time.time() < deadline:
        with Handler.lock:
            if Handler.collected:
                break
        time.sleep(0.1)
    time.sleep(0.5)

    with Handler.lock:
        records = list(Handler.collected)

    if keep:
        print(f"  scratch kept at {work}")
    else:
        shutil.rmtree(work, ignore_errors=True)
    return {
        "records": records,
        "exit": proc.returncode,
        "elapsed": elapsed,
        "stderr": proc.stderr[-2000:],
        "stdout": proc.stdout[-2000:],
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--old", default=str(Path.home() / ".local/bin/atomcode"))
    ap.add_argument("--new", default="target/release/atomcode")
    ap.add_argument("--prompt", default="Run the bash tool once, then say done.")
    ap.add_argument("--keep", action="store_true", help="keep the scratch dirs")
    ap.add_argument(
        "--raw",
        action="store_true",
        help="also print every record unnormalised, for reading a diff that "
        "normalisation reduced to `<zero>` vs `<nonzero>`",
    )
    args = ap.parse_args()

    for which, path in (("old", args.old), ("new", args.new)):
        if not Path(path).exists():
            print(f"no {which} binary at {path}", file=sys.stderr)
            return 2

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()
    print(f"fake model + telemetry collector on 127.0.0.1:{port}\n")

    runs = {}
    for which, path in (("old", args.old), ("new", args.new)):
        version = subprocess.run(
            [path, "--version"], capture_output=True, text=True
        ).stdout.strip()
        print(f"{which}: {path}  ({version})")
        runs[which] = run_one(path, port, args.prompt, args.keep)
        run = runs[which]
        ids = [r.get("event_id") for r in run["records"]]
        print(f"  exit {run['exit']} in {run['elapsed']:.1f}s, {len(ids)} event(s): {ids}")
        if run["exit"] != 0:
            print(f"  stderr tail:\n{run['stderr']}")
        if args.raw:
            for record in run["records"]:
                print(f"    {json.dumps(record, sort_keys=True, ensure_ascii=False)}")
    server.shutdown()

    old = summarise(runs["old"]["records"])
    new = summarise(runs["new"]["records"])

    print("\n--- diff (old → new), volatile fields normalised ---")
    if not old and not new:
        print("NEITHER build reported anything — the collector saw no POST at all.")
        print("That is a broken harness, not parity. Check the run output above.")
        return 2
    import difflib

    delta = list(difflib.unified_diff(old, new, "old", "new", lineterm="", n=1))
    if not delta:
        print(f"identical: {len(old)} record(s) match shape for shape")
        return 0
    for line in delta:
        print(line)
    return 1


if __name__ == "__main__":
    sys.exit(main())
