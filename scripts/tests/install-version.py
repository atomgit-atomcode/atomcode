"""Exercise the installer's real version resolver with mocked network responses."""
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SOURCE = (ROOT / "scripts/install.sh").read_text()
CONSTANTS = "\n".join(line for line in SOURCE.splitlines() if line.startswith("REPO_"))
RESOLVER = SOURCE[SOURCE.index("release_tag() {"):SOURCE.index('\nBIN_NAME=')]


def working_bash():
    """A `bash` that actually runs, or None.

    Windows ships a WSL launcher named `bash.exe`, so `shutil.which` alone is not an
    answer: with no distribution installed it exits non-zero before running anything and
    the failure reads as an installer bug. The shell that matters is one that runs.
    """
    found = shutil.which("bash")
    if not found:
        return None
    try:
        probe = subprocess.run([found, "-c", "exit 0"], capture_output=True)
    except OSError:
        return None
    return found if probe.returncode == 0 else None


BASH = working_bash()


class InstallerVersion(unittest.TestCase):
    def resolve(self, api, manifest, override=None, api_status=0, manifest_status=0, shell=("sh",)):
        with tempfile.TemporaryDirectory() as directory:
            calls = Path(directory) / "calls"
            code = f'''set -eu
{CONSTANTS}
_fetch=fake_fetch
fake_fetch() {{
    printf '%s\\n' "$1" >> {shlex.quote(str(calls))}
    if [ "$1" = "$REPO_LATEST_API" ]; then
        printf '%s' {shlex.quote(api)}
        return {api_status}
    fi
    printf '%s' {shlex.quote(manifest)}
    return {manifest_status}
}}
{RESOLVER}
printf 'RESULT=%s\\n' "$VERSION"
'''
            env = os.environ.copy()
            env.pop("ATOMCODE_VERSION", None)
            if override is not None:
                env["ATOMCODE_VERSION"] = override
            result = subprocess.run([*shell, "-c", code], env=env, capture_output=True, text=True)
            requests = calls.read_text().splitlines() if calls.exists() else []
            return result, requests

    def test_explicit_version_skips_network(self):
        result, calls = self.resolve("", "", override="v9.8.7")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("RESULT=v9.8.7", result.stdout)
        self.assertEqual(calls, [])

    def test_api_success_skips_manifest(self):
        result, calls = self.resolve('{"tag_name":"v9.8.6-beta.1+build.2"}', '{"version":"v1.2.3"}')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("RESULT=v9.8.6-beta.1+build.2", result.stdout)
        self.assertEqual(len(calls), 1)

    def test_failed_missing_or_invalid_api_uses_dynamic_manifest(self):
        for body, status in [("", 1), ('{"tag_name":"v1.2.3"}', 1), ('{}', 0), ('{"tag_name":"../../bad"}', 0)]:
            with self.subTest(body=body, status=status):
                result, calls = self.resolve(body, '{"version":"v9.8.5-beta.1"}', api_status=status)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("RESULT=v9.8.5-beta.1", result.stdout)
                self.assertEqual(len(calls), 2)

    def test_both_sources_unusable_fail_with_override_guidance(self):
        for body, status in [("", 1), ('{"version":"v1.2.3"}', 1), ('{}', 0), ('{"version":"broken"}', 0)]:
            with self.subTest(body=body, status=status):
                result, calls = self.resolve("", body, api_status=1, manifest_status=status)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("ATOMCODE_VERSION", result.stderr)
                self.assertNotIn("RESULT=", result.stdout)
                self.assertEqual(len(calls), 2)

    @unittest.skipUnless(BASH, "no working bash is available")
    def test_empty_api_metadata_still_falls_back_with_pipefail(self):
        result, calls = self.resolve('{}', '{"version":"v9.8.5"}', shell=(BASH, "-o", "pipefail"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("RESULT=v9.8.5", result.stdout)
        self.assertEqual(len(calls), 2)

    def test_no_pinned_fallback_in_either_installer(self):
        self.assertNotIn("DEFAULT_VERSION", SOURCE)
        self.assertNotIn("$DefaultVersion", (ROOT / "scripts/install.ps1").read_text())


if __name__ == "__main__":
    unittest.main()
