#!/usr/bin/env python3
"""Build an optional local GUI acceptance target. Does not launch it or grant permissions."""
import argparse
import json
from pathlib import Path
import plistlib
import subprocess
import tempfile


def build(swift, output):
    output = output.parent.resolve(strict=True) / output.name
    if output.suffix != ".app" or output.exists() or output.is_symlink():
        raise ValueError("provide a new .app output; existing files are never replaced")
    source = Path(__file__).with_name("gui-fixture") / "Fixture.swift"
    sdk = subprocess.check_output(["/usr/bin/xcrun", "--sdk", "macosx", "--show-sdk-path"], text=True).strip()
    with tempfile.TemporaryDirectory(prefix=".ac-fixture-", dir=output.parent) as work:
        stage = Path(work) / output.name
        contents = stage / "Contents"
        binary = contents / "MacOS/AtomCodeCUFixture"
        binary.parent.mkdir(parents=True)
        subprocess.run([str(swift), "-sdk", sdk, "-swift-version", "6", "-parse-as-library",
                        str(source), "-o", str(binary)], check=True)
        info = dict(CFBundleIdentifier="dev.atomcode.computer-use-fixture", CFBundleExecutable=binary.name,
                    CFBundleName="AtomCode CU Fixture", CFBundleDisplayName="AtomCode CU Fixture",
                    CFBundlePackageType="APPL", CFBundleVersion="1", LSMinimumSystemVersion="14.0",
                    NSHighResolutionCapable=True, NSPrincipalClass="NSApplication")
        (contents / "Info.plist").write_bytes(plistlib.dumps(info))
        subprocess.run(["/usr/bin/codesign", "--force", "--sign", "-", "--timestamp=none", str(stage)], check=True)
        output.mkdir()
        try:
            contents.rename(output / "Contents")
        except BaseException:
            output.rmdir()
            raise
    subprocess.run(["/usr/bin/codesign", "--verify", "--deep", "--strict", str(output)], check=True)
    print(json.dumps(dict(fixture=str(output), launched=False, gui_actions=0)))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--swiftc", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    build(args.swiftc, args.output)
