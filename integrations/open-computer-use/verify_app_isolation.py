#!/usr/bin/env python3
"""Opt-in isolated App/proxy acceptance. Only diagnostics and lease RPCs, no GUI actions."""
import argparse
import contextlib
import errno
import hashlib
import json
import os
from pathlib import Path
import plistlib
import queue
import shutil
import socket
import subprocess
import tempfile
import threading
import time

import verify_lease as lease


def namespace(app):
    binary = app / "Contents/MacOS/OpenComputerUse"
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    identity = str(app.resolve()) + "\0" + digest
    name = hashlib.sha256(identity.encode()).hexdigest()[:32]
    return Path(f"/tmp/atomcode-ocu-agent-{os.getuid()}/{name}.sock"), digest


def info(path):
    peer = lease.AgentPeer(path)
    try:
        return peer.envelope(dict(kind="agentInfo"))
    finally:
        peer.close()


def stop_app(app):
    path, digest = namespace(app)
    if not path.exists():
        return
    peer = lease.AgentPeer(path)
    try:
        identity = peer.envelope(dict(kind="agentInfo"))
        assert Path(identity.get("bundleURL", "")).resolve() == app.resolve()
        assert identity.get("executableSHA256") == digest
        assert identity.get("socketPath") == str(path)
        peer.envelope(dict(kind="terminate"))
    finally:
        peer.close()
    began = time.monotonic()
    while path.exists():
        assert time.monotonic() - began < 5, "owned App did not remove its bound socket on shutdown"
        time.sleep(0.02)


class ProxyPeer(lease.Peer):
    def __init__(self, binary, env, stderr):
        self.process = subprocess.Popen([str(binary), "mcp"], env=env, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=stderr, text=True,
                                        encoding="utf-8", bufsize=1)
        self.lines = queue.Queue()
        self.counter = 0
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()


class Sentinel:
    def __init__(self, path, response):
        self.path = path
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.bind(str(path))
        path.chmod(0o600)
        self.identity = path.lstat().st_ino
        self.socket.listen(8)
        self.socket.settimeout(0.1)
        self.requests = []
        self.response = response
        self.stopped = threading.Event()
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        while not self.stopped.is_set():
            try:
                client, _ = self.socket.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            with client:
                client.settimeout(0.1)
                data = b""
                while not self.stopped.is_set():
                    try:
                        part = client.recv(4096)
                    except socket.timeout:
                        continue
                    except OSError:
                        break
                    if not part:
                        break
                    data += part
                    if b"\n" in data:
                        line, data = data.split(b"\n", 1)
                        self.requests.append(json.loads(line))
                        if self.response is not None:
                            client.sendall(json.dumps(self.response).encode() + b"\n")

    def intact(self):
        assert self.thread.is_alive() and self.path.lstat().st_ino == self.identity
        assert not any(r.get("kind") == "terminate" for r in self.requests), "proxy must never terminate a mismatching listener"

    def close(self):
        self.stopped.set()
        self.socket.close()
        self.thread.join(timeout=2)
        assert not self.thread.is_alive()
        if self.path.exists() and self.path.lstat().st_ino == self.identity:
            self.path.unlink()


