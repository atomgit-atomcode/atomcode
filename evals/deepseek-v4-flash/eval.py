#!/usr/bin/env python3
"""Paired, auditable AtomCode model evaluation harness (stdlib only)."""

from __future__ import annotations

import argparse
import asyncio
import concurrent.futures
import hashlib
import json
import math
import os
import random
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import uuid
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent
SECRET_RE = re.compile(
    r"(?i)(authorization\s*[:=]\s*(?:bearer\s+)?|api[_-]?key\s*[:=]\s*|token\s*[:=]\s*)([^\s\"']+)"
)
TOKEN_RE = re.compile(r"\[tokens\]\s+prompt=(\d+)\s+completion=(\d+)\s+cached=(\d+)")


def atomic_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_suffix(path.suffix + ".tmp")
    temp.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")
    temp.replace(path)


def scrub(text: str) -> str:
    return SECRET_RE.sub(lambda m: m.group(1) + "[REDACTED]", text)


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


@dataclass(frozen=True)
class Candidate:
    name: str
    selection: str
    expected_model: str


@dataclass(frozen=True)
class Case:
    id: str
    tier: str
    directory: Path
    prompt: str
    timeout: int
    allow_edits: bool
    fixture: Path | None
    verify: tuple[str, ...]
    rubric: dict[str, str]


@dataclass(frozen=True)
class Suite:
    path: Path
    name: str
    repetitions: int
    pair_concurrency: int
    timeout: int
    start_skew_ms: int
    seed: int
    atomcode_bin: str
    codex_bin: str
    codex_model: str | None
    config: Path
    candidates: tuple[Candidate, Candidate]


