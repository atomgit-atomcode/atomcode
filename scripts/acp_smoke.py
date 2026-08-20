#!/usr/bin/env python3
"""ACP stdio smoke test: drives `atomcode acp` over stdin/stdout.

The protocol router pins ONE protocol version per connection (from the first
`initialize`), so v1 and v2 are exercised on two separate agent processes:

- Process A (v1): initialize v1 -> capabilities; session/new with an explicit
  cwd; session/list shows the live session; session/close removes it.
- Process B (v2): initialize v2 -> capabilities + info; session/new;
  session/list; session/resume on a nonexistent id fails with an explicit
  JSON-RPC error; session/close releases the live runtime but leaves the
  session in history (per protocol); session/delete removes it again.

Hermetic: each agent runs with a fresh empty ATOMCODE_HOME, so no user state
leaks in and failures surface as a clean JSON-RPC error or process exit
instead of a hang. The server may push notifications (e.g. `session/update`
right after `session/new`); the client skips them and waits for the matching
response, exactly like a real ACP client must.

Usage:
    python3 scripts/acp_smoke.py                  # uses target/debug/atomcode
    python3 scripts/acp_smoke.py path/to/atomcode # explicit binary
    python3 scripts/acp_smoke.py --help

Requires Python 3 (stdlib only). Exit code 0 on success, 1 on any failure.
"""
import json
import os
import subprocess
import sys
import tempfile


def usage():
    print(__doc__)
    return 0


class Agent:
    """One `atomcode acp` subprocess with a fresh, empty ATOMCODE_HOME."""

    def __init__(self, binary):
        self.tmp_home = tempfile.mkdtemp(prefix="acp-smoke-home-")
        env = dict(os.environ)
        env["ATOMCODE_HOME"] = self.tmp_home
        self.proc = subprocess.Popen(
            [binary, "acp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=env,
        )
        self._id = 0

    def request(self, method, params):
        """Send one request and return its matching response.

        Server-initiated notifications (no `id`) may arrive between a request
        and its response — e.g. `session/update` after `session/new`. Skip them
        and keep reading until the response with our `id` shows up.
        """
        self._id += 1
        obj = {"jsonrpc": "2.0", "id": self._id, "method": method, "params": params}
        assert self.proc.stdin is not None and self.proc.stdout is not None
        self.proc.stdin.write(json.dumps(obj) + "\n")
        self.proc.stdin.flush()
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError(
                    "agent closed stdout without a response (stderr tail: %s)"
                    % (self.proc.stderr.read() if self.proc.stderr else "")[-400:]
                )
            msg = json.loads(line)
            if "id" not in msg:
                print("  (notification) %s" % msg.get("method"))
                continue
            if msg.get("id") != self._id:
                continue  # stale/out-of-order response; keep waiting for ours
            return msg

    def close(self):
        if self.proc.stdin:
            self.proc.stdin.close()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        if self.proc.stderr:
            err = self.proc.stderr.read()
            if err.strip():
                print("(agent stderr):", err[-400:])


def smoke_v1(a: Agent):
    r = a.request("initialize", {"protocolVersion": 1})
    result = r["result"]
    assert result["protocolVersion"] == 1, r
    caps = result["agentCapabilities"]
    sess = caps.get("sessionCapabilities", {})
    assert sess.get("list") is not None, caps
    assert sess.get("close") is not None, caps
    assert sess.get("delete") is not None, caps
    assert caps["promptCapabilities"]["image"] is True, caps
    print("v1 initialize: ok")
    new = a.request("session/new", {"cwd": "/tmp", "mcpServers": []})
    assert "result" in new, f"session/new failed: {new}"
    sid = new["result"]["sessionId"]
    assert new["result"]["modes"]["currentModeId"] == "build", new
    print(f"v1 session/new: ok -> {sid} (modes advertised)")
    listed = a.request("session/list", {})
    assert len(listed["result"]["sessions"]) == 1, listed
    print("v1 session/list (1 live): ok")
    closed = a.request("session/close", {"sessionId": sid})
    assert "result" in closed, closed
    print("v1 session/close: ok")


def smoke_v2(a: Agent):
    r = a.request(
        "initialize",
        {"protocolVersion": 2, "info": {"name": "smoke", "version": "0"}},
    )
    result = r["result"]
    assert result["protocolVersion"] == 2, r
    assert isinstance(result.get("capabilities"), dict), r
    assert result.get("info", {}).get("name") == "atomcode", r
    print("v2 initialize: ok")
    new = a.request("session/new", {"cwd": "/tmp", "mcpServers": []})
    assert "result" in new, f"session/new failed: {new}"
    sid = new["result"]["sessionId"]
    print(f"v2 session/new: ok -> {sid}")
    listed = a.request("session/list", {})
    assert len(listed["result"]["sessions"]) == 1, listed
    print("v2 session/list (1 live): ok")
    resume = a.request("session/resume", {"sessionId": "acp-999", "cwd": "/tmp"})
    assert "error" in resume, f"expected explicit error for v2 resume: {resume}"
    print(f"v2 resume explicit error: ok -> {resume['error'].get('message')}")
    closed = a.request("session/close", {"sessionId": sid})
    assert "result" in closed, closed
    # Per the protocol, session/close only frees the live runtime: the session
    # stays in history and session/list still returns it, until session/delete
    # removes the entry.
    listed = a.request("session/list", {})
    assert len(listed["result"]["sessions"]) == 1, listed
    print("v2 session/close keeps history entry (per protocol): ok")
    deleted = a.request("session/delete", {"sessionId": sid})
    assert "result" in deleted, deleted
    listed = a.request("session/list", {})
    assert listed["result"]["sessions"] == [], listed
    print("v2 session/delete + list(0): ok")


def main(argv=None):
    if argv is None:
        argv = sys.argv[1:]
    if any(arg in ("-h", "--help") for arg in argv):
        return usage()
    if len(argv) > 1:
        print("usage: python3 scripts/acp_smoke.py [binary]", file=sys.stderr)
        return 2
    binary = argv[0] if argv else "target/debug/atomcode"
    if not os.path.isfile(binary):
        print(
            f"binary not found: {binary}\n"
            "build it first with `cargo build -p atomcode`, or pass the path:\n"
            "  python3 scripts/acp_smoke.py path/to/atomcode",
            file=sys.stderr,
        )
        return 2
    va = Agent(binary)
    try:
        smoke_v1(va)
    finally:
        va.close()
    vb = Agent(binary)
    try:
        smoke_v2(vb)
    finally:
        vb.close()
    print("SMOKE OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())