def run(app, report):
    app = app.resolve(strict=True)
    binary = app / "Contents/MacOS/OpenComputerUse"
    metadata = plistlib.loads((app / "Contents/Info.plist").read_bytes())
    assert metadata.get("AtomCodeDesktopLeaseProtocol") == "1"
    subprocess.run(["/usr/bin/codesign", "--verify", "--deep", "--strict", str(app)], check=True)
    path, digest = namespace(app)
    assert not os.path.lexists(path), "use an idle, separately packaged test App; nothing was replaced"
    evidence = dict(app=str(app), executable_sha256=digest, gui_actions=0, checks=[])
    # Short private paths also fit the macOS Unix socket pathname limit.
    with tempfile.TemporaryDirectory(prefix="ac-app-", dir="/tmp") as root:
        root = Path(root)
        legacy_dir = root / "legacy"
        legacy_dir.mkdir(mode=0o700)
        base_env = dict(os.environ, OPEN_COMPUTER_USE_DISABLE_APP_AGENT_PROXY="0",
                        OPEN_COMPUTER_USE_TURN_LEASE_DIRECTORY=str(root / "turns"),
                        TMPDIR=str(legacy_dir) + "/")
        for flag in ["OPEN_COMPUTER_USE_ALLOW_GLOBAL_POINTER_FALLBACKS", "OPEN_COMPUTER_USE_MACOS_ALLOW_FOREGROUND_INPUT"]:
            base_env[flag] = "0"
        with contextlib.ExitStack() as cleanup:
            sentinel = Sentinel(legacy_dir / "open-computer-use-agent.sock", {"error": "sentinel must not receive a request"})
            cleanup.callback(sentinel.close)
            cleanup.callback(stop_app, app)
            errors = cleanup.enter_context((root / "proxy-stderr").open("w"))
            a = ProxyPeer(binary, dict(base_env, ATOMCODE_HOME=str(root / "profile-a")), errors)
            cleanup.callback(a.close)
            lease.initialize(a)
            first_info = info(path)
            assert Path(first_info["bundleURL"]).resolve() == app
            assert first_info["executableSHA256"] == digest
            assert first_info["socketPath"] == str(path)
            assert first_info["processId"] != a.process.pid
            doctor = subprocess.run([str(binary), "doctor", "--no-ui"], env=base_env,
                                    capture_output=True, text=True, timeout=10, check=True)
            assert not doctor.stderr, doctor.stderr
            diagnostic = json.loads(doctor.stdout)
            assert Path(diagnostic["runtime"]["app_bundle"]).resolve() == app
            assert Path(diagnostic["runtime"]["permission_app"]).resolve() == app
            assert diagnostic["runtime"]["process_id"] == first_info["processId"]
            assert diagnostic["ui"] is False
            evidence["permissions"] = diagnostic["permissions"]
            b = ProxyPeer(binary, dict(base_env, ATOMCODE_HOME=str(root / "different-profile-b")), errors)
            cleanup.callback(b.close)
            lease.initialize(b)
            assert info(path)["processId"] == first_info["processId"], "two proxies share only their exact isolated helper"
            lease.successful(lease.acquire(a, "A"), "acquired")
            lease.rejected(lease.acquire(b, "B"), "busy")
            lease.successful(lease.release(a, "A"), "released")
            lease.successful(lease.acquire(b, "B"), "acquired")
            b.finish()
            lease.acquire_after_disconnect(a, "C")
            assert info(path)["processId"] == first_info["processId"], "proxy EOF cannot terminate the shared helper"
            lease.successful(lease.release(a, "C"), "released")
            a.finish()
            evidence["checks"].append("LaunchServices proxy startup, exact helper identity, two profiles, scoped handover, EOF and neighbor/helper survival")
            sentinel.intact()
            assert not sentinel.requests, "the legacy default socket received no connection payload"

            # A copied signed bundle has a different namespace, without changing
            # the running helper or the installed App. It is never launched here.
            mismatch = root / "Mismatch.app"
            shutil.copytree(app, mismatch)
            cleanup.callback(stop_app, mismatch)
            wrong_path, _ = namespace(mismatch)
            assert wrong_path != path and not os.path.lexists(wrong_path)
            wrong = Sentinel(wrong_path, dict(bundleURL="wrong-app", processId=-1))
            try:
                result = subprocess.run([str(mismatch / "Contents/MacOS/OpenComputerUse"), "doctor", "--no-ui"],
                                        env=base_env, capture_output=True, text=True, timeout=8)
                assert result.returncode != 0 and "identity mismatch" in result.stderr.lower(), result.stderr
                wrong.intact()
                assert [r.get("kind") for r in wrong.requests] == ["agentInfo"]
            finally:
                wrong.close()
            evidence["checks"].append("different App namespace and mismatching helper refusal without terminate/unlink")

            # A wedged listener must fail within the identity budget, rather than
            # using the ordinary 120-second GUI response timeout.
            hung = Sentinel(wrong_path, None)
            try:
                began = time.monotonic()
                result = subprocess.run([str(mismatch / "Contents/MacOS/OpenComputerUse"), "doctor", "--no-ui"],
                                        env=base_env, capture_output=True, text=True, timeout=6)
                assert result.returncode != 0
                assert time.monotonic() - began < 4
                hung.intact()
            finally:
                hung.close()
            evidence["checks"].append("bounded identity timeout leaves a hung listener untouched")

            # A full connection backlog must not hang before the identity
            # deadline starts. The listener deliberately never accepts.
            blocked = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            blocked.bind(str(wrong_path))
            wrong_path.chmod(0o600)
            blocked.listen(1)
            blocked_inode = wrong_path.lstat().st_ino
            fillers = []
            try:
                for _ in range(16):
                    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    client.setblocking(False)
                    fillers.append(client)
                    code = client.connect_ex(str(wrong_path))
                    if code != 0:
                        break
                assert code in (errno.ECONNREFUSED, errno.EAGAIN, errno.EWOULDBLOCK) and len(fillers) > 1, "the controlled live backlog must actually be full"
                began = time.monotonic()
                result = subprocess.run([str(mismatch / "Contents/MacOS/OpenComputerUse"), "doctor", "--no-ui"],
                                        env=base_env, capture_output=True, text=True, timeout=14)
                elapsed = time.monotonic() - began
                assert result.returncode != 0, (result.returncode, result.stdout, result.stderr)
                assert elapsed < 13, (elapsed, result.stderr)
                assert wrong_path.lstat().st_ino == blocked_inode, "an occupied backlog must never be unlinked"
            finally:
                for client in fillers:
                    client.close()
                blocked.close()
                if wrong_path.exists() and wrong_path.lstat().st_ino == blocked_inode:
                    wrong_path.unlink()
            evidence["checks"].append("full Unix socket backlog is bounded without replacing the occupied listener")

            # Launch the actual listener twice on one test-owned socket. This
            # tests the lifetime lock in the App runtime, not only a unit mock.
            shared = root / "race.sock"
            helpers = []
            warm = subprocess.Popen([str(binary), "__open-computer-use-app-agent", str(shared)],
                                    stdout=subprocess.DEVNULL, stderr=errors)
            try:
                began = time.monotonic()
                while True:
                    try:
                        previous = info(shared)
                        break
                    except (OSError, AssertionError):
                        assert warm.poll() is None and time.monotonic() - began < 8
                        time.sleep(0.02)
                assert previous["processId"] == warm.pid
                warm.kill()
                assert warm.wait(timeout=5) == -9
                assert shared.exists(), "SIGKILL must leave a socket with a verifiable dead-owner record"
                for _ in range(2):
                    helpers.append(subprocess.Popen([str(binary), "__open-computer-use-app-agent", str(shared)],
                                                    stdout=subprocess.DEVNULL, stderr=errors))
                began = time.monotonic()
                while True:
                    try:
                        winner = info(shared)
                        if any(p.poll() is not None for p in helpers):
                            break
                    except (OSError, AssertionError):
                        pass
                    assert time.monotonic() - began < 8
                    time.sleep(0.02)
                assert winner["processId"] in [p.pid for p in helpers]
                assert sum(p.poll() is None for p in helpers) == 1
                winner_inode = shared.lstat().st_ino
                assert info(shared)["processId"] == winner["processId"]
                assert shared.lstat().st_ino == winner_inode
                peer = lease.AgentPeer(shared)
                try:
                    peer.envelope(dict(kind="terminate"))
                finally:
                    peer.close()
                for p in helpers:
                    p.wait(timeout=5)
                assert not shared.exists(), "normal listener cleanup removes its bound filesystem socket"
            finally:
                if warm.poll() is None:
                    warm.terminate()
                    warm.wait(timeout=5)
                for p in helpers:
                    if p.poll() is None:
                        p.terminate()
                        p.wait(timeout=5)
            evidence["checks"].append("real concurrent listener startup, verified dead-owner stale socket recovery, winner survival and owned inode cleanup")
            sentinel.intact()
            assert not sentinel.requests
            evidence["checks"].append("live legacy default-socket sentinel remained untouched throughout acceptance")
        proxy_errors = (root / "proxy-stderr").read_text()
        assert "data race detected" not in proxy_errors.lower(), proxy_errors
        evidence["checks"].append("cold launch and proxy diagnostics have no actor-isolation race warning")
    evidence["passed"] = True
    report.write_text(json.dumps(evidence, indent=2) + "\n")
    print(f"Passed {len(evidence['checks'])} isolated App acceptance groups; GUI actions=0")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    run(args.app, args.report)
