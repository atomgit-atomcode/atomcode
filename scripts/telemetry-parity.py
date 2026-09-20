#!/usr/bin/env python3
"""Two builds, one set of scripted sessions, the same telemetry — or a diff.

The wire goldens (`crates/atomcode-telemetry/tests/golden/wire/`) pin the SHAPE
of each event, and the criteria in `atomcode-coding`, `atomcode-tui` and
`atomcode-cli` pin that each one is emitted. Neither can see what a whole
process actually puts on the wire after a real session: which events fire, how
many, in what order, with what envelope. That is what this is for.

It runs two binaries through the same scripted scenarios against the same fake
model, collects what each POSTs to a local telemetry endpoint, normalises away
the parts that must differ (ids, timestamps, exact token counts), and diffs.

    scripts/telemetry-parity.py --new target/debug/atomcode
    scripts/telemetry-parity.py --only approval-denied --raw
    scripts/telemetry-parity.py --list

It found `llm_chat.duration_ms` reporting `0` on every round (fixed in
`5fe755a2`) on its first run, which is the kind of thing it is for: the field
was present, well-typed and legal, and every test was green.

Nothing leaves the machine: both the model and the telemetry endpoint are
sockets on 127.0.0.1, and `ATOMCODE_HOME` points at a scratch directory, so
neither run can see the real config, sessions or queue.

## What the old binary can and cannot be an oracle for

`mcp_connect` was dropped from the product in `f296e6e2` (2026-07-24) and no
release since emits it, so no shipped binary can be its oracle — it is covered
by the wire golden and by `a_failed_mcp_connection_is_metered` instead.

`use_command` needs a terminal: it is reported by a front end, not by a
headless run. That half is pinned by `atomcode-tui/src/command.rs`,
`atomcode-cli/src/tui_command_meter.rs` and
`atomcode-tuix/tests/use_command_oracle.rs`.

Compaction is deliberately NOT a scenario. It would be reachable (a tiny
`context_window` plus a large tool result), but what it measures is not parity:
the compaction strategy changed on purpose between these builds, so the round
counts differ by design and the diff would report a decision as a defect.
"""

import argparse
import difflib
import gzip
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

# How long the fake model takes to answer. An instant model is not a realistic
# input: `duration_ms` is milliseconds, `as_millis()` truncates, and a
# sub-millisecond round trip reports `0` on one build and `2` on another purely
# because they start their clocks a few instructions apart. That difference is
# noise for any real model and unreadable as a signal. Answering slowly enough
# to be measurable turns `duration_ms` back into something the diff can judge.
MODEL_DELAY_S = 0.05


# ---- scenarios -------------------------------------------------------------


def text(body):
    return {"content": body}


def call(name, args):
    return {
        "tool_calls": [
            {
                "id": "call_1",
                "type": "function",
                "function": {"name": name, "arguments": json.dumps(args)},
            }
        ]
    }


def status(code, headers=None):
    """Answer with an HTTP status instead of a completion."""
    return {"status": code, "headers": headers or {}}


@dataclass
class Scenario:
    name: str
    why: str
    script: list
    args: list = field(default_factory=list)
    prompt: str = "do the thing"
    delay_s: float = MODEL_DELAY_S
    # Send SIGINT this many seconds in, for the scenario about cancelling.
    interrupt_after: float = None
    timeout_s: int = 180
    # Reporting nothing is this scenario's ANSWER, not a hole in it. Anywhere
    # else, silence means the scenario reached no metered path (or the harness
    # broke) and the run says so instead of calling it parity.
    expect_silence: bool = False


