"""Unit tests for eval/swebench/render_prompt.py."""
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

RENDER = Path(__file__).resolve().parent.parent / "render_prompt.py"


def make_instance(**overrides) -> dict:
    """Default valid instance dict for tests."""
    base = {
        "instance_id": "sympy__sympy-20590",
        "repo": "sympy/sympy",
        "base_commit": "cffd4e0f86fefd4802349a9f9b19ed70934ea354",
        "problem_statement": "Symbol instances have __dict__ in 1.7+\n\nShould have __slots__.",
        "hints_text": "",
    }
    base.update(overrides)
    return base


def run_render(instance: dict, template: str = "default", include_hints: bool = False) -> tuple[int, str, str]:
    """Run render_prompt.py with instance JSON on stdin."""
    result = subprocess.run(
        [sys.executable, str(RENDER),
         "--template", template,
         "--include-hints" if include_hints else "--no-include-hints"],
        input=json.dumps(instance),
        capture_output=True, text=True, check=False,
    )
    return result.returncode, result.stdout, result.stderr


class TestRenderPrompt(unittest.TestCase):
    def test_basic_rendering_no_hints(self):
        """Default case: all placeholders substituted, hints_block empty."""
        code, out, err = run_render(make_instance(), include_hints=False)
        self.assertEqual(code, 0, f"stderr: {err}")
        self.assertIn("sympy/sympy", out)
        self.assertIn("cffd4e0f", out)         # short SHA
        self.assertIn("Symbol instances", out)  # problem statement
        self.assertNotIn("--- HINTS", out)      # no hints block
        self.assertNotIn("{", out)              # all placeholders resolved

    def test_include_hints_with_content(self):
        """include_hints=true + non-empty hints_text → hints block rendered."""
        inst = make_instance(hints_text="The bug is in symbol.py line 42.")
        code, out, _ = run_render(inst, include_hints=True)
        self.assertEqual(code, 0)
        self.assertIn("--- HINTS (developer comments from the original PR) ---", out)
        self.assertIn("The bug is in symbol.py line 42.", out)
        self.assertIn("--- END HINTS ---", out)

    def test_include_hints_but_empty_hints_text(self):
        """include_hints=true but hints_text is "" → still no hints block."""
        inst = make_instance(hints_text="")
        code, out, _ = run_render(inst, include_hints=True)
        self.assertEqual(code, 0)
        self.assertNotIn("--- HINTS", out)

    def test_include_hints_false_ignores_hints_text(self):
        """include_hints=false must drop hints even if hints_text is non-empty."""
        inst = make_instance(hints_text="SPOILER: the bug is in foo.py")
        code, out, _ = run_render(inst, include_hints=False)
        self.assertEqual(code, 0)
        self.assertNotIn("SPOILER", out)
        self.assertNotIn("--- HINTS", out)

    def test_base_commit_short_is_8_chars(self):
        inst = make_instance(base_commit="abcdef1234567890abcdef1234567890abcdef12")
        code, out, _ = run_render(inst)
        self.assertEqual(code, 0)
        self.assertIn("abcdef12", out)
        # Full SHA also appears once in the "Base commit:" line
        self.assertIn("abcdef1234567890abcdef1234567890abcdef12", out)

    def test_missing_required_field_errors(self):
        """Missing problem_statement → exit non-zero, clear error."""
        inst = make_instance()
        del inst["problem_statement"]
        code, out, err = run_render(inst)
        self.assertNotEqual(code, 0)
        self.assertTrue(
            "problem_statement" in err or "problem_statement" in out,
            f"expected error to name the missing field. stderr: {err}",
        )


if __name__ == "__main__":
    unittest.main()
