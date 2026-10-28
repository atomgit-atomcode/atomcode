#!/usr/bin/env python3
"""Isolated multi-turn prompt A/B evaluation; Python 3.11+, stdlib only."""
import argparse
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import random
import re
import time
import tomllib
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent.parent


def save(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")
    temporary.replace(path)


def rust_constant(source, name):
    match = re.search(r"const " + re.escape(name) + r': &str =\s*"((?:\\.|[^"\\])*)";', source, re.S)
    if not match:
        raise ValueError(f"Cannot extract Rust string constant {name}")
    # Rust line continuations remove the newline and following whitespace.
    literal = re.sub(r"\\\n\s*", "", match[1])
    return json.loads('"' + literal.replace("\n", "\\n") + '"')


def snapshot(label, persona):
    path = ROOT / "results" / f"{label}-prompts.json"
    if path.exists():
        raise ValueError(f"Snapshot already exists: {path}")
    source = (REPO / "crates/atomcode-capabilities/src/compaction.rs").read_text()
    prompts = {"persona": persona.read_text(),
               "summary_system": rust_constant(source, "SUMMARY_SYSTEM_PROMPT"),
               "summary_template": rust_constant(source, "SUMMARY_TEMPLATE"),
               "summary_framing": rust_constant(source, "SUMMARY_FRAMING")}
    if not prompts["persona"].strip():
        raise ValueError("Persona dump is empty")
    prompts["sha256"] = hashlib.sha256(json.dumps(prompts, sort_keys=True).encode()).hexdigest()
    save(path, prompts)


def configured_provider(path, selection):
    config = tomllib.loads(path.expanduser().read_text())
    if selection in config.get("models", {}):
        model = config["models"][selection]
        account = config["provider_accounts"][model["account"]]
        data = {**account, **model}
        kind = account.get("provider", "openai")
    else:
        data = config["providers"][selection]
        kind = data.get("type", "openai")
    # Supported built-in defaults from atomcode-config/provider_preset.rs.
    defaults = {"taotoken": "https://taotoken.net/api/v1", "deepseek": "https://api.deepseek.com/v1",
                "openrouter": "https://openrouter.ai/api/v1"}
    endpoint = data.get("base_url") or defaults.get(kind)
    key = data.get("api_key")
    if not endpoint or not key:
        raise ValueError("Evaluation needs an explicit/static-key compatible account; gateway signing is not supported")
    if not endpoint.startswith("https://"):
        raise ValueError("Evaluation endpoint must use HTTPS")
    return endpoint.rstrip("/") + "/chat/completions", key, data


class RejectRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise urllib.error.HTTPError(req.full_url, code, "redirect refused", headers, fp)


class Client:
    def __init__(self, endpoint, key, model, effort, max_tokens, timeout):
        self.endpoint, self.key, self.model = endpoint, key, model
        self.effort, self.max_tokens, self.timeout = effort, max_tokens, timeout
        self.opener = urllib.request.build_opener(RejectRedirects())

    def complete(self, messages):
        body = {"model": self.model, "messages": messages, "stream": False, "max_tokens": self.max_tokens}
        if self.effort:
            body["reasoning_effort"] = self.effort
        req = urllib.request.Request(self.endpoint, json.dumps(body).encode(),
                                     {"Authorization": "Bearer " + self.key, "Content-Type": "application/json"})
        started = time.monotonic()
        try:
            with self.opener.open(req, timeout=self.timeout) as response:
                raw = json.load(response)
        except urllib.error.HTTPError as exc:
            # Never persist request headers, response bodies or a credentialed URL.
            raise RuntimeError(f"HTTP {exc.code}") from None
        except Exception as exc:
            raise RuntimeError(type(exc).__name__) from None
        choice = raw["choices"][0]
        msg = choice["message"]
        text = msg.get("content")
        if choice.get("finish_reason") != "stop" or not isinstance(text, str) or not text.strip():
            raise RuntimeError("incomplete_or_empty_response")
        assistant = {"role": "assistant", "content": text}
        if msg.get("reasoning_content"):
            assistant["reasoning_content"] = msg["reasoning_content"]
        metadata = {"model": raw.get("model"), "usage": raw.get("usage", {}),
                    "seconds": time.monotonic() - started, "finish_reason": choice.get("finish_reason")}
        return assistant, metadata


def trial(client, prompts, case, compressed, progress=None):
    if compressed and prompts.get("loaded_skill"):
        raise ValueError("Loaded-skill exposure currently supports ordinary trials only")
    messages = [{"role": "system", "content": prompts["persona"]}]
    turns, requests = [], []
    for index, user in enumerate(case["turns"]):
        if compressed and index == 3:
            transcript = "\n".join(m["role"].upper() + ": " + m["content"] for m in messages[1:])
            summary, meta = client.complete([
                {"role": "system", "content": prompts["summary_system"]},
                {"role": "user", "content": prompts["summary_template"] + "\n\nConversation:\n" + transcript}])
            requests.append({"kind": "summary", **meta})
            turns.append({"kind": "summary", "text": summary["content"]})
            if progress:
                progress({"turns": turns, "requests": requests})
            messages = [messages[0], {"role": "user", "content": prompts["summary_framing"] + "\n" + summary["content"]}]
        messages.append({"role": "user", "content": user})
        # Controlled exposure: replay a successful use_skill result, not tool execution.
        # This tests following a loaded skill, not automatic skill selection.
        if index == 0 and prompts.get("loaded_skill"):
            messages.extend([
                {"role": "assistant", "content": None, "tool_calls": [{
                    "id": "eval_loaded_skill", "type": "function", "function": {
                        "name": "use_skill", "arguments": '{"name":"brainstorming"}'}}]},
                {"role": "tool", "tool_call_id": "eval_loaded_skill", "content": prompts["loaded_skill"]}])
        answer, meta = client.complete(messages)
        requests.append({"kind": "answer", "turn": index + 1, **meta})
        # Keep reasoning in memory for the endpoint's continuation contract, not artifacts.
        turns.append({"kind": "answer", "user": user, "text": answer["content"]})
        messages.append(answer)
        if progress:
            progress({"turns": turns, "requests": requests})
    return {"turns": turns, "requests": requests, "status": "review_required"}


def run(args):
    endpoint, key, model_config = configured_provider(args.config, args.selection)
    model = model_config["model"]
    client = Client(endpoint, key, model, model_config.get("reasoning_effort"), args.max_tokens, args.timeout)
    case_path = getattr(args, "cases", ROOT / "cases.json")
    cases = json.loads(case_path.read_text())
    if args.case:
        cases = [case for case in cases if case["id"] in args.case]
        if not cases:
            raise ValueError("No matching cases")
    prompts = {name: json.loads((ROOT / "results" / f"{name}-prompts.json").read_text())
               for name in getattr(args, "arms", ["baseline", "candidate"])}
    out = args.output
    out.mkdir(parents=True, exist_ok=False)
    rng = random.Random(args.seed)
    jobs = [(case, rep, arm, mode) for case in cases for rep in range(args.repetitions)
            for arm in prompts for mode in ([False, True] if args.phase == "both" else [args.phase == "summary"])]
    rng.shuffle(jobs)
    manifest = []
    for number, (case, rep, arm, mode) in enumerate(jobs):
        manifest.append({"packet": f"trial-{number:03d}", "case": case["id"], "repetition": rep + 1,
                         "arm": arm, "phase": "summary" if mode else "ordinary"})
    save(out / "private-manifest.json", {"selection": args.selection, "requested_model": model,
         "effort": client.effort, "max_tokens": args.max_tokens, "seed": args.seed,
         "cases_sha256": hashlib.sha256(case_path.read_bytes()).hexdigest(),
         "prompts": {arm: data["sha256"] for arm, data in prompts.items()}, "trials": manifest})

    def work(entry, job):
        case, _, arm, mode = job
        partial = {"turns": [], "requests": []}
        def update(state):
            partial.update(state)
            save(out / (entry["packet"] + ".partial.json"), state)
            latest = state["requests"][-1]
            print(json.dumps({"packet": entry["packet"], "kind": latest["kind"],
                              "completed_requests": len(state["requests"]), "seconds": latest["seconds"]}), flush=True)
        try:
            result = trial(client, prompts[arm], case, mode, update)
        except Exception as exc:
            result = {**partial, "status": "infrastructure_failure", "error": type(exc).__name__ + ": " + str(exc).replace(key, "[REDACTED]")}
        save(out / (entry["packet"] + ".json"), {"case": case["id"], "rubric": case["rubric"], **result})
        (out / (entry["packet"] + ".partial.json")).unlink(missing_ok=True)
        return {"packet": entry["packet"], "status": result["status"]}

    with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
        futures = [pool.submit(work, entry, job) for entry, job in zip(manifest, jobs)]
        statuses = []
        for future in concurrent.futures.as_completed(futures):
            status = future.result()
            statuses.append(status)
            print(json.dumps(status), flush=True)
    save(out / "execution-summary.json", statuses)
    return int(any(status["status"] == "infrastructure_failure" for status in statuses))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    snap = sub.add_parser("snapshot")
    snap.add_argument("--label", choices=["baseline", "candidate"], required=True)
    snap.add_argument("--persona", type=Path, required=True)
    run_parser = sub.add_parser("run")
    run_parser.add_argument("--config", type=Path, default=Path.home()/".atomcode/config.toml")
    run_parser.add_argument("--selection", default="taotoken/deepseek-v4.1-flash-1017")
    run_parser.add_argument("--output", type=Path, required=True)
    run_parser.add_argument("--repetitions", type=int, default=3)
    run_parser.add_argument("--concurrency", type=int, default=4)
    run_parser.add_argument("--max-tokens", type=int, default=8192)
    run_parser.add_argument("--timeout", type=int, default=300)
    run_parser.add_argument("--seed", type=int, default=20261025)
    run_parser.add_argument("--phase", choices=["ordinary", "summary", "both"], default="ordinary")
    run_parser.add_argument("--case", action="append")
    run_parser.add_argument("--cases", type=Path, default=ROOT / "cases.json")
    run_parser.add_argument("--arms", nargs="+", choices=["baseline", "candidate", "summary-only", "persona-only", "skill-none", "skill-original", "skill-scoped"], default=["baseline", "candidate"])
    args = parser.parse_args()
    if args.command == "snapshot":
        snapshot(args.label, args.persona)
        return 0
    if min(args.repetitions, args.concurrency, args.max_tokens, args.timeout) < 1:
        parser.error("numeric budgets must be positive")
    return run(args)


if __name__ == "__main__":
    raise SystemExit(main())