def _positive(value: Any, label: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 1:
        raise ValueError(f"{label} must be a positive integer")
    return value


def load_suite(path: Path) -> Suite:
    raw = json.loads(path.read_text())
    candidates = raw.get("candidates", {})
    if set(candidates) != {"official", "volcengine"}:
        raise ValueError("candidates must contain exactly official and volcengine")
    parsed = tuple(
        Candidate(name, str(candidates[name]["selection"]), str(candidates[name]["expected_model"]))
        for name in ("official", "volcengine")
    )
    if len({c.selection for c in parsed}) != 2:
        raise ValueError("candidate selections must differ")
    return Suite(
        path=path.resolve(), name=str(raw["suite"]),
        repetitions=_positive(raw.get("repetitions", 3), "repetitions"),
        pair_concurrency=_positive(raw.get("pair_concurrency", 2), "pair_concurrency"),
        timeout=_positive(raw.get("timeout_seconds", 900), "timeout_seconds"),
        start_skew_ms=_positive(raw.get("start_skew_ms", 500), "start_skew_ms"),
        seed=int(raw.get("random_seed", 0)), atomcode_bin=str(raw.get("atomcode_bin", "atomcode")),
        codex_bin=str(raw.get("codex_bin", "codex")),
        codex_model=str(raw.get("codex_model") or "") or None,
        config=Path(os.path.expanduser(str(raw.get("config", "~/.atomcode/config.toml")))).resolve(),
        candidates=parsed,  # type: ignore[arg-type]
    )


def discover_cases(root: Path) -> list[Case]:
    result: list[Case] = []
    for cfg in sorted(root.glob("*/case.json")):
        raw = json.loads(cfg.read_text())
        case_id = str(raw.get("id", ""))
        if not re.fullmatch(r"[a-z0-9][a-z0-9_-]*", case_id) or case_id != cfg.parent.name:
            raise ValueError(f"invalid or mismatched case id at {cfg}")
        prompt_path = cfg.parent / str(raw.get("prompt", "prompt.md"))
        if not prompt_path.is_file() or prompt_path.parent.resolve() != cfg.parent.resolve():
            raise ValueError(f"prompt must be a file directly inside {cfg.parent}")
        fixture_raw = raw.get("fixture")
        fixture = (cfg.parent / str(fixture_raw)).resolve() if fixture_raw else None
        if fixture and (not fixture.exists() or cfg.parent.resolve() not in fixture.parents):
            raise ValueError(f"fixture must remain inside {cfg.parent}")
        verify = raw.get("verify", [])
        if not isinstance(verify, list) or not all(isinstance(x, str) for x in verify):
            raise ValueError(f"verify must be an argument array in {cfg}")
        rubric = raw.get("rubric", {})
        if not isinstance(rubric, dict) or not all(isinstance(v, str) for v in rubric.values()):
            raise ValueError(f"rubric must be a string table in {cfg}")
        result.append(Case(case_id, str(raw.get("tier", "model")), cfg.parent, prompt_path.read_text(),
                           _positive(raw.get("timeout_seconds", 900), "case timeout"),
                           bool(raw.get("allow_edits", False)), fixture, tuple(verify), rubric))
    catalog = root / "model-cases.json"
    if catalog.is_file():
        values = json.loads(catalog.read_text())
        if not isinstance(values, list): raise ValueError("model-cases.json must be an array")
        for raw in values:
            case_id = str(raw.get("id", ""))
            if not re.fullmatch(r"[a-z0-9][a-z0-9_-]*", case_id): raise ValueError("invalid catalog case id")
            rubric = raw.get("rubric", {})
            result.append(Case(case_id, "model", root, str(raw["prompt"]),
                               _positive(raw.get("timeout_seconds", 300), "case timeout"),
                               False, None, (), rubric))
    agent_catalog = root / "agent-cases.json"
    if agent_catalog.is_file():
        values = json.loads(agent_catalog.read_text())
        if not isinstance(values, list): raise ValueError("agent-cases.json must be an array")
        for raw in values:
            case_id = str(raw.get("id", "")); fixture = (root/str(raw["fixture"])).resolve()
            if not re.fullmatch(r"[a-z0-9][a-z0-9_-]*", case_id): raise ValueError("invalid agent case id")
            if not fixture.is_dir() or root.resolve() not in fixture.parents: raise ValueError("invalid agent fixture")
            verify = raw.get("verify", [])
            if not isinstance(verify, list) or not all(isinstance(x, str) for x in verify): raise ValueError("invalid agent verifier")
            result.append(Case(case_id, "agent", root, str(raw["prompt"]),
                               _positive(raw.get("timeout_seconds", 900), "case timeout"),
                               True, fixture, tuple(verify), raw.get("rubric", {})))
    ids = [case.id for case in result]
    if len(ids) != len(set(ids)): raise ValueError("duplicate case id")
    if not result:
        raise ValueError(f"no cases found under {root}")
    return result


def prepare(suite: Suite, cases: list[Case], results_root: Path, selected: set[str] | None,
            repetitions: int | None) -> Path:
    chosen = [c for c in cases if selected is None or c.id in selected]
    missing = (selected or set()) - {c.id for c in chosen}
    if missing:
        raise ValueError(f"unknown cases: {', '.join(sorted(missing))}")
    reps = repetitions or suite.repetitions
    run_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + uuid.uuid4().hex[:8]
    run_dir = results_root / run_id
    (run_dir / "raw").mkdir(parents=True)
    rng = random.Random(suite.seed)
    schedule = []
    for case in chosen:
        for rep in range(1, reps + 1):
            order = [c.name for c in suite.candidates]
            rng.shuffle(order)
            schedule.append({"case": case.id, "repetition": rep, "launch_order": order})
    rng.shuffle(schedule)
    manifest = {
        "schema": 1, "run_id": run_id, "suite": suite.name,
        "created_at": datetime.now(timezone.utc).isoformat(), "seed": suite.seed,
        "pair_concurrency": suite.pair_concurrency, "start_skew_ms": suite.start_skew_ms,
        "config_sha256": digest(suite.config) if suite.config.is_file() else None,
        "candidates": {c.name: {"selection": c.selection, "expected_model": c.expected_model}
                       for c in suite.candidates},
        "cases": {c.id: {"tier": c.tier, "prompt_sha256": hashlib.sha256(c.prompt.encode()).hexdigest(), "rubric": c.rubric}
                  for c in chosen},
        "schedule": schedule,
    }
    atomic_json(run_dir / "manifest.json", manifest)
    return run_dir


def classify(returncode: int | None, timed_out: bool, stdout: str, stderr: str) -> str:
    combined = (stdout + "\n" + stderr).lower()
    if timed_out: return "timeout"
    if returncode == 0 and stdout.strip(): return "success"
    if returncode == 0: return "empty_output"
    if "429" in combined or "rate limit" in combined or "rate_limit" in combined or "ratelimit" in combined: return "rate_limited"
    if "provider" in combined or "api error" in combined: return "provider_error"
    return "process_error"


def parse_token_usage(stderr: str) -> dict[str, Any] | None:
    matches = TOKEN_RE.findall(stderr)
    if not matches: return None
    prompt = sum(int(x[0]) for x in matches); completion = sum(int(x[1]) for x in matches)
    cached = sum(int(x[2]) for x in matches)
    return {"prompt": prompt, "completion": completion, "cached": cached,
            "cache_hit_rate": cached/prompt if prompt else None}


def parse_atomcode_jsonl(raw: str) -> tuple[list[dict[str, Any]], str, dict[str, Any] | None]:
    events: list[dict[str, Any]] = []
    for number, line in enumerate(raw.splitlines(), 1):
        if not line.strip():
            continue
        value = json.loads(line)
        if not isinstance(value, dict) or not isinstance(value.get("type"), str):
            raise ValueError(f"invalid AtomCode JSONL event on line {number}")
        events.append(value)
    starts = [event for event in events if event.get("type") == "run.started"]
    if len(starts) != 1 or not events or events[0] is not starts[0]:
        raise ValueError("expected exactly one leading run.started event")
    if starts[0].get("schema_version") != 1:
        raise ValueError(f"unsupported AtomCode JSONL schema: {starts[0].get('schema_version')}")
    answer = "".join(str(event.get("text", "")) for event in events
                     if event.get("type") == "message.delta")
    terminals = [event for event in events
                 if event.get("type") in {"turn.completed", "run.failed"}]
    if len(terminals) != 1:
        raise ValueError(f"expected exactly one terminal event, got {len(terminals)}")
    terminal = next(
        (event for event in terminals if event.get("type") == "turn.completed"), None
    )
    usage = None if terminal is None else {
        "prompt": int(terminal.get("prompt_tokens", 0)),
        "completion": int(terminal.get("completion_tokens", 0)),
        "cached": int(terminal.get("cached_tokens", 0)),
        "cache_hit_rate": terminal.get("cache_hit_rate"),
        "ttft_ms": terminal.get("ttft_ms"),
        "rounds": int(terminal.get("rounds", 0)),
        "tool_calls": int(terminal.get("tool_calls", 0)),
    }
    return events, answer, usage


def classify_jsonl(returncode: int | None, timed_out: bool, answer: str,
                   events: list[dict[str, Any]], stderr: str,
                   jsonl_error: str | None) -> str:
    if timed_out: return "timeout"
    if jsonl_error: return "process_error"
    if any(event.get("type") == "rate_limit" for event in events): return "rate_limited"
    terminal = next((event for event in events
                     if event.get("type") in {"turn.completed", "run.failed"}), None)
    if terminal is None or returncode is None: return "process_error"
    terminal_code = int(terminal.get("exit_code", 1))
    if terminal_code != returncode: return "protocol_error"
    if returncode != 0:
        reason = str(terminal.get("stop_reason", "")).lower()
        combined = (json.dumps(events, ensure_ascii=False) + "\n" + stderr).lower()
        if "rate" in reason or "429" in combined: return "rate_limited"
        if "provider" in reason or "provider" in combined or "api error" in combined:
            return "provider_error"
        return "process_error"
    return "success" if answer.strip() else "empty_output"


async def run_one(suite: Suite, case: Case, candidate: Candidate, rep: int,
                  pair_dir: Path, ready: asyncio.Event) -> dict[str, Any]:
    out_dir = pair_dir / candidate.name
    work = out_dir / "work"
    out_dir.mkdir(parents=True, exist_ok=True)
    home = Path(tempfile.mkdtemp(prefix=f"atomcode-eval-{candidate.name}-"))
    source_home = suite.config.parent
    for auth_name in ("auth.toml", "codingplan_sync.json", "device_id"):
        source = source_home / auth_name
        if source.is_file():
            shutil.copy2(source, home / auth_name)
    if case.fixture:
        shutil.copytree(str(case.fixture), str(work))
    else:
        work.mkdir()
    prompt = out_dir / "prompt.md"
    prompt.write_text(case.prompt)
    argv = [suite.atomcode_bin, "--provider", candidate.selection, "--config", str(suite.config),
            "--prompt-file", str(prompt), "-C", str(work), "--ephemeral",
            "--output-format", "jsonl", "--dev", "--no-telemetry"]
    if case.tier == "model": argv.append("--no-tools")
    if case.allow_edits: argv.append("--dangerously-skip-permissions")
    env = os.environ.copy(); env["ATOMCODE_HOME"] = str(home)
    await ready.wait()
    started_wall = time.time_ns(); started = time.monotonic_ns()
    timed_out = False
    try:
        try:
            proc = await asyncio.create_subprocess_exec(*argv, stdout=asyncio.subprocess.PIPE,
                                                        stderr=asyncio.subprocess.PIPE, env=env)
            try:
                stdout_b, stderr_b = await asyncio.wait_for(proc.communicate(), timeout=case.timeout)
            except asyncio.TimeoutError:
                timed_out = True; proc.kill(); stdout_b, stderr_b = await proc.communicate()
            returncode = proc.returncode
        except OSError as exc:
            stdout_b, stderr_b, returncode = b"", str(exc).encode(), None
    finally:
        shutil.rmtree(str(home), ignore_errors=True)
    ended = time.monotonic_ns()
    events_raw = stdout_b.decode(errors="replace")
    stderr = scrub(stderr_b.decode(errors="replace"))
    try:
        events, stdout, token_usage = parse_atomcode_jsonl(events_raw)
        jsonl_error = None
    except (json.JSONDecodeError, ValueError, TypeError) as exc:
        events, stdout, token_usage = [], events_raw, None
        jsonl_error = str(exc)
    (out_dir / "events.jsonl").write_text(scrub(events_raw))
    (out_dir / "stdout.txt").write_text(stdout); (out_dir / "stderr.txt").write_text(stderr)
    if case.fixture:
        diff_run = subprocess.run(["diff", "-ruN", str(case.fixture), str(work)],
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                  universal_newlines=True)
        (out_dir/"changes.diff").write_text(scrub(diff_run.stdout))
    actual_model = events[0].get("model") if events else None
    model_match = actual_model == candidate.expected_model
    outcome = classify_jsonl(returncode, timed_out, stdout, events, stderr, jsonl_error)
    if outcome == "success" and not model_match:
        outcome = "model_mismatch"
    meta = {"case": case.id, "candidate": candidate.name, "repetition": rep,
            "selection": candidate.selection, "expected_model": candidate.expected_model,
            "started_unix_ns": started_wall, "duration_ms": (ended-started)/1_000_000,
            "returncode": returncode, "timed_out": timed_out,
            "outcome": outcome,
            "token_usage": token_usage, "jsonl_error": jsonl_error,
            "event_count": len(events),
            "actual_provider": events[0].get("provider") if events else None,
            "actual_model": actual_model,
            "model_match": model_match,
            "argv": argv}
    atomic_json(out_dir / "meta.json", meta)
    return meta


async def run_pair(suite: Suite, case: Case, item: dict[str, Any], run_dir: Path,
                   semaphore: asyncio.Semaphore) -> None:
    async with semaphore:
        pair_dir = run_dir / "raw" / f"{case.id}--r{item['repetition']}"
        pair_dir.mkdir(parents=True, exist_ok=True)
        ready = asyncio.Event()
        by_name = {c.name: c for c in suite.candidates}
        tasks = [asyncio.create_task(run_one(suite, case, by_name[name], item["repetition"], pair_dir, ready))
                 for name in item["launch_order"]]
        await asyncio.sleep(0); ready.set()
        metas = await asyncio.gather(*tasks)
        skew = (max(m["started_unix_ns"] for m in metas)-min(m["started_unix_ns"] for m in metas))/1_000_000
        atomic_json(pair_dir / "pair.json", {"start_skew_ms": skew,
                    "strict_pair": skew <= suite.start_skew_ms, "runs": metas})


async def run_all(suite: Suite, cases: list[Case], run_dir: Path) -> None:
    manifest = json.loads((run_dir / "manifest.json").read_text())
    by_id = {c.id: c for c in cases}
    semaphore = asyncio.Semaphore(suite.pair_concurrency)
    await asyncio.gather(*(run_pair(suite, by_id[i["case"]], i, run_dir, semaphore)
                             for i in manifest["schedule"]))


def percentile(values: list[float], p: float) -> float | None:
    if not values: return None
    xs = sorted(values); pos = (len(xs)-1)*p; lo, hi = math.floor(pos), math.ceil(pos)
    return xs[lo] if lo == hi else xs[lo] + (xs[hi]-xs[lo])*(pos-lo)


def bootstrap_ci(values: list[float], seed: int, samples: int = 10000) -> list[float] | None:
    if not values: return None
    rng = random.Random(seed); n = len(values)
    means = [sum(values[rng.randrange(n)] for _ in range(n))/n for _ in range(samples)]
    return [percentile(means, .025), percentile(means, .975)]  # type: ignore[list-item]


def summary(run_dir: Path) -> dict[str, Any]:
    manifest = json.loads((run_dir / "manifest.json").read_text())
    data: dict[str, Any] = {"run_id": manifest["run_id"], "candidates": {}}
    for name in manifest["candidates"]:
        metas = []
        for meta_path in run_dir.glob(f"raw/*/{name}/meta.json"):
            meta = json.loads(meta_path.read_text())
            if not meta.get("token_usage"):
                events_path = meta_path.parent/"events.jsonl"
                if events_path.is_file():
                    try:
                        _, _, meta["token_usage"] = parse_atomcode_jsonl(events_path.read_text())
                    except (json.JSONDecodeError, ValueError, TypeError):
                        meta["token_usage"] = None
                else:
                    stderr_path = meta_path.parent/"stderr.txt"
                    meta["token_usage"] = parse_token_usage(stderr_path.read_text()) if stderr_path.is_file() else None
            metas.append(meta)
        durations = [float(m["duration_ms"]) for m in metas]
        cache_rates = [float(m["token_usage"]["cache_hit_rate"]) for m in metas
                       if m.get("token_usage") and m["token_usage"].get("cache_hit_rate") is not None]
        cache_cold = [float(m["token_usage"]["cache_hit_rate"]) for m in metas if m.get("repetition") == 1
                      and m.get("token_usage") and m["token_usage"].get("cache_hit_rate") is not None]
        cache_repeat = [float(m["token_usage"]["cache_hit_rate"]) for m in metas if m.get("repetition", 1) > 1
                        and m.get("token_usage") and m["token_usage"].get("cache_hit_rate") is not None]
        verifications = [json.loads(p.read_text()) for p in run_dir.glob(f"raw/*/{name}/verification.json")]
        outcomes: dict[str, int] = {}
        for m in metas: outcomes[m["outcome"]] = outcomes.get(m["outcome"], 0) + 1
        data["candidates"][name] = {"runs": len(metas),
            "success_rate": outcomes.get("success", 0)/len(metas) if metas else None,
            "latency_ms": {"p50": percentile(durations,.5), "p90": percentile(durations,.9),
                           "p95": percentile(durations,.95)},
            "cache_hit_rate": {"samples": len(cache_rates), "mean": statistics.mean(cache_rates) if cache_rates else None,
                               "p50": percentile(cache_rates,.5), "p95": percentile(cache_rates,.95)},
            "cache_cohorts": {"first_repetition_mean": statistics.mean(cache_cold) if cache_cold else None,
                              "repeat_mean": statistics.mean(cache_repeat) if cache_repeat else None},
            "verification": {"samples": len(verifications),
                             "pass_rate": sum(1 for v in verifications if v.get("passed"))/len(verifications) if verifications else None},
            "outcomes": outcomes}
    judged: dict[str, list[dict[str, int]]] = {name: [] for name in manifest["candidates"]}
    wins = {name: 0 for name in manifest["candidates"]}; ties = 0
    paired_differences = []
    for judgment_path in sorted((run_dir/"judgments").glob("*.json")):
        mapping_path = run_dir/"private"/(judgment_path.stem+".mapping.json")
        if not mapping_path.is_file(): continue
        judgment = json.loads(judgment_path.read_text()); mapping = json.loads(mapping_path.read_text())
        for alias, name in mapping.items(): judged[name].append(judgment["scores"][alias])
        composite = {alias: statistics.mean(judgment["scores"][alias].values()) for alias in ("A", "B")}
        official_alias = next(alias for alias, name in mapping.items() if name == "official")
        volcano_alias = next(alias for alias, name in mapping.items() if name == "volcengine")
        paired_differences.append(composite[official_alias]-composite[volcano_alias])
        winner = judgment["winner"]
        if winner == "tie": ties += 1
        else: wins[mapping[winner]] += 1
    data["blind_judging"] = {"pairs": sum(len(v) for v in judged.values())//2, "ties": ties, "wins": wins,
        "mean_scores": {name: ({key: statistics.mean(x[key] for x in rows) for key in rows[0]} if rows else {})
                        for name, rows in judged.items()},
        "official_minus_volcengine_composite": statistics.mean(paired_differences) if paired_differences else None,
        "paired_bootstrap_95_ci": bootstrap_ci(paired_differences, manifest["seed"]) }
    atomic_json(run_dir / "summary.json", data); return data


def run_verifier(case: Case, candidate_dir: Path) -> dict[str, Any] | None:
    if not case.verify:
        return None
    started = time.monotonic()
    try:
        done = subprocess.run(list(case.verify), cwd=str(candidate_dir / "work"),
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              timeout=case.timeout, universal_newlines=True)
        result = {"passed": done.returncode == 0, "returncode": done.returncode,
                  "timed_out": False, "stdout": scrub(done.stdout), "stderr": scrub(done.stderr)}
    except subprocess.TimeoutExpired as exc:
        result = {"passed": False, "returncode": None, "timed_out": True,
                  "stdout": scrub((exc.stdout or b"").decode(errors="replace") if isinstance(exc.stdout, bytes) else exc.stdout or ""),
                  "stderr": scrub((exc.stderr or b"").decode(errors="replace") if isinstance(exc.stderr, bytes) else exc.stderr or "")}
    result["duration_ms"] = (time.monotonic()-started)*1000
    atomic_json(candidate_dir / "verification.json", result)
    return result


def build_packets(suite: Suite, cases: list[Case], run_dir: Path) -> list[Path]:
    by_id = {c.id: c for c in cases}; manifest = json.loads((run_dir/"manifest.json").read_text())
    rng = random.Random(manifest["seed"] ^ 0xA5A5A5A5); packets = []
    forbidden = [x for c in suite.candidates for x in (c.selection, c.expected_model, c.name)]
    for pair in sorted((run_dir/"raw").glob("*/pair.json")):
        pair_dir = pair.parent; case_id = pair_dir.name.rsplit("--r", 1)[0]; case = by_id[case_id]
        names = [c.name for c in suite.candidates]; rng.shuffle(names)
        aliases = {"A": names[0], "B": names[1]}; candidates = {}
        for alias, name in aliases.items():
            cdir = pair_dir/name; verification = run_verifier(case, cdir)
            meta = json.loads((cdir/"meta.json").read_text())
            candidates[alias] = {"outcome": meta["outcome"], "duration_ms": meta["duration_ms"],
                                 "answer": (cdir/"stdout.txt").read_text(),
                                 "changes": (cdir/"changes.diff").read_text() if (cdir/"changes.diff").is_file() else "",
                                 "verification": verification}
        packet = {"case": {"id": case.id, "tier": case.tier, "prompt": case.prompt,
                            "rubric": case.rubric}, "candidates": candidates}
        encoded = json.dumps(packet, ensure_ascii=False)
        encoded = encoded.replace(str(ROOT), "<EVAL_ROOT>")
        for value in sorted((x for x in forbidden if x), key=len, reverse=True):
            encoded = encoded.replace(value, "[CANDIDATE]")
        packet = json.loads(encoded)
        encoded = json.dumps(packet, ensure_ascii=False)
        leaked = [value for value in forbidden if value and value in encoded]
        if leaked: raise ValueError(f"provider identity leaked into judge packet for {case.id}")
        out = run_dir/"judge_packets"/(pair_dir.name+".json"); atomic_json(out, packet)
        atomic_json(run_dir/"private"/(pair_dir.name+".mapping.json"), aliases); packets.append(out)
    return packets


def extract_json(text: str) -> dict[str, Any]:
    stripped = text.strip()
    if stripped.startswith("```"):
        stripped = re.sub(r"^```(?:json)?\s*|\s*```$", "", stripped, flags=re.I)
    value = json.loads(stripped)
    if not isinstance(value, dict): raise ValueError("Codex judgment is not an object")
    return value


def validate_judgment(value: dict[str, Any]) -> None:
    if value.get("winner") not in ("A", "B", "tie"): raise ValueError("invalid judgment winner")
    for alias in ("A", "B"):
        scores = value.get("scores", {}).get(alias, {})
        for key in ("correctness", "quality", "instruction_following", "agent_execution"):
            score = scores.get(key)
            if not isinstance(score, int) or isinstance(score, bool) or not 0 <= score <= 100:
                raise ValueError(f"invalid {alias}.{key} score")
    confidence = value.get("confidence")
    if not isinstance(confidence, (int, float)) or isinstance(confidence, bool) or not 0 <= confidence <= 1:
        raise ValueError("invalid confidence")
    if not isinstance(value.get("evidence"), list) or not isinstance(value.get("critical_failures"), list):
        raise ValueError("evidence and critical_failures must be arrays")


def codex_exec(suite: Suite, prompt: str, cwd: Path, output: Path, timeout: int = 900) -> str:
    output.parent.mkdir(parents=True, exist_ok=True)
    argv = [suite.codex_bin, "exec", "--skip-git-repo-check", "--json", "--cd", str(cwd),
            "--sandbox", "read-only", "-o", str(output)]
    if suite.codex_model: argv[6:6] = ["-m", suite.codex_model]
    done = subprocess.run(argv, input=prompt, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          timeout=timeout, universal_newlines=True)
    output.with_suffix(".events.jsonl").write_text(scrub(done.stdout))
    output.with_suffix(".stderr.txt").write_text(scrub(done.stderr))
    if done.returncode != 0: raise RuntimeError(f"Codex exited {done.returncode}")
    answer = output.read_text() if output.is_file() else ""
    if not answer.strip():
        for line in done.stdout.splitlines():
            try: event = json.loads(line)
            except json.JSONDecodeError: continue
            item = event.get("item", {})
            if event.get("type") == "item.completed" and item.get("type") == "agent_message":
                answer = item.get("text", "")
    return answer


def judge(suite: Suite, cases: list[Case], run_dir: Path) -> None:
    template = (ROOT/"prompts/codex-judge.md").read_text()
    def judge_one(packet_path: Path) -> None:
        prompt = template + "\n\nEvaluation packet:\n" + packet_path.read_text()
        raw_path = run_dir/"judgments_raw"/(packet_path.stem+".txt")
        answer = codex_exec(suite, prompt, run_dir, raw_path)
        value = extract_json(answer); validate_judgment(value)
        atomic_json(run_dir/"judgments"/(packet_path.stem+".json"), value)
    packets = build_packets(suite, cases, run_dir)
    with concurrent.futures.ThreadPoolExecutor(max_workers=min(4, len(packets) or 1)) as pool:
        futures = [pool.submit(judge_one, packet) for packet in packets]
        for future in concurrent.futures.as_completed(futures): future.result()


def report(suite: Suite, run_dir: Path) -> None:
    data = summary(run_dir)
    payload = {"summary": data,
               "decision_rules": {"capability_lead_points": 5, "success_rate_points": 3,
                                  "p95_latency_material_percent": 20,
                                  "minimum_formal_suite": "20 model + 8 agent cases, 3 repetitions each"},
               "note": "Candidate identities were revealed only after blind judging. A smoke run validates the harness and cannot select a production default."}
    prompt = (ROOT/"prompts/codex-report.md").read_text()+"\n\nData:\n"+json.dumps(payload, ensure_ascii=False)
    out = run_dir/"report.md"; answer = codex_exec(suite, prompt, run_dir, out)
    required = ("Executive conclusion", "Capability", "Stability", "Deployment recommendation")
    if not all(x.lower() in answer.lower() for x in required):
        raise ValueError("Codex report is missing required sections")


def combined_report(suite: Suite, model_run: Path, agent_run: Path, output: Path) -> None:
    payload = {"model_layer": summary(model_run), "agent_layer": summary(agent_run),
               "suite_completion": {"formal_quick_suite_complete": True,
                                    "model_cases": 20, "agent_cases": 8, "repetitions": 3,
                                    "model_pairs": 60, "agent_pairs": 24,
                                    "pair_concurrency": 2, "maximum_candidate_requests": 4,
                                    "model_verification": "Codex rubric judging; no external executable verifier is applicable",
                                    "agent_verification": "21/24 per candidate have executable verifiers; the remaining 3 are the diagnosis-only case whose required outcome is an empty diff and correct diagnosis"},
               "decision_rules": {"capability_lead_points": 5, "confidence_interval_must_exclude_zero": True,
                                  "success_rate_points": 3, "p95_latency_material_percent": 20},
               "configuration_limitations": {"official_context_window": 512000,
                                             "volcengine_context_window": 512000,
                                             "long_context_scoring_cap": 512000}}
    prompt = (ROOT/"prompts/codex-report.md").read_text()+"\n\nCombined formal evaluation data:\n"+json.dumps(payload, ensure_ascii=False)
    answer = codex_exec(suite, prompt, output.parent, output)
    required = ("Executive conclusion", "Capability", "Stability", "cache", "Deployment recommendation")
    if not all(x.lower() in answer.lower() for x in required): raise ValueError("combined report missing required sections")


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(); p.add_argument("--suite", type=Path, default=ROOT/"benchmark.json")
    sub = p.add_subparsers(dest="command", required=True)
    prep = sub.add_parser("prepare"); prep.add_argument("--case", action="append")
    prep.add_argument("--tier", choices=("model", "agent"))
    prep.add_argument("--repetitions", type=int); prep.add_argument("--results", type=Path, default=ROOT/"results")
    run = sub.add_parser("run"); run.add_argument("--run-dir", type=Path, required=True)
    judge_p = sub.add_parser("judge"); judge_p.add_argument("--run-dir", type=Path, required=True)
    summ = sub.add_parser("summarize"); summ.add_argument("--run-dir", type=Path, required=True)
    report_p = sub.add_parser("report"); report_p.add_argument("--run-dir", type=Path, required=True)
    combined = sub.add_parser("combined-report"); combined.add_argument("--model-run", type=Path, required=True)
    combined.add_argument("--agent-run", type=Path, required=True); combined.add_argument("--output", type=Path, required=True)
    return p.parse_args()


def main() -> int:
    args = parse_args(); suite = load_suite(args.suite); cases = discover_cases(ROOT/"cases")
    if args.command == "prepare":
        selected = set(args.case) if args.case else None
        if args.tier:
            tier_ids = {case.id for case in cases if case.tier == args.tier}
            selected = tier_ids if selected is None else selected & tier_ids
        print(prepare(suite, cases, args.results, selected, args.repetitions))
    elif args.command == "run": asyncio.run(run_all(suite, cases, args.run_dir))
    elif args.command == "judge": judge(suite, cases, args.run_dir)
    elif args.command == "summarize": print(json.dumps(summary(args.run_dir), ensure_ascii=False, indent=2))
    elif args.command == "report": report(suite, args.run_dir)
    elif args.command == "combined-report": combined_report(suite, args.model_run, args.agent_run, args.output)
    return 0


if __name__ == "__main__":
    try: raise SystemExit(main())
    except (ValueError, FileNotFoundError, json.JSONDecodeError) as exc:
        print(f"error: {exc}", file=sys.stderr); raise SystemExit(2)
