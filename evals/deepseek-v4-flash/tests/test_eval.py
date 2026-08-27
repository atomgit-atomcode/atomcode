import asyncio
import importlib.util
import json
import os
import sys
import tempfile
import textwrap
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("deepseek_eval", ROOT / "eval.py")
ev = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = ev
SPEC.loader.exec_module(ev)


class EvalTests(unittest.TestCase):
    def test_suite_and_cases_load(self):
        suite = ev.load_suite(ROOT / "benchmark.json")
        cases = ev.discover_cases(ROOT / "cases")
        self.assertEqual([c.name for c in suite.candidates], ["official", "volcengine"])
        self.assertEqual(len([case for case in cases if case.tier == "model"]), 20)
        self.assertEqual(len([case for case in cases if case.tier == "agent"]), 8)

    def test_scrub_removes_common_secrets(self):
        value = ev.scrub("Authorization: Bearer abc API_KEY=secret token: xyz")
        self.assertNotIn("abc", value)
        self.assertNotIn("secret", value)
        self.assertNotIn("xyz", value)

    def test_percentile_interpolates(self):
        self.assertEqual(ev.percentile([1, 2, 3, 4], .5), 2.5)
        self.assertIsNone(ev.percentile([], .95))

    def test_token_usage_and_cache_hit_rate(self):
        usage = ev.parse_token_usage("[tokens] prompt=100 completion=9 cached=25")
        self.assertEqual(usage["cached"], 25)
        self.assertEqual(usage["cache_hit_rate"], .25)

    def test_jsonl_requires_one_terminal_and_empty_answer_is_not_success(self):
        raw = "\n".join([
            json.dumps({"type": "run.started", "schema_version": 1,
                        "provider": "p", "model": "m"}),
            json.dumps({"type": "turn.completed", "exit_code": 0,
                        "stop_reason": "Stopped", "prompt_tokens": 10,
                        "completion_tokens": 0, "cached_tokens": 0}),
        ])
        events, answer, usage = ev.parse_atomcode_jsonl(raw)
        self.assertEqual(answer, "")
        self.assertEqual(usage["prompt"], 10)
        self.assertEqual(
            ev.classify_jsonl(0, False, answer, events, "", None),
            "empty_output",
        )
        with self.assertRaises(ValueError):
            ev.parse_atomcode_jsonl(json.dumps({"type": "run.started"}))

    def test_jsonl_rejects_terminal_process_exit_mismatch(self):
        events = [{"type": "turn.completed", "exit_code": 0,
                   "stop_reason": "Stopped"}]
        self.assertEqual(
            ev.classify_jsonl(2, False, "answer", events, "", None),
            "protocol_error",
        )

    def test_judgment_validation(self):
        good = {"winner": "tie", "scores": {
            "A": {"correctness": 80, "quality": 80, "instruction_following": 90, "agent_execution": 70},
            "B": {"correctness": 80, "quality": 80, "instruction_following": 90, "agent_execution": 70}},
            "evidence": [], "critical_failures": [], "confidence": .5}
        ev.validate_judgment(good)
        good["scores"]["A"]["quality"] = 101
        with self.assertRaises(ValueError):
            ev.validate_judgment(good)

    def test_extract_json_accepts_fence(self):
        self.assertEqual(ev.extract_json("```json\n{\"winner\":\"tie\"}\n```")["winner"], "tie")

    def test_prepare_is_deterministic_except_run_identity(self):
        suite = ev.load_suite(ROOT / "benchmark.json")
        cases = ev.discover_cases(ROOT / "cases")
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            selected = {cases[0].id}
            a = json.loads((ev.prepare(suite, cases, root, selected, 2)/"manifest.json").read_text())
            b = json.loads((ev.prepare(suite, cases, root, selected, 2)/"manifest.json").read_text())
            self.assertEqual(a["schedule"], b["schedule"])
            self.assertNotEqual(a["run_id"], b["run_id"])

    def test_pair_runs_concurrently_and_isolates_homes(self):
        with tempfile.TemporaryDirectory() as td:
            tmp = Path(td)
            fake = tmp / "fake_atomcode.py"
            fake.write_text(textwrap.dedent("""\
                #!/usr/bin/env python3
                import json, os, sys, time
                print(json.dumps({"home": os.environ["ATOMCODE_HOME"], "argv": sys.argv[1:]}))
                time.sleep(0.15)
            """))
            fake.chmod(0o755)
            base = ev.load_suite(ROOT / "benchmark.json")
            suite = ev.Suite(base.path, base.name, 1, 1, 5, 500, base.seed,
                             str(fake), base.codex_bin, None, base.config, base.candidates)
            case = ev.discover_cases(ROOT / "cases")[0]
            run = ev.prepare(suite, [case], tmp / "results", {case.id}, 1)
            started = time.monotonic()
            asyncio.run(ev.run_all(suite, [case], run))
            elapsed = time.monotonic() - started
            pair = json.loads(next((run / "raw").glob("*/pair.json")).read_text())
            self.assertTrue(pair["strict_pair"])
            summed = sum(item["duration_ms"] for item in pair["runs"])/1000
            self.assertLess(elapsed, summed, "candidate processes should overlap")
            homes = [json.loads(p.read_text())["home"] for p in (run/"raw").glob("*/*/stdout.txt")]
            self.assertEqual(len(set(homes)), 2)


if __name__ == "__main__":
    unittest.main()
