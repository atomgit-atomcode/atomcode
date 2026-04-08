"""Unit tests for eval/swebench/pricing.py."""
import subprocess
import sys
import unittest
from pathlib import Path

PRICING = Path(__file__).resolve().parent.parent / "pricing.py"


def run_cli(*args: str) -> tuple[int, str, str]:
    """Invoke pricing.py as a subprocess. Returns (exit_code, stdout, stderr)."""
    result = subprocess.run(
        [sys.executable, str(PRICING), *args],
        capture_output=True, text=True, check=False,
    )
    return result.returncode, result.stdout.strip(), result.stderr.strip()


class TestPricing(unittest.TestCase):
    def test_known_provider_siliconflow(self):
        """siliconflow: 0.14 prompt + 0.28 completion per 1M tokens."""
        # 1M prompt + 1M completion = 0.14 + 0.28 = 0.42 USD
        code, out, err = run_cli("siliconflow", "1000000", "1000000")
        self.assertEqual(code, 0, f"stderr: {err}")
        self.assertEqual(out, "0.420000")

    def test_anthropic_sonnet(self):
        """anthropic-sonnet-4-6: 3.00 prompt + 15.00 completion per 1M."""
        # 10k prompt + 1k completion = 0.03 + 0.015 = 0.045 USD
        code, out, _ = run_cli("anthropic-sonnet-4-6", "10000", "1000")
        self.assertEqual(code, 0)
        self.assertEqual(out, "0.045000")

    def test_zero_tokens(self):
        code, out, _ = run_cli("siliconflow", "0", "0")
        self.assertEqual(code, 0)
        self.assertEqual(out, "0.000000")

    def test_unknown_provider_falls_back_with_warning(self):
        """Unknown providers fall back to siliconflow pricing with a stderr warning."""
        code, out, err = run_cli("no-such-provider-xyz", "1000000", "0")
        self.assertEqual(code, 0)
        self.assertEqual(out, "0.140000")
        self.assertIn("no-such-provider-xyz", err)
        self.assertIn("falling back", err.lower())

    def test_invalid_token_count_errors(self):
        code, out, err = run_cli("siliconflow", "not-a-number", "0")
        self.assertNotEqual(code, 0)
        self.assertIn("invalid", err.lower())


if __name__ == "__main__":
    unittest.main()
