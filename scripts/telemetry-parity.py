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
from dataclasses import dataclass, field, replace
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
    # The same session, spelled for the OLD build when a tool it is scripted to
    # call takes different arguments there. What is being compared is the
    # telemetry, not the tool schemas, so an unrelated schema change is
    # normalised like a uuid is — otherwise the diff reports "the old build
    # never metered this" when what actually happened is that the old build
    # rejected the arguments and never ran the tool at all. (It did. That is how
    # this field came to exist.)
    script_old: list = None
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
        name="subagent",
        why=(
            "a `task` child, which runs its OWN kernel loop with no telemetry "
            "hooks of its own. Its rounds are metered by a provider DECORATOR "
            "instead (`coding/src/parts.rs` fills the subagent slot with a "
            "`MeteredProvider` tagged `surface=\"subagent\"`), so what the "
            "child reports — and whether its tool calls report at all — comes "
            "from a different mechanism than the parent's. The fake model "
            "answers the parent and the child alike, in order: parent asks for "
            "a task, child answers, parent wraps up."
        ),
        script=[
            call("task", {"task": "say hello and stop"}),
            text("child done"),
            text("done"),
        ],
        # 5.1.0's `task` takes a BATCH (`tasks: [{description, prompt, …}]`);
        # HEAD's takes one `task` string. Read off each build's own tool
        # definitions rather than guessed.
        script_old=[
            call(
                "task",
                {
                    "tasks": [
                        {
                            "description": "say hello",
                            "prompt": "say hello and stop",
                            # `explore`, not `worker`: 5.1.0 refuses a worker
                            # that declares no `scope` (its writable lane).
                            "subagent_type": "explore",
                        }
                    ]
                },
            ),
            text("child done"),
            text("done"),
        ],
        args=["-y"],
        timeout_s=120,
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
    requests = []
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

        body = self._read_body()
        try:
            Handler.requests.append(json.loads(body))
        except (json.JSONDecodeError, UnicodeDecodeError):
            pass
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
    # The per-zone breakdown, and the message count it is computed over. These
    # CANNOT match and it is not a defect: the two builds ship different system
    # prompts (36895B against 35671B here), the zones are byte/4 estimates
    # scaled to the real prompt total, and `messages_count` counts the internal
    # list rather than the wire — where the two builds agree exactly, as
    # `--dump-requests` shows. Bucketed rather than dropped, so a zone that
    # stops being reported at all is still a diff.
    "system_tokens",
    "tool_def_tokens",
    "tool_result_tokens",
    "message_tokens",
    "messages_count",
}

# NOT volatile, and compared exactly. Every one of these is decided by the fake
# model's `usage` block or by the config, so the two builds must agree to the
# token — these are the numbers a bill is computed from. They were bucketed with
# the rest until someone asked to see them, which is how a harness ends up
# reporting "identical" about fields it never compared.
#   input/output/cached — straight from the provider's usage report
#   context_window      — straight from the config
#   tool_calls_count    — the length of what the model was scripted to call


# Keys the NEW build is expected to carry and the old one cannot: dropped from
# the comparison, because their whole point is that they did not exist before.
# Listed rather than ignored silently, so this stays a short, reviewable list
# and not a place divergence can hide.
#   turn / round / request — the correlation chain below `session_id`, added
#   deliberately (the envelope's `turn_id` was declared and never once set).
ADDED_SINCE_OLD = {"turn", "round", "request"}


def shape(record):
    """What must be identical: which event, and every non-volatile field."""
    out = {}
    for key, value in sorted(record.items()):
        if key in VOLATILE_ENVELOPE or key in ADDED_SINCE_OLD:
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


def new_only(record):
    """Records the new build reports and the old one never did.

    Measured, not assumed: with a `task` child that both builds actually run
    (same 4 requests to the model, request 1 being the child's), 5.1.0 emits 2
    `llm_chat` and HEAD emits 3. The child runs its own kernel loop with no
    telemetry hooks, so its rounds are metered by a provider decorator
    (`coding/src/parts.rs`, `MeteredProvider` tagged `surface="subagent"`) —
    which 5.1.0 did not have wired. The child's token spend was invisible.

    The same for a rate-limited turn. `turn_complete` used to report only
    `ProviderError` and `Timeout`; a 429 pause fell into `_ => return`, so the
    whole turn produced `open_atomcode` and nothing else — a person who waited
    and gave up was indistinguishable from a session where nothing happened.
    Measured here: old 1 record, new 2.

    Listed one by one, each against a measurement. A predicate that guessed
    would be a place for divergence to hide.
    """
    if record.get("surface") == "subagent":
        return True
    return record.get("event_id") == "llm_chat" and record.get("error_kind") == "rate_limited"


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


def run_one(binary, port, scenario, keep, which="new"):
    # The old build gets its own spelling of the session when one was given.
    Handler.scenario = (
        replace(scenario, script=scenario.script_old)
        if which == "old" and scenario.script_old
        else scenario
    )
    Handler.collected.clear()
    Handler.requests.clear()
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
        requests = list(Handler.requests)
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
        "requests": requests,
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
    ap.add_argument(
        "--dump-requests",
        action="store_true",
        help="print the role and size of every message each build sent the model — "
        "what the per-zone token breakdown in `llm_chat` is computed from",
    )
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
            run = runs[which] = run_one(path, port, scenario, args.keep, which)
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
            if args.dump_requests:
                for n, body in enumerate(run["requests"]):
                    roles = [
                        f"{m.get('role')}({len(json.dumps(m, ensure_ascii=False))}B"
                        + (",calls" if m.get("tool_calls") else "")
                        + ")"
                        for m in body.get("messages", [])
                    ]
                    print(f"      request {n}: {len(roles)} message(s)  {' '.join(roles)}")

        old = summarise(runs["old"]["records"])
        extra = [r for r in runs["new"]["records"] if new_only(r)]
        new = summarise([r for r in runs["new"]["records"] if not new_only(r)])
        if extra:
            # Said out loud rather than dropped quietly: "the new build reports
            # more" is a result, and a silent exemption is how it stops being one.
            kinds = sorted(
                {
                    f"{r.get('event_id')}/{r.get('surface') or r.get('error_kind')}"
                    for r in extra
                }
            )
            print(f"   +  {len(extra)} record(s) only the new build reports: {kinds}")
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
