#!/usr/bin/env python3
"""Opt-in lease protocol acceptance against a separately built native backend.

Only handshake, lease control, ping and nonexistent-tool requests are sent.
No application contents, screenshot or GUI action are requested.
"""
import argparse
import contextlib
import json
import os
from pathlib import Path
import queue
import socket
import subprocess
import sys
import tempfile
import threading
import time


class Peer:
    def __init__(self, backend, profile, directory=None):
        # Protocol-only direct-process cases must not resolve an installed App
        # or attach to its shared default socket. No GUI tool is requested.
        env = dict(os.environ, ATOMCODE_HOME=str(profile), OPEN_COMPUTER_USE_DISABLE_APP_AGENT_PROXY="1")
        if directory is not None:
            env["OPEN_COMPUTER_USE_TURN_LEASE_DIRECTORY"] = str(directory)
        else:
            env.pop("OPEN_COMPUTER_USE_TURN_LEASE_DIRECTORY", None)
        self.process = subprocess.Popen(
            [str(backend), "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, encoding="utf-8", bufsize=1, env=env,
        )
        self.lines = queue.Queue()
        self.counter = 0
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _read(self):
        for line in self.process.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def request(self, method, params=None):
        self.counter += 1
        request = dict(jsonrpc="2.0", id=self.counter, method=method)
        if params is not None:
            request["params"] = params
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()
        try:
            line = self.lines.get(timeout=6)
        except queue.Empty as error:
            raise AssertionError("native MCP did not return within its test budget") from error
        assert line is not None, "native MCP exited before responding"
        response = json.loads(line)
        assert response.get("id") == self.counter, "native MCP response identity mismatch"
        return response

    def notify(self, method, params=None):
        self.process.stdin.write(json.dumps(dict(jsonrpc="2.0", method=method, params=params or {})) + "\n")
        self.process.stdin.flush()
        assert "result" in self.request("ping"), "notification must not create an extra response"

    def close(self):
        if self.process.poll() is None:
            self.process.stdin.close()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.reader.join(timeout=1)
        self.process.stdout.close()

    def finish(self):
        self.process.stdin.close()
        try:
            code = self.process.wait(timeout=5)
        except subprocess.TimeoutExpired as error:
            raise AssertionError("normal EOF did not stop the native MCP; cleanup must not count a forced kill as graceful exit") from error
        assert code == 0, "normal native MCP EOF must exit successfully"
        self.reader.join(timeout=1)
        self.process.stdout.close()

    def kill(self):
        self.process.kill()
        assert self.process.wait(timeout=5) == -9, "forced process-death acceptance must actually deliver SIGKILL"


class AgentPeer:
    def __init__(self, path):
        self.connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.connection.settimeout(6)
        self.connection.connect(str(path))
        self.file = self.connection.makefile("rwb", buffering=0)
        self.counter = 0

    def envelope(self, payload):
        self.file.write(json.dumps(payload).encode() + b"\n")
        line = self.file.readline(1024 * 1024)
        assert line, "private App agent closed the test connection"
        return json.loads(line)

    def request(self, method, params=None):
        self.counter += 1
        request = dict(jsonrpc="2.0", id=self.counter, method=method)
        if params is not None:
            request["params"] = params
        response = self.envelope(dict(kind="mcp", line=json.dumps(request), environment={}))
        assert isinstance(response.get("response"), str), "missing private App agent MCP response"
        response = json.loads(response["response"])
        assert response.get("id") == self.counter
        return response

    def close(self):
        self.file.close()
        self.connection.close()


def initialize(peer, scoped=True):
    capabilities = {"experimental": {"atomcode/turn-scopes": {"version": 1}}} if scoped else {}
    response = peer.request("initialize", dict(protocolVersion="2025-03-26", capabilities=capabilities,
                                             clientInfo=dict(name="atomcode-lease-acceptance", version="1")))
    assert response["result"]["capabilities"]["experimental"]["atomcode/desktop-turn-lease"]["version"] == 1


def scope(owner):
    return {"_meta": {"atomcode/turn": owner}}


def acquire(peer, owner):
    return peer.request("atomcode/desktop/acquire", scope(owner))


def release(peer, owner):
    return peer.request("atomcode/desktop/release", scope(owner))


def successful(response, key):
    assert response.get("result", {}).get(key) is True, f"lease operation did not acknowledge {key}: {response}"


def rejected(response, fragment):
    result = response.get("result", {})
    if "error" in response:
        text = response["error"].get("message", "")
    else:
        assert result.get("isError") is True, f"expected an explicit failure: {response}"
        text = " ".join(block.get("text", "") for block in result.get("content", []))
    assert fragment.lower() in text.lower(), f"expected diagnostic {fragment!r}: {text}"


def acquire_after_disconnect(peer, owner):
    began = time.monotonic()
    while True:
        response = acquire(peer, owner)
        if response.get("result", {}).get("acquired") is True:
            return
        rejected(response, "busy")
        assert time.monotonic() - began < 3, "disconnected backend connection retained its lease"
        time.sleep(0.01)


def run(backend, report):
    evidence = {"platform": "macos", "backend": str(backend), "gui_actions": 0,
                "checks": [], "scope": "native MCP and private App-agent protocol; no GUI acceptance"}
    with tempfile.TemporaryDirectory(prefix="atomcode-desktop-lease-") as scratch:
        scratch = Path(scratch)
        with contextlib.ExitStack() as cleanup:
            def peer(name, directory=None):
                instance = Peer(backend, scratch / name, directory)
                cleanup.callback(instance.close)
                initialize(instance)
                return instance

            a, b = peer("profile-a"), peer("different-profile-b")
            successful(acquire(a, "turn-A"), "acquired")
            successful(acquire(a, "turn-A"), "acquired")
            initialize(a, scoped=False)  # a second initialize cannot disable ownership
            rejected(a.request("tools/call", dict(name="__nonexistent_test_tool__", arguments={})), "Missing host-generated")
            rejected(acquire(a, "different-turn"), "busy")
            rejected(acquire(b, "turn-A"), "busy")
            assert release(b, "turn-A")["result"]["released"] is False
            rejected(release(a, "stale-turn"), "busy")
            rejected(acquire(b, "turn-B"), "busy")
            a.notify("notifications/turn-ended")
            rejected(acquire(b, "turn-B"), "busy")
            rejected(a.request("tools/call", dict(name="__nonexistent_test_tool__", arguments=scope("turn-A"))),
                     "Missing host-generated")
            rejected(a.request("tools/call", dict(name="__nonexistent_test_tool__", arguments={}, **scope("turn-B"))),
                     "does not own")
            successful(release(a, "turn-A"), "released")
            successful(acquire(b, "turn-B"), "acquired")
            rejected(a.request("tools/call", dict(name="__nonexistent_test_tool__", arguments={}, **scope("turn-A"))),
                     "does not own")
            successful(release(b, "turn-B"), "released")
            evidence["checks"].append("two processes / different product homes / reentrancy / wrong-owner release / addressed metadata")

            successful(acquire(a, "graceful"), "acquired")
            a.finish()
            acquire_after_disconnect(b, "after-graceful")
            successful(release(b, "after-graceful"), "released")
            c = peer("killed-client")
            successful(acquire(c, "killed"), "acquired")
            c.kill()
            acquire_after_disconnect(b, "after-kill")
            successful(release(b, "after-kill"), "released")
            evidence["checks"].append("normal EOF and direct native process death release OS ownership")

            actual = scratch / "private-target"
            actual.mkdir(mode=0o700)
            link = scratch / "symlink-directory"
            link.symlink_to(actual, target_is_directory=True)
            unsafe = peer("unsafe-path", link)
            rejected(acquire(unsafe, "unsafe"), "not a symlink")
            assert not (actual / "desktop.lock").exists()
            rejected(acquire(b, ""), "nonempty")
            rejected(acquire(b, "x" * 513), "bounded")
            rejected(acquire(b, "界" * 171), "bounded")
            successful(acquire(b, "x" * 512), "acquired")
            successful(release(b, "x" * 512), "released")
            bad_file = scratch / "private-file-dir"
            bad_file.mkdir(mode=0o700)
            outside = scratch / "untouched"
            outside.write_text("untouched", encoding="utf-8")
            outside.chmod(0o600)
            (bad_file / "desktop.lock").symlink_to(outside)
            unsafe_file = peer("unsafe-file", bad_file)
            rejected(acquire(unsafe_file, "unsafe"), "Cannot open")
            assert outside.read_text(encoding="utf-8") == "untouched"
            hard_dir = scratch / "hardlink-dir"
            hard_dir.mkdir(mode=0o700)
            os.link(outside, hard_dir / "desktop.lock")
            unsafe_hardlink = peer("unsafe-hardlink", hard_dir)
            rejected(acquire(unsafe_hardlink, "unsafe"), "private regular file")
            public_dir = scratch / "public-directory"
            public_dir.mkdir(mode=0o755)
            unsafe_mode = peer("unsafe-mode", public_dir)
            rejected(acquire(unsafe_mode, "unsafe"), "must be private")
            assert outside.read_text(encoding="utf-8") == "untouched"
            evidence["checks"].append("symlink/hardlink/mode defenses, malformed scope and UTF-8 byte limits fail closed")

            unsupported = Peer(backend, scratch / "unsupported")
            cleanup.callback(unsupported.close)
            for version in [2, True, "1"]:
                response = unsupported.request("initialize", {"capabilities": {"experimental": {"atomcode/turn-scopes": {"version": version}}}})
                assert response.get("error", {}).get("code") == -32602, "unsupported scope protocol must not silently downgrade"
            initialize(unsupported, scoped=False)
            rejected(acquire(unsupported, "legacy"), "not negotiated")
            rejected(unsupported.request("tools/call", dict(name="__nonexistent_test_tool__", arguments={})), "unsupportedTool")
            evidence["checks"].append("strict capability version and explicit legacy-client behavior")

            # Exercise the same AppAgentConnection boundary used by the real
            # macOS proxy, through a unique private socket owned by this test.
            private_socket = scratch / "private-agent.sock"
            agent = subprocess.Popen([str(backend), "__open-computer-use-app-agent", str(private_socket)],
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                began = time.monotonic()
                while not private_socket.exists():
                    assert agent.poll() is None, "private App agent exited during startup"
                    assert time.monotonic() - began < 10, "private App agent did not create its private socket"
                    time.sleep(0.02)
                with contextlib.ExitStack() as clients:
                    first, second = AgentPeer(private_socket), AgentPeer(private_socket)
                    clients.callback(first.close)
                    clients.callback(second.close)
                    initialize(first)
                    initialize(second)
                    successful(acquire(first, "shared-agent-A"), "acquired")
                    rejected(acquire(second, "shared-agent-B"), "busy")
                    rejected(acquire(b, "direct-process-B"), "busy")
                    assert release(second, "shared-agent-A")["result"]["released"] is False
                    first.close()
                    acquire_after_disconnect(b, "after-agent-disconnect")
                    assert agent.poll() is None, "disconnect must not terminate the shared App agent"
                    rejected(acquire(second, "shared-agent-B"), "busy")
                    successful(release(b, "after-agent-disconnect"), "released")
                    successful(acquire(second, "shared-agent-B"), "acquired")
                    successful(release(second, "shared-agent-B"), "released")
                evidence["checks"].append("private shared App: per-connection ownership, cross-process exclusion, independent disconnect and surviving neighbor")
            finally:
                if agent.poll() is None:
                    try:
                        terminator = AgentPeer(private_socket)
                        try:
                            terminator.envelope(dict(kind="terminate"))
                        finally:
                            terminator.close()
                    except (OSError, AssertionError):
                        agent.terminate()
                    try:
                        agent.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        agent.kill()
                        agent.wait(timeout=5)

    evidence["passed"] = True
    if report:
        report.write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
    print(f"Passed {len(evidence['checks'])} desktop lease acceptance groups; no GUI actions requested.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backend", type=Path, required=True)
    parser.add_argument("--report", type=Path)
    arguments = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("this backend lease acceptance harness currently targets macOS only")
    run(arguments.backend.resolve(strict=True), arguments.report)
