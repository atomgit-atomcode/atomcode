#!/usr/bin/env python3
"""Real isolated App/proxy scope refusal checks; no screenshots or GUI input."""
import argparse
import contextlib
import json
import os
from pathlib import Path
import tempfile

import verify_lease as lease
from verify_app_isolation import ProxyPeer, namespace, stop_app

KEY = "OPEN_COMPUTER_USE_ALLOWED_BUNDLE_IDS"


def initialize(peer):
    response = peer.request("initialize", dict(protocolVersion="2025-03-26", capabilities={},
                            clientInfo=dict(name="atomcode-app-scope-acceptance", version="1")))
    version = response.get("result", {}).get("capabilities", {}).get("experimental", {}).get("atomcode/application-scope", {}).get("version")
    assert type(version) is int and version == 1, "backend does not advertise application-scope v1"


def call(peer, name, arguments=None):
    response = peer.request("tools/call", dict(name=name, arguments=arguments or {}))
    assert "error" not in response, response
    result = response["result"]
    text = "\n".join(c.get("text", "") for c in result.get("content", []) if c.get("type") == "text")
    assert not any(c.get("type") == "image" for c in result.get("content", [])), "refusal tests must never obtain a screenshot"
    return result.get("isError", False), text


def run(app, report):
    app = app.resolve(strict=True)
    path, digest = namespace(app)
    assert not path.exists(), "use an idle separately packaged test App; nothing was replaced"
    binary = app / "Contents/MacOS/OpenComputerUse"
    env = dict(os.environ, OPEN_COMPUTER_USE_DISABLE_APP_AGENT_PROXY="0",
               OPEN_COMPUTER_USE_ALLOW_GLOBAL_POINTER_FALLBACKS="0",
               OPEN_COMPUTER_USE_MACOS_ALLOW_FOREGROUND_INPUT="0")
    env.pop(KEY, None)
    evidence = dict(app=str(app), executable_sha256=digest, gui_actions=0, screenshots=0, checks=[])
    with tempfile.TemporaryDirectory(prefix="ac-app-scope-") as temporary:
        root = Path(temporary)
        env["OPEN_COMPUTER_USE_TURN_LEASE_DIRECTORY"] = str(root / "turns")
        with contextlib.ExitStack() as cleanup:
            cleanup.callback(stop_app, app)
            errors = cleanup.enter_context((root / "stderr").open("w"))
            # Bootstrap without a policy so the helper's baseline environment
            # cannot itself be contaminated by the first client's override.
            baseline = ProxyPeer(binary, env, errors)
            cleanup.callback(baseline.close)
            initialize(baseline)
            bad = ProxyPeer(binary, dict(env, **{KEY: "not-json"}), errors)
            cleanup.callback(bad.close)
            initialize(bad)
            tools = bad.request("tools/list")["result"]["tools"]
            expected = {"list_apps", "list_windows", "get_window", "get_window_state", "activate_window",
                        "launch_app", "get_app_state", "click", "perform_secondary_action", "scroll",
                        "drag", "type_text", "press_key", "set_value", "capture_display"}
            assert {tool["name"] for tool in tools} == expected
            evidence["tool_count"] = len(tools)
            for tool in tools:
                failed, text = call(bad, tool["name"], {KEY: "[]", "policy": "allow"})
                assert failed and "Invalid application scope" in text, tool["name"] + ": " + text
            evidence["checks"].append("malformed trusted policy refuses all 15 actual tools before argument dispatch")
            empty = ProxyPeer(binary, dict(env, **{KEY: "[]"}), errors)
            cleanup.callback(empty.close)
            initialize(empty)
            for name, expected in [("list_apps", ""), ("list_windows", "[]")]:
                failed, text = call(empty, name)
                assert not failed and text == expected, (name, failed, text)
            evidence["checks"].append("explicit deny-all produces empty app/window catalogs through the real proxy")
            for name, arguments in [
                ("get_app_state", dict(app="com.example.atomcode.absent", policy="allow")),
                ("type_text", dict(app="com.example.atomcode.absent", text="never-dispatched",
                                  **{KEY: '["com.example.atomcode.absent"]'})),
            ]:
                failed, text = call(empty, name, arguments)
                assert failed and "Application scope denied the resolved target" in text, (name, failed, text)
                assert "com.example.atomcode.absent" not in text
            evidence["checks"].append("model-like policy arguments cannot authorize observation or input for an unallowed bundle")
            failed, text = call(bad, "list_apps")
            assert failed and "Invalid application scope" in text
            failed, text = call(baseline, "__atomcode_application_scope_probe__")
            assert failed and "Invalid application scope" not in text and "unsupportedTool" in text, text
            evidence["checks"].append("malformed/empty/unconfigured proxy scopes remain isolated in the same shared App")
            lease.initialize(empty, scoped=True)
            lease.successful(lease.acquire(empty, "app-scope-owner"), "acquired")
            response = empty.request("tools/call", dict(name="capture_display", arguments={},
                                                       **lease.scope("app-scope-owner")))
            result = response.get("result", {})
            assert result.get("isError") is True
            assert "Application scope forbids full-display capture" in str(result), response
            assert not any(c.get("type") == "image" for c in result.get("content", []))
            lease.successful(lease.release(empty, "app-scope-owner"), "released")
            evidence["checks"].append("valid desktop lease ownership cannot override application scope or authorize full-display capture")
        assert "data race detected" not in (root / "stderr").read_text().lower()
    evidence["passed"] = True
    report.write_text(json.dumps(evidence, indent=2) + "\n")
    print("Passed 5 real App application-scope refusal groups; no screenshots or GUI input")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    run(args.app, args.report)
