#!/usr/bin/env python3
"""Report installed executable bytes, comparing only matching platform targets.

Growth warnings are advisory: they never change a successful build into a failure.
The updater manifest is read-only and its schema is unchanged.
"""
import argparse
import json
import os
from pathlib import Path
import re

TARGETS = r"darwin-arm64|darwin-x64|linux-arm64|linux-x64|ohos-arm64|windows-arm64|windows-x64"
ARTIFACT = re.compile(rf"^atomcode-v[0-9].*-({TARGETS})(\.exe)?$")


def report(artifacts, baseline, warn_percent=5.0, warn_bytes=1024 * 1024):
    records = []
    for path in sorted(Path(artifacts).iterdir()):
        match = ARTIFACT.fullmatch(path.name)
        if not path.is_file() or not match:
            continue
        target = match[1]
        if target.startswith('windows') != bool(match[2]):
            continue
        size = path.stat().st_size
        prior = baseline.get('binaries', {}).get(target, {}).get('size')
        if isinstance(prior, bool) or not isinstance(prior, int) or prior <= 0:
            prior = None
        delta = size - prior if prior is not None else None
        percent = delta / prior * 100 if prior is not None else None
        records.append(dict(target=target, file=path.name, bytes=size,
                            baseline_bytes=prior, delta_bytes=delta, delta_percent=percent,
                            growth_warning=delta is not None and delta > 0 and
                            (delta >= warn_bytes or percent >= warn_percent)))
    return dict(baseline_version=baseline.get('version'),
                thresholds=dict(percent=warn_percent, bytes=warn_bytes), artifacts=records)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--artifacts', type=Path, required=True)
    parser.add_argument('--baseline', type=Path, default=Path('latest.json'))
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--warn-percent', type=float, default=5)
    parser.add_argument('--warn-bytes', type=int, default=1024 * 1024)
    args = parser.parse_args()
    if not args.artifacts.is_dir():
        parser.error('artifacts directory does not exist')
    if not 0 < args.warn_percent < float('inf') or args.warn_bytes <= 0:
        parser.error('warning thresholds must be finite and positive')
    baseline = {}
    baseline_warning = None
    try:
        baseline = json.loads(args.baseline.read_text(encoding='utf-8'))
        if not isinstance(baseline, dict) or not isinstance(baseline.get('binaries', {}), dict):
            raise ValueError('baseline must contain a binaries object')
        if any(not isinstance(entry, dict) for entry in baseline.get('binaries', {}).values()):
            raise ValueError('baseline binary entries must be objects')
    except (OSError, ValueError) as error:
        baseline_warning = f'Baseline unavailable: {error}'
        baseline = {}
    result = report(args.artifacts, baseline, args.warn_percent, args.warn_bytes)
    result['baseline_warning'] = baseline_warning
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + '\n', encoding='utf-8')
    if baseline_warning:
        print(f'WARNING: {baseline_warning}')
    for entry in result['artifacts']:
        change = (f"{entry['delta_bytes']:+,} bytes ({entry['delta_percent']:+.2f}%)"
                  if entry['baseline_bytes'] is not None else 'no same-target baseline')
        print(f"{entry['target']}: {entry['bytes']:,} bytes; {change}")
        if entry['growth_warning']:
            message = f"{entry['target']} executable size grew: {change}"
            print(f'WARNING: {message}')
            if os.environ.get('GITHUB_ACTIONS') == 'true':
                print(f'::warning::{message}')
    if not result['artifacts']:
        print('WARNING: No CLI release artifacts found')
    print(f'Size report: {args.output}')


if __name__ == '__main__':
    main()