SCENARIOS = [
    Scenario(
        name="plain",
        why="the floor: one round, no tools. Everything else is this plus something.",
        script=[text("done")],
        args=["-y"],
    ),
    Scenario(
        name="tool-call",
        why="a tool runs and is reported. The `tool_call` success shape.",
        script=[call("bash", {"command": "echo parity"}), text("done")],
        args=["-y"],
    ),
    Scenario(
        name="tool-refused",
        why=(
            "a tool call that does not get to run, and how the refusal is "
            "classified. The single most valuable comparison here: the new "
            "engine decides `error_kind` by a substring test on the result "
            "(`coding/src/telemetry.rs`: `blocked:` prefix -> DeniedByUser, "
            "anything else -> ExecutionFailed), and whether that lands on the "
            "same bucket the retired engine used is not readable from either "
            "source.\n"
            "    A sensitive path, which is the documented hard floor and the "
            "trigger `coding/tests/sensitive_path.rs` itself uses. Three "
            "cheaper-looking triggers do NOT work, and each one quietly "
            "reported `success: true` until it was run:\n"
            "      - dropping `-y`: headless FENCES rather than asks "
            "(`on_harness.rs`, Presence::Headless patches `fs` with "
            "`root: Some(working_dir)`), so there is no prompt to refuse;\n"
            "      - `echo`: `RiskLevel::Safe`, straight through the gate;\n"
            "      - `rm -rf <relative>`: Risky, but inside the fence, so it "
            "runs.\n"
            "    Reading a key is also the one trigger that cannot destroy "
            "anything if some build decides to allow it."
        ),
        script=[
            call("read_file", {"file_path": "/home/u/.ssh/id_rsa"}),
            text("cannot read it"),
        ],
        args=["-y"],
    ),
    Scenario(
        name="unknown-tool",
        why=(
            "a name no tool answers to. The new engine builds the error result "
            "in the kernel, BEFORE the middleware chain — so its meter never "
            "sees the call, and the old engine may well have reported one. A "
            "missing record here is a real difference, not a flake."
        ),
        script=[call("definitely_not_a_tool", {"x": 1}), text("done")],
        args=["-y"],
    ),
    Scenario(
        name="provider-error",
        why=(
            "the model is down for the whole turn. Pins `had_error`, the "
            "`error_kind` classification, and how many `llm_chat` records a "
            "retried-then-abandoned turn produces."
        ),
        script=[status(500)],
        args=["-y"],
        timeout_s=120,
    ),
    Scenario(
        name="rate-limited",
        why=(
            "429 with a `Retry-After` the whole way. Separately classified from "
            "a plain server error, and the one error path with a deliberate "
            "wait in it."
        ),
        script=[status(429, {"retry-after": "1"})],
        args=["-y"],
        timeout_s=120,
    ),
    Scenario(
        name="cancelled",
        why=(
            "the person gives up mid-round. A slow model plus SIGINT.\n"
            "    The answer, on BOTH builds, is that it reports nothing at all — "
            "not even the `open_atomcode` from startup. `track` hands the record "
            "to a writer task behind a `BufWriter`, and SIGINT takes the process "
            "before anything rolls, so the queue on disk is empty too (this "
            "harness reads it, so that is a finding and not a blind spot). Kept "
            "because it is the answer, and because a build that started "
            "flushing — or stopped — would change it."
        ),
        script=[text("this answer never arrives in time")],
        args=["-y"],
        delay_s=6.0,
        interrupt_after=1.5,
        timeout_s=60,
        expect_silence=True,
    ),
]


# ---- the fake model + the collector ----------------------------------------


def sse(obj):
    return f"data: {json.dumps(obj)}\n\n".encode()


