"""Unit tests for parse_case.py.

Uses python stdlib unittest only — no pytest dependency.
Run: python3 -m unittest scripts.eval.tests.test_parse_case
"""
import json
import subprocess
import tempfile
import unittest
from pathlib import Path

PARSE_CASE = Path(__file__).resolve().parent.parent / "parse_case.py"


def run_parse(case_path: Path) -> dict:
    """Invoke parse_case.py as a subprocess and decode its JSON stdout."""
    result = subprocess.run(
        ["python3", str(PARSE_CASE), str(case_path)],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"parse_case.py crashed: rc={result.returncode}\nstderr={result.stderr}"
        )
    return json.loads(result.stdout)


class TestFormA(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.cases_dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_minimal_form_a(self):
        f = self.cases_dir / "001-mini.md"
        f.write_text(
            "+++\n"
            'id = "001-mini"\n'
            'provider = "kimi"\n'
            "+++\n"
            "Write hello world.\n"
        )
        result = run_parse(f)
        self.assertTrue(result["ok"])
        self.assertEqual(result["id"], "001-mini")
        self.assertEqual(result["form"], "A")
        self.assertEqual(result["provider"], "kimi")
        self.assertEqual(result["timeout_secs"], 120)  # default
        self.assertEqual(result["tags"], [])
        self.assertEqual(result["seed_files"], {})
        self.assertEqual(result["prompt_body"].strip(), "Write hello world.")

    def test_form_a_with_optional_fields(self):
        f = self.cases_dir / "002-full.md"
        f.write_text(
            "+++\n"
            'id = "002-full"\n'
            'provider = "kimi"\n'
            'description = "fuller example"\n'
            "timeout_secs = 60\n"
            'tags = ["code-gen", "smoke"]\n'
            "\n"
            "[seed_files]\n"
            '"hint.txt" = "useful hint"\n'
            "+++\n"
            "Do the thing.\n"
        )
        result = run_parse(f)
        self.assertTrue(result["ok"])
        self.assertEqual(result["timeout_secs"], 60)
        self.assertEqual(result["tags"], ["code-gen", "smoke"])
        self.assertEqual(result["seed_files"], {"hint.txt": "useful hint"})

    def test_missing_id_is_invalid(self):
        f = self.cases_dir / "no-id.md"
        f.write_text(
            "+++\n"
            'provider = "kimi"\n'
            "+++\n"
            "body\n"
        )
        result = run_parse(f)
        self.assertFalse(result["ok"])
        self.assertTrue(any("id" in e for e in result["errors"]))

    def test_missing_provider_is_valid_defaults_to_empty(self):
        """Provider is optional — missing field means 'use config's default_provider'
        when the runner invokes atomcode. parse_case.py emits provider='' in that case."""
        f = self.cases_dir / "no-provider.md"
        f.write_text(
            "+++\n"
            'id = "no-provider"\n'
            "+++\n"
            "body\n"
        )
        result = run_parse(f)
        self.assertTrue(result["ok"])
        self.assertEqual(result["provider"], "")

    def test_empty_provider_string_is_invalid(self):
        """Explicit provider = '' is ambiguous — reject it so authors use
        absence-of-field instead to mean 'default provider'."""
        f = self.cases_dir / "empty-provider.md"
        f.write_text(
            "+++\n"
            'id = "empty-provider"\n'
            'provider = ""\n'
            "+++\n"
            "body\n"
        )
        result = run_parse(f)
        self.assertFalse(result["ok"])
        self.assertTrue(any("provider" in e for e in result["errors"]))

    def test_id_must_match_filename(self):
        f = self.cases_dir / "001-foo.md"
        f.write_text(
            "+++\n"
            'id = "002-bar"\n'
            'provider = "kimi"\n'
            "+++\n"
            "body\n"
        )
        result = run_parse(f)
        self.assertFalse(result["ok"])
        self.assertTrue(any("filename" in e or "match" in e for e in result["errors"]))

    def test_id_charset_validation(self):
        f = self.cases_dir / "bad id.md"
        f.write_text(
            "+++\n"
            'id = "bad id"\n'
            'provider = "kimi"\n'
            "+++\n"
            "body\n"
        )
        result = run_parse(f)
        self.assertFalse(result["ok"])
        self.assertTrue(any("charset" in e or "alpha" in e for e in result["errors"]))

    def test_seed_files_path_must_be_relative(self):
        f = self.cases_dir / "003-bad-seed.md"
        f.write_text(
            "+++\n"
            'id = "003-bad-seed"\n'
            'provider = "kimi"\n'
            "\n"
            "[seed_files]\n"
            '"/etc/passwd" = "nope"\n'
            "+++\n"
            "body\n"
        )
        result = run_parse(f)
        self.assertFalse(result["ok"])
        self.assertTrue(any("path" in e or "relative" in e for e in result["errors"]))

    def test_seed_files_no_parent_traversal(self):
        f = self.cases_dir / "004-traversal.md"
        f.write_text(
            "+++\n"
            'id = "004-traversal"\n'
            'provider = "kimi"\n'
            "\n"
            "[seed_files]\n"
            '"../escape.txt" = "nope"\n'
            "+++\n"
            "body\n"
        )
        result = run_parse(f)
        self.assertFalse(result["ok"])
        self.assertTrue(any(".." in e or "traversal" in e for e in result["errors"]))

    def test_no_frontmatter_is_invalid(self):
        f = self.cases_dir / "005-noframe.md"
        f.write_text("just body, no frontmatter\n")
        result = run_parse(f)
        self.assertFalse(result["ok"])
        self.assertTrue(any("frontmatter" in e for e in result["errors"]))

    def test_unclosed_frontmatter_is_invalid(self):
        f = self.cases_dir / "006-unclosed.md"
        f.write_text("+++\nid = \"006-unclosed\"\nprovider = \"kimi\"\nbody never closes\n")
        result = run_parse(f)
        self.assertFalse(result["ok"])
        self.assertTrue(any("frontmatter" in e or "closing" in e for e in result["errors"]))


class TestFormB(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.cases_dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_minimal_form_b(self):
        case_dir = self.cases_dir / "010-refactor"
        case_dir.mkdir()
        (case_dir / "case.md").write_text(
            "+++\n"
            'id = "010-refactor"\n'
            'provider = "kimi"\n'
            "+++\n"
            "Refactor the thing.\n"
        )
        result = run_parse(case_dir / "case.md")
        self.assertTrue(result["ok"])
        self.assertEqual(result["id"], "010-refactor")
        self.assertEqual(result["form"], "B")
        self.assertIsNone(result["seed_dir"])

    def test_form_b_with_seed_dir(self):
        case_dir = self.cases_dir / "011-with-seed"
        case_dir.mkdir()
        seed = case_dir / "seed"
        seed.mkdir()
        (seed / "Cargo.toml").write_text("[package]\nname = 'demo'\n")
        (case_dir / "case.md").write_text(
            "+++\n"
            'id = "011-with-seed"\n'
            'provider = "kimi"\n'
            "+++\n"
            "Use the seed.\n"
        )
        result = run_parse(case_dir / "case.md")
        self.assertTrue(result["ok"])
        self.assertEqual(result["form"], "B")
        self.assertEqual(result["seed_dir"], str(seed))

    def test_form_b_seed_files_inline_is_invalid(self):
        case_dir = self.cases_dir / "012-mixed"
        case_dir.mkdir()
        (case_dir / "case.md").write_text(
            "+++\n"
            'id = "012-mixed"\n'
            'provider = "kimi"\n'
            "\n"
            "[seed_files]\n"
            '"inline.txt" = "should not allow"\n'
            "+++\n"
            "body\n"
        )
        result = run_parse(case_dir / "case.md")
        self.assertFalse(result["ok"])
        self.assertTrue(any("form B" in e or "seed_files" in e for e in result["errors"]))

    def test_form_b_id_matches_dirname(self):
        case_dir = self.cases_dir / "013-correct"
        case_dir.mkdir()
        (case_dir / "case.md").write_text(
            "+++\n"
            'id = "013-wrong"\n'
            'provider = "kimi"\n'
            "+++\n"
            "body\n"
        )
        result = run_parse(case_dir / "case.md")
        self.assertFalse(result["ok"])
        self.assertTrue(any("dirname" in e or "match" in e for e in result["errors"]))


if __name__ == "__main__":
    unittest.main()
