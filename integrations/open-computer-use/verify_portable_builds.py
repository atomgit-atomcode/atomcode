#!/usr/bin/env python3
"""Opt-in pinned cross-build measurements; never install or run a target backend."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import time

PIN = "e551c13e7082ce4c097234137582fbf4a1bc647a"
MODULES = {"linux": "apps/OpenComputerUseLinux", "windows": "apps/OpenComputerUseWindows"}


def git(source, *args):
    return subprocess.check_output(["git", "-C", str(source), *args])


def require_source(source):
    if git(source, "rev-parse", "HEAD").decode().strip() != PIN:
        raise ValueError("source must be the pinned upstream commit")
    if git(source, "status", "--porcelain", "--untracked-files=all"):
        raise ValueError("source must be clean; nothing was reset")


def snapshot(source):
    paths = git(source, "ls-files", "-z", "--", *MODULES.values(),
                "plugins/open-computer-use/.codex-plugin/plugin.json").decode().split("\0")
    measured = {}
    for name in filter(None, paths):
        path = source / name
        if path.is_symlink() or not path.is_file():
            raise ValueError("input must be a regular file: " + name)
        data = path.read_bytes()
        if data != git(source, "show", "HEAD:" + name):
            raise ValueError("input differs from pinned commit (including index-hidden edits): " + name)
        measured[name] = dict(bytes=len(data), sha256=hashlib.sha256(data).hexdigest())
    for module in MODULES.values():
        if module + "/go.mod" not in measured or module + "/go.sum" not in measured:
            raise ValueError("missing locked module inputs: " + module)
    return measured


def validate(source, output):
    source = source.resolve(strict=True)
    output = output.parent.resolve(strict=True) / output.name
    require_source(source)
    if source == output or source in output.parents:
        raise ValueError("output must be outside the source checkout")
    if os.path.lexists(output):
        raise ValueError("output exists; nothing was overwritten")
    return source, output


def run(source, output):
    source, output = validate(source, output)
    inputs = snapshot(source)
    verifier_digest = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    version = json.loads((source / "plugins/open-computer-use/.codex-plugin/plugin.json").read_text())["version"]
    env = dict(os.environ, GOTOOLCHAIN="local", GOWORK="off", GOFLAGS="-mod=readonly", CGO_ENABLED="0")
    toolchain = subprocess.check_output(["go", "version"], env=env, text=True).strip()
    output.mkdir(mode=0o700)
    report = dict(schema_version=1, upstream_commit=PIN, source=str(source), toolchain=toolchain,
                  backend_version=version, verifier_sha256=verifier_digest, source_inputs=inputs, host=dict(system=platform.system(),
                  machine=platform.machine()), gui_accepted=False, gui_actions=0,
                  target_binaries_executed=False, installed=False, status="running", steps=[], products=[])

    def save():
        (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")

    def step(name, args, cwd, target_env):
        print(name, flush=True)
        began = time.monotonic()
        log = output / (name + ".log")
        with log.open("wb") as stream:
            result = subprocess.run(args, cwd=cwd, env=target_env, stdin=subprocess.DEVNULL,
                                    stdout=stream, stderr=subprocess.STDOUT, timeout=360)
        report["steps"].append(dict(name=name, command=args, log=log.name,
                                    seconds=time.monotonic() - began, exit_code=result.returncode))
        save()
        if result.returncode:
            raise RuntimeError(name + " failed; see " + str(log))
        return log

    save()
    try:
        # On macOS the Linux desktop layer is explicitly stubbed, so these are
        # host-safe logic tests. Do not label them Linux desktop acceptance.
        if platform.system() == "Darwin":
            host_env = dict(env)
            host_env.pop("GOOS", None)
            host_env.pop("GOARCH", None)
            log = step("linux-logic-on-macos", ["go", "test", "-count=1", "-json", "./..."],
                       source / MODULES["linux"], host_env)
            events = []
            for line in log.read_text().splitlines():
                if line.startswith("{"):
                    events.append(json.loads(line))
            tests = [e for e in events if e.get("Test") and e.get("Action") in ("pass", "skip", "fail")]
            report["host_logic"] = dict(executed_on="macOS non-Linux stubs", passed=sum(e["Action"] == "pass" for e in tests),
                                        top_level_passed=sum(e["Action"] == "pass" and "/" not in e["Test"] for e in tests),
                                        skipped=sum(e["Action"] == "skip" for e in tests))
            if not report["host_logic"]["passed"]:
                raise RuntimeError("no host logic tests actually ran")
        else:
            report["host_logic"] = dict(executed=False, reason="this verifier only executes the reviewed macOS stub tests")
        for target, module in MODULES.items():
            for arch in ("amd64", "arm64"):
                label = target + "-" + arch
                target_env = dict(env, GOOS=target, GOARCH=arch)
                suffix = ".exe" if target == "windows" else ""
                binary = output / (label + suffix)
                step(label + "-build", ["go", "build", "-trimpath", "-ldflags",
                     "-s -w -X main.version=" + version, "-o", str(binary), "."], source / module, target_env)
                tests = output / (label + ".test" + suffix)
                step(label + "-test-compile", ["go", "test", "-c", "-o", str(tests), "."], source / module, target_env)
                graph = step(label + "-modules", ["go", "list", "-m", "-json", "all"], source / module, target_env)
                report["products"].append(dict(target=label, binary=binary.name, bytes=binary.stat().st_size,
                     sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                     test_binary=tests.name, test_binary_sha256=hashlib.sha256(tests.read_bytes()).hexdigest(),
                     module_graph=graph.name, compiled=True, tests_compiled=True, executed=False, gui_accepted=False))
        require_source(source)
        if hashlib.sha256(Path(__file__).read_bytes()).hexdigest() != verifier_digest:
            raise RuntimeError("verifier changed during verification")
        if snapshot(source) != inputs:
            raise RuntimeError("source input bytes changed during verification")
        report["status"] = "passed"
        save()
        print("Passed cross-build verification; target/GUI execution not performed", flush=True)
    except Exception as error:
        report["status"] = "failed"
        report["error"] = str(error)
        save()
        raise


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    run(args.source, args.output)
