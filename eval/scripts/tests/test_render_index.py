"""Unit tests for render_index.py."""
import json
import subprocess
import tempfile
import unittest
from pathlib import Path

RENDER = Path(__file__).resolve().parent.parent / "render_index.py"


def make_run(tmp: Path, summary: dict, cases: list[dict]) -> Path:
    """Create a fake run directory with summary.json + per-case meta.json files."""
    run_dir = tmp / "2026-04-07_12-00-00"
    run_dir.mkdir()
    (run_dir / "summary.json").write_text(json.dumps(summary))
    for c in cases:
        cd = run_dir / c["id"]
        cd.mkdir()
        (cd / "meta.json").write_text(json.dumps(c))
    return run_dir


class TestRenderIndex(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.tmp_path = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_renders_basic_index(self):
        run_dir = make_run(
            self.tmp_path,
            summary={
                "started_at": "2026-04-07T12:00:00Z",
                "ended_at": "2026-04-07T12:05:00Z",
                "status": "done",
                "atomcode": {
                    "version_string": "atomcode 2.5.0 (abc1234)",
                    "binary_path": "/tmp/atomcode",
                    "binary_sha256": "deadbeef",
                    "binary_mtime": "2026-04-07T11:00:00Z",
                },
                "runner_version": "scripts/eval/run.sh @ xyz789",
                "case_count": 2,
                "concurrency": 4,
                "totals": {
                    "pass": 1, "fail": 1, "denied": 0, "timeout": 0,
                    "cancelled": 0, "error": 0, "invalid": 0, "aborted": 0,
                },
            },
            cases=[
                {
                    "id": "001-fizzbuzz",
                    "form": "A",
                    "provider": "kimi",
                    "exit_code": 0,
                    "wall_ms": 4200,
                    "had_denial": False,
                    "denial_count": 0,
                    "status": "pass",
                },
                {
                    "id": "010-rust",
                    "form": "B",
                    "provider": "kimi",
                    "exit_code": 1,
                    "wall_ms": 58300,
                    "had_denial": False,
                    "denial_count": 0,
                    "status": "fail",
                },
            ],
        )
        result = subprocess.run(
            ["python3", str(RENDER), str(run_dir)],
            capture_output=True, text=True, check=False,
        )
        self.assertEqual(result.returncode, 0, f"stderr: {result.stderr}")
        index = (run_dir / "index.html").read_text()

        # Header / fingerprint
        self.assertIn("atomcode 2.5.0 (abc1234)", index)
        self.assertIn("deadbeef", index)
        # Both cases
        self.assertIn("001-fizzbuzz", index)
        self.assertIn("010-rust", index)
        # Status cells
        self.assertIn("pass", index)
        self.assertIn("fail", index)
        # Wall time formatted (4.2s, 58.3s)
        self.assertIn("4.2", index)
        self.assertIn("58.3", index)
        # Totals
        self.assertIn("pass 1", index)
        self.assertIn("fail 1", index)
        # Each case row links to its per-case detail page (case.html)
        self.assertIn('href="./001-fizzbuzz/case.html"', index)
        self.assertIn('href="./010-rust/case.html"', index)
        # And those detail pages should now exist on disk
        self.assertTrue((run_dir / "001-fizzbuzz" / "case.html").exists())
        self.assertTrue((run_dir / "010-rust" / "case.html").exists())

    def test_handles_invalid_case_with_no_meta(self):
        """Invalid cases (didn't run atomcode) have a stub meta.json."""
        run_dir = make_run(
            self.tmp_path,
            summary={
                "started_at": "2026-04-07T12:00:00Z",
                "ended_at": "2026-04-07T12:00:01Z",
                "status": "done",
                "atomcode": {
                    "version_string": "atomcode 2.5.0 (abc1234)",
                    "binary_path": "/tmp/atomcode",
                    "binary_sha256": "deadbeef",
                    "binary_mtime": "2026-04-07T11:00:00Z",
                },
                "runner_version": "scripts/eval/run.sh @ xyz",
                "case_count": 1,
                "concurrency": 1,
                "totals": {"pass": 0, "fail": 0, "denied": 0, "timeout": 0,
                           "cancelled": 0, "error": 0, "invalid": 1, "aborted": 0},
            },
            cases=[
                {
                    "id": "999-bad-frontmatter",
                    "form": None,
                    "provider": None,
                    "exit_code": None,
                    "wall_ms": None,
                    "had_denial": False,
                    "denial_count": 0,
                    "status": "invalid",
                    "errors": ["frontmatter parse error: ..."],
                },
            ],
        )
        result = subprocess.run(
            ["python3", str(RENDER), str(run_dir)],
            capture_output=True, text=True, check=False,
        )
        self.assertEqual(result.returncode, 0, f"stderr: {result.stderr}")
        index = (run_dir / "index.html").read_text()
        self.assertIn("999-bad-frontmatter", index)
        self.assertIn("invalid", index)
        # Should not crash on None fields
        self.assertNotIn("None", index)

    def test_html_escapes_user_strings(self):
        """Make sure malicious case ids / descriptions don't break HTML."""
        run_dir = make_run(
            self.tmp_path,
            summary={
                "started_at": "2026-04-07T12:00:00Z",
                "ended_at": "2026-04-07T12:00:01Z",
                "status": "done",
                "atomcode": {
                    "version_string": "<script>alert(1)</script>",
                    "binary_path": "/tmp/x",
                    "binary_sha256": "ff",
                    "binary_mtime": "2026-04-07T11:00:00Z",
                },
                "runner_version": "v",
                "case_count": 1,
                "concurrency": 1,
                "totals": {"pass": 0, "fail": 0, "denied": 0, "timeout": 0,
                           "cancelled": 0, "error": 0, "invalid": 0, "aborted": 1},
            },
            cases=[
                {
                    "id": "001-evil",
                    "form": "A",
                    "provider": "<img src=x onerror=alert(2)>",
                    "exit_code": 0,
                    "wall_ms": 100,
                    "had_denial": False,
                    "denial_count": 0,
                    "status": "aborted",
                },
            ],
        )
        subprocess.run(
            ["python3", str(RENDER), str(run_dir)],
            capture_output=True, text=True, check=False,
        )
        index = (run_dir / "index.html").read_text()
        # Header field escaping
        self.assertNotIn("<script>alert(1)</script>", index)
        self.assertIn("&lt;script&gt;", index)
        # Row field escaping (provider)
        self.assertNotIn("<img src=x onerror=alert(2)>", index)
        self.assertIn("&lt;img", index)


    def test_case_detail_page_aggregates_artifacts(self):
        """case.html should surface prompt, stdout, cwd file list, turn count
        from home/logs/, status badge, and a back link to ../index.html."""
        run_dir = make_run(
            self.tmp_path,
            summary={
                "started_at": "2026-04-07T12:00:00Z",
                "ended_at": "2026-04-07T12:00:30Z",
                "status": "done",
                "atomcode": {
                    "version_string": "atomcode 2.5.0 (abc1234)",
                    "binary_path": "/tmp/atomcode",
                    "binary_sha256": "deadbeef",
                    "binary_mtime": "2026-04-07T11:00:00Z",
                },
                "runner_version": "test",
                "case_count": 1,
                "concurrency": 1,
                "totals": {"pass": 1, "fail": 0, "denied": 0, "timeout": 0,
                           "cancelled": 0, "error": 0, "invalid": 0, "aborted": 0},
            },
            cases=[
                {
                    "id": "001-detail",
                    "form": "A",
                    "provider": "siliconflow",
                    "exit_code": 0,
                    "wall_ms": 12345,
                    "had_denial": False,
                    "denial_count": 0,
                    "status": "pass",
                    "started_at": "2026-04-07T12:00:00.123Z",
                    "ended_at":   "2026-04-07T12:00:12.468Z",
                },
            ],
        )

        # Populate the case directory with realistic artifacts.
        case_dir = run_dir / "001-detail"
        (case_dir / "prompt.md").write_text(
            "+++\n"
            'id = "001-detail"\n'
            'description = "MARKER_DESCRIPTION_FOR_TEST"\n'
            "+++\n"
            "MARKER_PROMPT_BODY please do the thing\n",
            encoding="utf-8",
        )
        (case_dir / "stdout.txt").write_text("MARKER_STDOUT model final reply", encoding="utf-8")
        (case_dir / "stderr.txt").write_text(
            "[tool→ create_file ...]\n[tool← create_file OK 0ms]\n[tokens] prompt=100\n",
            encoding="utf-8",
        )

        # cwd: one real file + one inside a build dir that should be skipped
        cwd = case_dir / "cwd"
        cwd.mkdir(exist_ok=True)
        (cwd / "MARKER_CWD_FILE.py").write_text("print('ok')\n", encoding="utf-8")
        skipdir = cwd / "target" / "debug"
        skipdir.mkdir(parents=True)
        (skipdir / "huge_artifact.bin").write_text("x" * 5000, encoding="utf-8")

        # home/logs: 4 files = 2 realistic turn pairs. The per-turn renderer
        # parses these for the breakdown section, so populate them with
        # plausible data (estimated_tokens, duration_ms, tool_calls, text).
        logs = case_dir / "home" / "logs"
        logs.mkdir(parents=True)
        # Turn 1: 800 tokens in, 5s, calls create_file
        (logs / "2026-04-07_12-00-00_100.json").write_text(
            json.dumps({
                "estimated_tokens": 800,
                "model": "test", "step": 0, "messages": [], "tools": [],
            }), encoding="utf-8")
        (logs / "2026-04-07_12-00-05_200_response.json").write_text(
            json.dumps({
                "duration_ms": 5000, "text": "",
                "tool_calls": [{
                    "id": "tc1", "name": "create_file",
                    "arguments": '{"file_path":"output.py","content":"print(1)"}',
                }],
            }), encoding="utf-8")
        # Turn 2: 950 tokens in, 3s, MARKER_FINAL_REPLY text and no tool calls
        (logs / "2026-04-07_12-00-05_300.json").write_text(
            json.dumps({
                "estimated_tokens": 950,
                "model": "test", "step": 1, "messages": [], "tools": [],
            }), encoding="utf-8")
        (logs / "2026-04-07_12-00-08_400_response.json").write_text(
            json.dumps({
                "duration_ms": 3000,
                "text": "MARKER_FINAL_REPLY ok done",
                "tool_calls": [],
            }), encoding="utf-8")

        result = subprocess.run(
            ["python3", str(RENDER), str(run_dir)],
            capture_output=True, text=True, check=False,
        )
        self.assertEqual(result.returncode, 0, f"stderr: {result.stderr}")

        case_html_path = case_dir / "case.html"
        self.assertTrue(case_html_path.exists(), "case.html was not generated")
        detail = case_html_path.read_text(encoding="utf-8")

        # Header / metadata
        self.assertIn("001-detail", detail)
        self.assertIn("siliconflow", detail)
        self.assertIn("MARKER_DESCRIPTION_FOR_TEST", detail)
        self.assertIn("12.3s", detail)  # wall time formatted
        self.assertIn("pass", detail)   # status badge

        # Prompt body inlined
        self.assertIn("MARKER_PROMPT_BODY", detail)

        # Stdout inlined
        self.assertIn("MARKER_STDOUT", detail)

        # Cwd file list — real file shown, build artifact skipped
        self.assertIn("MARKER_CWD_FILE.py", detail)
        self.assertNotIn("huge_artifact.bin", detail)

        # Stderr tail captured
        self.assertIn("create_file", detail)

        # Turn count = 2 (2 request/response pairs in home/logs/)
        self.assertIn("turns", detail)

        # Per-turn breakdown section — both turns visible with their data
        self.assertIn("Per-turn breakdown", detail)
        self.assertIn("Turn 1", detail)
        self.assertIn("Turn 2", detail)
        # Turn 1: prompt=800, duration 5.0s, calls create_file
        self.assertIn("800", detail)  # prompt token count
        self.assertIn("5.0s", detail)  # duration formatted
        # Turn 2: prompt=950, duration 3.0s, has MARKER_FINAL_REPLY text
        self.assertIn("950", detail)
        self.assertIn("3.0s", detail)
        self.assertIn("MARKER_FINAL_REPLY", detail)
        # Tool args compact format: file_path=output.py inline
        self.assertIn("file_path=output.py", detail)

        # Back link to the index
        self.assertIn("../index.html", detail)

        # Raw artifact links present
        self.assertIn('href="./prompt.md"', detail)
        self.assertIn('href="./stdout.txt"', detail)
        self.assertIn('href="./meta.json"', detail)
        self.assertIn('href="./cwd/"', detail)
        self.assertIn('href="./home/logs/"', detail)


class TestRenderSwebench(unittest.TestCase):
    """Tests for the form=swebench rendering branch."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.tmp_path = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_renders_swebench_row_in_index(self):
        run_dir = make_run(
            self.tmp_path,
            summary={
                "started_at": "2026-04-08T10:00:00Z",
                "ended_at": "2026-04-08T10:30:00Z",
                "status": "done",
                "atomcode": {
                    "version_string": "atomcode 2.5.0 (abc1234)",
                    "binary_path": "/tmp/atomcode",
                    "binary_sha256": "deadbeef",
                    "binary_mtime": "2026-04-08T09:00:00Z",
                },
                "runner_version": "eval/swebench/run.sh",
                "case_count": 1,
                "concurrency": 1,
                "totals": {"pass": 0, "fail": 0, "denied": 0, "timeout": 0,
                           "cancelled": 0, "error": 0, "invalid": 0, "aborted": 0,
                           "predicted": 1},
            },
            cases=[],
        )
        # Write a swebench case dir manually
        case_dir = run_dir / "sympy__sympy-20590"
        case_dir.mkdir()
        (case_dir / "meta.json").write_text(json.dumps({
            "id": "sympy__sympy-20590",
            "form": "swebench",
            "provider": "siliconflow",
            "exit_code": 0,
            "wall_ms": 127000,
            "timed_out": False,
            "had_denial": False,
            "denial_count": 0,
            "started_at": "2026-04-08T10:00:00Z",
            "ended_at":   "2026-04-08T10:02:07Z",
            "run_id": "2026-04-08_10-00-00",
            "status": "predicted",
            "swebench": {
                "repo": "sympy/sympy",
                "base_commit": "cffd4e0f86fefd4802349a9f9b19ed70934ea354",
                "prompt_template": "default",
                "include_hints": False,
                "dataset_revision": "abcdef",
                "patch_size_bytes": 1847,
            },
            "efficiency": {
                "turns": 14,
                "prompt_tokens": 32000,
                "completion_tokens": 2700,
                "tool_calls": 18,
                "tool_breakdown": {"read_file": 7, "grep": 4, "edit_file": 3},
                "stop_reason": "natural",
                "estimated_cost_usd": 0.082,
            },
        }))
        (case_dir / "patch.diff").write_text(
            "diff --git a/sympy/core/symbol.py b/sympy/core/symbol.py\n"
            "--- a/sympy/core/symbol.py\n"
            "+++ b/sympy/core/symbol.py\n"
            "@@ -10,6 +10,7 @@ class Symbol:\n"
            "+    __slots__ = ()\n"
            "     pass\n"
        )
        (case_dir / "stdout.txt").write_text("Fixed by adding __slots__.")
        (case_dir / "stderr.txt").write_text(
            "[tokens] prompt=32000 completion=2700\n"
            "[done] 127.0s tokens=34700 turns=14 tool_calls=18\n"
        )

        result = subprocess.run(
            ["python3", str(RENDER), str(run_dir)],
            capture_output=True, text=True, check=False,
        )
        self.assertEqual(result.returncode, 0, f"stderr: {result.stderr}")

        index = (run_dir / "index.html").read_text()
        self.assertIn("sympy__sympy-20590", index)
        self.assertIn("Predicted", index)      # status badge for ungraded instance
        self.assertIn("14 turns", index)       # efficiency badge
        self.assertIn("$0.08", index)          # cost badge

        case_html = (case_dir / "case.html").read_text()
        self.assertIn("sympy/sympy", case_html)
        self.assertIn("diff-add", case_html)   # patch.diff rendered with syntax coloring
        self.assertIn("__slots__", case_html)

    def test_resolved_instance_shows_green_badge(self):
        run_dir = make_run(
            self.tmp_path,
            summary={
                "started_at": "2026-04-08T10:00:00Z",
                "ended_at": "2026-04-08T11:00:00Z",
                "status": "done",
                "atomcode": {
                    "version_string": "atomcode 2.5.0",
                    "binary_path": "/tmp/atomcode",
                    "binary_sha256": "deadbeef",
                    "binary_mtime": "2026-04-08T09:00:00Z",
                },
                "runner_version": "swebench",
                "case_count": 1,
                "concurrency": 1,
                "totals": {"pass": 0, "fail": 0, "denied": 0, "timeout": 0,
                           "cancelled": 0, "error": 0, "invalid": 0, "aborted": 0,
                           "resolved": 1},
            },
            cases=[],
        )
        case_dir = run_dir / "sympy__sympy-1"
        case_dir.mkdir()
        (case_dir / "meta.json").write_text(json.dumps({
            "id": "sympy__sympy-1",
            "form": "swebench",
            "status": "resolved",
            "swebench_resolved": True,
            "swebench_failure_mode": None,
            "graded_at": "2026-04-08T11:00:00Z",
            "provider": "",
            "exit_code": 0,
            "wall_ms": 60000,
            "had_denial": False,
            "denial_count": 0,
            "started_at": "2026-04-08T10:00:00Z",
            "ended_at": "2026-04-08T10:01:00Z",
            "run_id": "run1",
            "timed_out": False,
            "swebench": {"repo":"sympy/sympy","base_commit":"abc","prompt_template":"default","include_hints":False,"dataset_revision":"","patch_size_bytes":100},
            "efficiency": {"turns":5,"prompt_tokens":10000,"completion_tokens":500,"tool_calls":5,"tool_breakdown":{},"stop_reason":"natural","estimated_cost_usd":0.01},
        }))
        (case_dir / "patch.diff").write_text("diff --git a b\n+add\n-del\n")
        (case_dir / "stdout.txt").write_text("ok")
        (case_dir / "stderr.txt").write_text("[done] 60s tokens=10500 turns=5 tool_calls=5\n")

        subprocess.run(["python3", str(RENDER), str(run_dir)], check=False)
        index = (run_dir / "index.html").read_text()
        self.assertIn("Resolved", index)
        self.assertIn("resolved", index.lower())


if __name__ == "__main__":
    unittest.main()
