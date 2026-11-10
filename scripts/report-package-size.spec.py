import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name('report-package-size.py')
spec = importlib.util.spec_from_file_location('package_size', SCRIPT)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class PackageSizeTests(unittest.TestCase):
    def test_only_same_target_executables_are_compared(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for name in ['atomcode-v5.2.7-linux-x64', 'atomcode-v5.2.7-windows-x64.exe',
                         'atomcode-daemon-v5.2.7-linux-x64', 'package-size.json',
                         'atomcode-v5.2.7-windows-arm64', 'atomcode-v5.2.7-linux-x64.exe']:
                (root / name).write_bytes(b'x' * 150)
            baseline = dict(version='v5.2.6', binaries={'linux-x64': dict(size=100)})
            result = module.report(root, baseline, 5, 1000)
            self.assertEqual([entry['target'] for entry in result['artifacts']], ['linux-x64', 'windows-x64'])
            linux, windows = result['artifacts']
            self.assertEqual((linux['delta_bytes'], linux['delta_percent']), (50, 50))
            self.assertTrue(linux['growth_warning'])
            self.assertIsNone(windows['delta_bytes'])
            self.assertFalse(windows['growth_warning'])

    def test_shrinkage_and_invalid_size_baselines_do_not_warn(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'atomcode-v5.2.7-darwin-arm64').write_bytes(b'xx')
            for prior in [3, 0, -1, True, '3', None]:
                entry = module.report(root, dict(binaries={'darwin-arm64': dict(size=prior)}))['artifacts'][0]
                self.assertFalse(entry['growth_warning'])

    def test_absolute_growth_also_warns(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'atomcode-v5.2.7-linux-x64').write_bytes(b'x' * 1010)
            entry = module.report(root, dict(binaries={'linux-x64': dict(size=1000)}), 5, 10)['artifacts'][0]
            self.assertTrue(entry['growth_warning'])

    def test_cli_missing_or_malformed_baseline_is_advisory(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'atomcode-v5.2.7-linux-x64').write_bytes(b'xxx')
            baseline = root / 'baseline.json'
            output = root / 'sizes.json'
            for value in [None, 'bad json', '[]', '{"binaries":{"linux-x64":null}}']:
                if value is not None:
                    baseline.write_text(value)
                run = subprocess.run([sys.executable, str(SCRIPT), '--artifacts', str(root),
                                      '--baseline', str(baseline), '--output', str(output)], capture_output=True, text=True)
                self.assertEqual(run.returncode, 0, run.stderr)
                self.assertIn('Baseline unavailable', run.stdout)
                self.assertEqual(len(json.loads(output.read_text())['artifacts']), 1)

    def test_cli_growth_warning_is_non_blocking(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'atomcode-v5.2.7-linux-x64').write_bytes(b'x' * 110)
            (root / 'baseline.json').write_text('{"binaries":{"linux-x64":{"size":100}}}')
            run = subprocess.run([sys.executable, str(SCRIPT), '--artifacts', str(root),
                                  '--baseline', str(root / 'baseline.json'), '--output', str(root / 'out.json')], capture_output=True, text=True)
            self.assertEqual(run.returncode, 0, run.stderr)
            self.assertIn('WARNING: linux-x64 executable size grew', run.stdout)


if __name__ == '__main__':
    unittest.main()