class Handler(BaseHTTPRequestHandler):
    # Set by the driver before each run.
    scenario = SCENARIOS[0]
    collected = []
    round_index = [0]
    lock = threading.Lock()

    def log_message(self, *_):
        pass  # the driver prints what matters

    def handle_one_request(self):
        # A cancelled run kills the client mid-stream. That is the scenario,
        # not an error, and the default handler prints a traceback per socket.
        try:
            super().handle_one_request()
        except (BrokenPipeError, ConnectionResetError):
            self.close_connection = True

    def _read_body(self):
        length = int(self.headers.get("content-length", 0))
        raw = self.rfile.read(length) if length else b""
        if self.headers.get("content-encoding") == "gzip":
            raw = gzip.decompress(raw)
        return raw

    def _empty(self, code, headers=None):
        self.send_response(code)
        for key, value in (headers or {}).items():
            self.send_header(key, value)
        self.send_header("content-length", "0")
        self.end_headers()

    def do_POST(self):
        if self.path.startswith("/telemetry"):
            body = self._read_body()
            with Handler.lock:
                for line in body.decode("utf-8", "replace").splitlines():
                    if line.strip():
                        Handler.collected.append(json.loads(line))
            self._empty(200)
            return

        self._read_body()
        scenario = Handler.scenario
        with Handler.lock:
            i = min(Handler.round_index[0], len(scenario.script) - 1)
            Handler.round_index[0] += 1
        step = scenario.script[i]

        time.sleep(scenario.delay_s)

        if "status" in step:
            self._empty(step["status"], step["headers"])
            return

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
            for n, tc in enumerate(step["tool_calls"]):
                delta["tool_calls"].append({"index": n, **tc})
            self.wfile.write(sse({**base, "choices": [{"index": 0, "delta": delta}]}))
            finish = "tool_calls"
        else:
            self.wfile.write(
                sse(
                    {
                        **base,
                        "choices": [
                            {
                                "index": 0,
                                "delta": {"role": "assistant", "content": step["content"]},
                            }
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
# `delay_s` to answer, "both report tens of milliseconds" is a real assertion,
# and "one of them reports zero" is a real finding.
MAGNITUDE_EVENT = {"duration_ms"}

VOLATILE_EVENT = {
    # Carries paths, wall-clock durations and the provider's own words. Its
    # PRESENCE is compared; its content cannot be.
    "error_data",
    # Token accounting: the two builds send different prompts (the system
    # prompt is not the thing under test), so exact numbers cannot match.
    # Presence and non-zero-ness are what `shape()` keeps.
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
    """What must be identical: which event, and every non-volatile field."""
    out = {}
    for key, value in sorted(record.items()):
        if key in VOLATILE_ENVELOPE:
            continue
        if key in MAGNITUDE_EVENT and isinstance(value, (int, float)):
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
    return [json.dumps(shape(r), sort_keys=True, ensure_ascii=False) for r in records]


# ---- running one scenario against one build --------------------------------

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


def read_queue(queue_dir):
    """Whatever is still sitting in the on-disk queue, oldest first.

    The sender claims a segment by renaming it, so a run interrupted mid-send
    can leave one under either name; both are read, because what is being
    asked is "what did this process decide to report", not "what reached the
    endpoint".
    """
    if not queue_dir.exists():
        return []
    out = []
    for path in sorted(queue_dir.iterdir()):
        if path.suffix in {".marker", ".lock"} or path.is_dir():
            continue
        try:
            body = path.read_text("utf-8")
        except OSError:
            continue
        for line in body.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                out.append(json.loads(line))
            except json.JSONDecodeError:
                pass  # a half-written last line in an interrupted segment
    return out


def run_one(binary, port, scenario, keep):
    Handler.scenario = scenario
    Handler.collected.clear()
    Handler.round_index[0] = 0

    work = Path(tempfile.mkdtemp(prefix=f"atomcode-parity-{scenario.name}-"))
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
        scenario.prompt,
        "-C",
        str(project),
        "--dev",  # no self-update in the middle of a measurement
        "--config",
        str(config),
        *scenario.args,
    ]
    started = time.time()
    timed_out = False
    interrupted = False
    proc = subprocess.Popen(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        if scenario.interrupt_after is not None:
            time.sleep(scenario.interrupt_after)
            if proc.poll() is None:
                proc.send_signal(signal.SIGINT)
                interrupted = True
        _out, err = proc.communicate(timeout=scenario.timeout_s)
    except subprocess.TimeoutExpired:
        timed_out = True
        proc.kill()
        _out, err = proc.communicate()
    elapsed = time.time() - started

    # The shutdown drain is bounded; give the collector a moment to see it.
    deadline = time.time() + 5
    while time.time() < deadline:
        with Handler.lock:
            if Handler.collected:
                break
        time.sleep(0.1)
    time.sleep(0.6)

    with Handler.lock:
        records = list(Handler.collected)
    # A run that was killed never got to drain, but the queue is on disk and
    # the next launch would have sent it. Reading it is what makes a cancelled
    # run measurable at all — otherwise the scenario compares nothing to
    # nothing and calls it parity.
    records += read_queue(home / "telemetry/queue")

    if keep:
        print(f"      scratch kept at {work}")
    else:
        shutil.rmtree(work, ignore_errors=True)
    return {
        "records": records,
        "exit": proc.returncode,
        "elapsed": elapsed,
        "timed_out": timed_out,
        "interrupted": interrupted,
        "stderr": (err or "")[-1500:],
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--old", default=str(Path.home() / ".local/bin/atomcode"))
    ap.add_argument("--new", default="target/release/atomcode")
    ap.add_argument("--only", action="append", help="run only these scenarios (repeatable)")
    ap.add_argument("--list", action="store_true", help="print the scenarios and why each exists")
    ap.add_argument("--keep", action="store_true", help="keep the scratch dirs")
    ap.add_argument("--raw", action="store_true", help="also print every record unnormalised")
    args = ap.parse_args()

    if args.list:
        for s in SCENARIOS:
            print(f"{s.name}\n    {s.why}\n")
        return 0

    chosen = SCENARIOS
    if args.only:
        names = {n for spec in args.only for n in spec.split(",")}
        unknown = names - {s.name for s in SCENARIOS}
        if unknown:
            print(f"no such scenario: {sorted(unknown)}", file=sys.stderr)
            return 2
        chosen = [s for s in SCENARIOS if s.name in names]

    for which, path in (("old", args.old), ("new", args.new)):
        if not Path(path).exists():
            print(f"no {which} binary at {path}", file=sys.stderr)
            return 2

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()

    versions = {}
    for which, path in (("old", args.old), ("new", args.new)):
        versions[which] = subprocess.run(
            [path, "--version"], capture_output=True, text=True
        ).stdout.strip()
    print(f"old  {args.old}  ({versions['old']})")
    print(f"new  {args.new}  ({versions['new']})")
    print(f"fake model + telemetry collector on 127.0.0.1:{port}\n")

    diverged, empty = [], []
    for scenario in chosen:
        print(f"-- {scenario.name}")
        runs = {}
        for which, path in (("old", args.old), ("new", args.new)):
            run = runs[which] = run_one(path, port, scenario, args.keep)
            ids = [r.get("event_id") for r in run["records"]]
            flags = ("  TIMEOUT" if run["timed_out"] else "") + (
                "  SIGINT" if run["interrupted"] else ""
            )
            print(
                f"   {which}: exit {run['exit']} in {run['elapsed']:.1f}s{flags}"
                f"  -> {len(ids)} event(s) {ids}"
            )
            if args.raw:
                for record in run["records"]:
                    print(f"      {json.dumps(record, sort_keys=True, ensure_ascii=False)}")

        old = summarise(runs["old"]["records"])
        new = summarise(runs["new"]["records"])
        if not old and not new:
            if scenario.expect_silence:
                print("   ok both silent, as this scenario expects")
            else:
                print("   ! neither build reported anything - nothing was compared")
                empty.append(scenario.name)
            continue
        if scenario.expect_silence:
            # The interesting direction: something started reporting where
            # nothing used to, which is a change in behaviour either way.
            print(f"   ! expected silence, got {len(old)} old / {len(new)} new")
            diverged.append(scenario.name)
        delta = list(difflib.unified_diff(old, new, "old", "new", lineterm="", n=0))
        if not delta:
            print(f"   ok identical ({len(old)} record(s))")
            continue
        diverged.append(scenario.name)
        print("   XX diverged:")
        for line in delta:
            print(f"     {line}")
    server.shutdown()

    print("\n-- summary")
    ran = [s.name for s in chosen]
    print(f"   {len(ran) - len(diverged) - len(empty)}/{len(ran)} identical")
    if empty:
        print(f"   {len(empty)} reported nothing on either build: {empty}")
        print("     (not parity - the scenario reached no metered path, or the harness is broken)")
    if diverged:
        print(f"   {len(diverged)} diverged: {diverged}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
