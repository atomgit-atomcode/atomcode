#!/usr/bin/env python3
"""Explicit GUI acceptance against the owned fixture only; requires the isolated App's OS grants."""
import argparse
import base64
import contextlib
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time

import verify_lease as lease
from verify_app_isolation import ProxyPeer, stop_app


def wait_state(path, predicate, seconds=8):
    began = time.monotonic()
    while True:
        if path.exists():
            state = json.loads(path.read_text())
            if predicate(state):
                return state
        assert time.monotonic() - began < seconds, "owned fixture did not reach the expected GUI state"
        time.sleep(0.01)


def tool(peer, owner, name, arguments, expect_error=False):
    response = peer.request("tools/call", dict(name=name, arguments=arguments, **lease.scope(owner)))
    result = response.get("result", {})
    assert "error" not in response and result.get("isError", False) is expect_error, name + ": " + str(response.get("error", result.get("isError")))
    return result


def capture(peer, owner, window):
    result = tool(peer, owner, "get_window_state", dict(window=window, include_screenshot=True, include_text=True))
    text = next(c["text"] for c in result["content"] if c.get("type") == "text")
    data = json.loads(text)
    image = next(c for c in result["content"] if c.get("type") == "image")
    png = base64.b64decode(image["data"], validate=True)
    assert png.startswith(b"\x89PNG\r\n\x1a\n") and image["mimeType"] == "image/png"
    assert data["screenshots"] and data["screenshots"][0]["id"]
    assert data["window"]["id"] == window["id"]
    return data, png


def element(tree, label, field=False):
    rows = [line.strip() for line in tree.splitlines() if label in line]
    if field:
        rows = [r for r in rows if "field" in r.lower() or "文本" in r or "acceptance-message" in r]
    else:
        rows = [r for r in rows if "button" in r.lower() or "按钮" in r]
    indices = [re.match(r"^(\d+)\s", row).group(1) for row in rows if re.match(r"^(\d+)\s", row)]
    assert len(indices) == 1, "target widget must be unambiguous in its fresh accessibility tree: " + label
    return indices[0]


