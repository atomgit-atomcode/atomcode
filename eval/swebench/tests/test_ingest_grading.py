"""Unit tests for eval/swebench/ingest_grading.py."""
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

INGEST = Path(__file__).resolve().parent.parent / "ingest_grading.py"


def make_meta(instance_id: str, status: str = "predicted", **extra) -> dict:
    meta = {
        "id": instance_id,
        "form": "swebench",
        "provider": "siliconflow",
        "exit_code": 0,
        "wall_ms": 12345,
        "timed_out": False,
        "had_denial": False,
        "denial_count": 0,
        "started_at": "2026-04-08T10:00:00Z",
        "ended_at": "2026-04-08T10:02:00Z",
        "run_id": "2026-04-08_10-00-00",
        "status": status,
        "swebench": {
            "repo": "sympy/sympy",
            "base_commit": "abc123",
            "prompt_template": "default",
            "include_hints": False,
            "dataset_revision": "rev1",
            "patch_size_bytes": 500,
        },
        "efficiency": {
            "turns": 5,
            "prompt_tokens": 1000,
            "completion_tokens": 100,
            "tool_calls": 5,
            "tool_breakdown": {},
            "stop_reason": "natural",
            "estimated_cost_usd": 0.01,
        },
    }
    meta.update(extra)
    return meta


def make_grader_report(per_instance: dict) -> dict:
    """Fake shape of what the upstream grader's report JSON looks like."""
    return {
        "resolved_ids": [iid for iid, v in per_instance.items() if v["resolved"]],
        "unresolved_ids": [iid for iid, v in per_instance.items() if not v["resolved"]],
        "report": per_instance,
    }


class TestIngestGrading(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.run_dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def _write_meta(self, instance_id: str, meta: dict):
        case_dir = self.run_dir / instance_id
        case_dir.mkdir()
        (case_dir / "meta.json").write_text(json.dumps(meta))

    def _run(self, grader_report: dict) -> tuple[int, str]:
        report_path = self.run_dir / "grader_report.json"
        report_path.write_text(json.dumps(grader_report))
        result = subprocess.run(
            [sys.executable, str(INGEST),
             "--run-dir", str(self.run_dir),
             "--grader-report", str(report_path)],
            capture_output=True, text=True, check=False,
        )
        return result.returncode, result.stderr

    def _read_meta(self, instance_id: str) -> dict:
        return json.loads((self.run_dir / instance_id / "meta.json").read_text())

    def test_resolved_instance_updates_meta(self):
        self._write_meta("sympy__sympy-1", make_meta("sympy__sympy-1"))
        report = make_grader_report({
            "sympy__sympy-1": {"resolved": True, "failure_mode": None},
        })
        code, err = self._run(report)
        self.assertEqual(code, 0, err)

        meta = self._read_meta("sympy__sympy-1")
        self.assertEqual(meta["status"], "resolved")
        self.assertEqual(meta["swebench_resolved"], True)
        self.assertIsNone(meta["swebench_failure_mode"])
        self.assertIn("graded_at", meta)

    def test_unresolved_applied_but_failed(self):
        self._write_meta("sympy__sympy-2", make_meta("sympy__sympy-2"))
        report = make_grader_report({
            "sympy__sympy-2": {"resolved": False, "failure_mode": "applied_but_failed"},
        })
        code, err = self._run(report)
        self.assertEqual(code, 0, err)

        meta = self._read_meta("sympy__sympy-2")
        self.assertEqual(meta["status"], "unresolved")
        self.assertEqual(meta["swebench_resolved"], False)
        self.assertEqual(meta["swebench_failure_mode"], "applied_but_failed")

    def test_instance_not_in_report_stays_predicted(self):
        """An instance that was selected for grading but isn't in the report
        (because the grader skipped it somehow) should stay status=predicted."""
        self._write_meta("sympy__sympy-3", make_meta("sympy__sympy-3"))
        report = make_grader_report({})  # empty report
        code, _ = self._run(report)
        self.assertEqual(code, 0)

        meta = self._read_meta("sympy__sympy-3")
        self.assertEqual(meta["status"], "predicted")
        self.assertNotIn("swebench_resolved", meta)

    def test_non_swebench_meta_is_left_alone(self):
        """A Form A/B meta.json in the same run dir must not be touched."""
        meta_a = {
            "id": "001-fizzbuzz",
            "form": "A",
            "status": "pass",
        }
        case_dir = self.run_dir / "001-fizzbuzz"
        case_dir.mkdir()
        (case_dir / "meta.json").write_text(json.dumps(meta_a))

        self._write_meta("sympy__sympy-4", make_meta("sympy__sympy-4"))
        report = make_grader_report({
            "sympy__sympy-4": {"resolved": True, "failure_mode": None},
        })
        self._run(report)

        # Form A meta should be unchanged
        reloaded = json.loads((case_dir / "meta.json").read_text())
        self.assertEqual(reloaded, meta_a)

    def test_idempotent_on_already_graded(self):
        """Running ingest twice with the same report is a no-op."""
        self._write_meta("sympy__sympy-5", make_meta("sympy__sympy-5"))
        report = make_grader_report({
            "sympy__sympy-5": {"resolved": True, "failure_mode": None},
        })
        self._run(report)
        first = self._read_meta("sympy__sympy-5")

        self._run(report)
        second = self._read_meta("sympy__sympy-5")
        # Strip graded_at timestamp for comparison
        first.pop("graded_at", None)
        second.pop("graded_at", None)
        self.assertEqual(first, second)


if __name__ == "__main__":
    unittest.main()