def run(app, fixture, report, perform):
    if not perform:
        raise ValueError("Pass --perform-gui explicitly; this test opens and operates only its fixture window")
    app, fixture = app.resolve(strict=True), fixture.resolve(strict=True)
    binary = app / "Contents/MacOS/OpenComputerUse"
    env = dict(os.environ, OPEN_COMPUTER_USE_DISABLE_APP_AGENT_PROXY="0",
               OPEN_COMPUTER_USE_ALLOW_GLOBAL_POINTER_FALLBACKS="0",
               OPEN_COMPUTER_USE_MACOS_ALLOW_FOREGROUND_INPUT="0")
    # Use the actual per-user desktop lock, independently of product profiles.
    env.pop("OPEN_COMPUTER_USE_TURN_LEASE_DIRECTORY", None)
    try:
        diagnostic = subprocess.run([str(binary), "doctor", "--no-ui"], env=env,
                                    capture_output=True, text=True, timeout=12, check=True)
        diagnostic = json.loads(diagnostic.stdout)
        assert diagnostic["ui"] is False
        assert Path(diagnostic["runtime"]["permission_app"]).resolve() == app
        if diagnostic["permissions"] != {"accessibility": True, "screen_recording": True}:
            raise ValueError("This exact isolated App still needs Accessibility and Screen Recording; no fixture was launched")
        evidence = dict(app=str(app), fixture=str(fixture), checks=[], gui_actions=0)
        with tempfile.TemporaryDirectory(prefix="ac-gui-", dir="/tmp") as root:
            root = Path(root)
            state, gate = root / "state.json", root / "release"
            with contextlib.ExitStack() as cleanup:
                errors = cleanup.enter_context((root / "stderr").open("w"))
                target = subprocess.Popen([str(fixture / "Contents/MacOS/AtomCodeCUFixture"), "--state-file", str(state), "--release-gate", str(gate)],
                                          stdout=subprocess.DEVNULL, stderr=errors)
                def close_fixture():
                    gate.touch()  # unblock only this test's pending callback
                    if target.poll() is None:
                        target.terminate()
                        target.wait(timeout=5)
                cleanup.callback(close_fixture)
                ready = wait_state(state, lambda s: s["stage"] == "ready")
                assert ready["process_id"] == target.pid
                window = dict(id=ready["window_id"], app="dev.atomcode.computer-use-fixture", title="AtomCode CU Acceptance")
                a = ProxyPeer(binary, dict(env, ATOMCODE_HOME=str(root / "profile-a")), errors)
                cleanup.callback(a.close)
                lease.initialize(a)
                lease.successful(lease.acquire(a, "gui-turn-A"), "acquired")
                first, png = capture(a, "gui-turn-A", window)
                first_id = first["screenshots"][0]["id"]
                evidence["screenshot_sha256"] = hashlib.sha256(png).hexdigest()
                field = element(first["accessibility"]["tree"], "Acceptance message", field=True)
                tool(a, "gui-turn-A", "set_value", dict(window=first["window"], element_index=field, value="atomcode-native-acceptance"))
                current, _ = capture(a, "gui-turn-A", window)
                button = element(current["accessibility"]["tree"], "Record once")
                tool(a, "gui-turn-A", "click", dict(window=current["window"], element_index=button, click_method="accessibility", screenshotId=current["screenshots"][0]["id"]))
                recorded = wait_state(state, lambda s: s["submissions"] == 1)
                assert recorded["message"] == "atomcode-native-acceptance"
                evidence["checks"].append("real owned-window PNG/AX observation, background set_value and one actual button action")
                # A separately connected native process checks the OS lock while
                # the App's accepted GUI request is still in flight.
                direct = lease.Peer(binary, root / "direct-profile")
                cleanup.callback(direct.close)
                lease.initialize(direct)
                current, _ = capture(a, "gui-turn-A", window)
                gated = element(current["accessibility"]["tree"], "Gated record")
                a.counter += 1
                request = dict(jsonrpc="2.0", id=a.counter, method="tools/call",
                               params=dict(name="click", arguments=dict(window=current["window"], element_index=gated,
                                           click_method="accessibility", screenshotId=current["screenshots"][0]["id"]),
                                           **lease.scope("gui-turn-A")))
                a.process.stdin.write(json.dumps(request) + "\n")
                a.process.stdin.flush()
                wait_state(state, lambda s: s["stage"] == "entered-gated-action")
                assert a.lines.empty(), "accepted GUI request must still be awaiting its response"
                a.kill()
                lease.rejected(lease.acquire(direct, "gui-turn-B"), "busy")
                assert json.loads(state.read_text())["stage"] == "entered-gated-action"
                gate.touch()
                wait_state(state, lambda s: s["submissions"] == 2)
                lease.acquire_after_disconnect(direct, "gui-turn-B")
                lease.successful(lease.release(direct, "gui-turn-B"), "released")
                evidence["checks"].append("proxy SIGKILL during accepted GUI callback retains execution-side lease; actual action completes once and then hands over")
                c = ProxyPeer(binary, dict(env, ATOMCODE_HOME=str(root / "profile-c")), errors)
                cleanup.callback(c.close)
                lease.initialize(c)
                lease.successful(lease.acquire(c, "gui-turn-C"), "acquired")
                fresh, _ = capture(c, "gui-turn-C", window)
                assert fresh["screenshots"][0]["id"] != first_id, "fresh connection cannot reissue the prior connection's first screenshot identity"
                button = element(fresh["accessibility"]["tree"], "Record once")
                stale = tool(c, "gui-turn-C", "click", dict(window=fresh["window"], element_index=button,
                             click_method="accessibility", screenshotId=first_id), expect_error=True)
                assert "stale screenshot" in " ".join(x.get("text", "") for x in stale["content"]).lower()
                assert json.loads(state.read_text())["submissions"] == 2
                lease.successful(lease.release(c, "gui-turn-C"), "released")
                evidence["checks"].append("new connection requires fresh observation and rejects an old screenshot ID even after its own first observation")
                evidence["gui_actions"] = 3  # one value assignment, two button actions
                evidence["submissions"] = 2
        evidence["passed"] = True
        report.write_text(json.dumps(evidence, indent=2) + "\n")
        print("Passed real GUI acceptance against the owned fixture only")
    finally:
        stop_app(app)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--perform-gui", action="store_true")
    args = parser.parse_args()
    run(args.app, args.fixture, args.report, args.perform_gui)
