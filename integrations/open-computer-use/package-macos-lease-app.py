#!/usr/bin/env python3
"""Package an explicitly built isolated development App; never install or grant TCC."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import subprocess
import tempfile

PIN = "e551c13e7082ce4c097234137582fbf4a1bc647a"
BUNDLE_ID = "com.ifuryst.opencomputeruse.dev"


def require(ok, message):
    if not ok:
        raise ValueError(message)


def probe(binary):
    env = dict(os.environ, OPEN_COMPUTER_USE_DISABLE_APP_AGENT_PROXY="1")
    result = subprocess.run([str(binary), "doctor", "--no-ui"], env=env,
                            capture_output=True, text=True, timeout=10, check=True)
    report = json.loads(result.stdout)
    require(report.get("kind") == "doctor" and report.get("ui") is False, "expected a silent doctor report")
    require(type(report.get("schema_version")) is int and report["schema_version"] == 1, "unsupported diagnostic schema")
    for capability in ["desktop_turn_lease", "app_agent_isolation"]:
        version = report.get("capabilities", {}).get(capability, {}).get("version")
        require(type(version) is int and version == 1, "binary does not support " + capability)
    return report


def package(backend, output, report_path, manifest_path):
    backend = backend.resolve(strict=True)
    require(backend.is_file() and os.access(backend, os.X_OK), "provide an explicitly built executable")
    require(output.suffix == ".app", "output must be an App bundle")
    parent = output.parent.resolve(strict=True)
    output = parent / output.name
    require(not os.path.lexists(output), "output already exists; nothing was replaced")
    require(not os.path.lexists(report_path), "provenance output already exists")
    patch = Path(__file__).with_name("desktop-turn-lease.patch")
    patch_digest = hashlib.sha256(patch.read_bytes()).hexdigest()
    report_path = report_path.parent.resolve(strict=True) / report_path.name
    require(output not in report_path.parents, "provenance must be outside the signed App")
    manifest_bytes = manifest_path.read_bytes()
    manifest = json.loads(manifest_bytes)
    require(type(manifest.get("schema_version")) is int and manifest["schema_version"] == 1, "unsupported build manifest")
    require(manifest.get("source_patch_sha256") == patch_digest and manifest.get("upstream_commit") == PIN, "build manifest does not match the current pinned source patch")
    payload = backend.read_bytes()
    input_digest = hashlib.sha256(payload).hexdigest()
    require(manifest.get("executable_sha256") == input_digest, "binary does not match its measured build manifest")
    manifest_digest = hashlib.sha256(manifest_bytes).hexdigest()
    # Validate the original before creating any output. A legacy backend must
    # never receive the isolation marker through packaging alone.
    probe(backend)
    with tempfile.TemporaryDirectory(prefix=".atomcode-lease-package-", dir=parent) as work:
        stage = Path(work) / output.name
        contents = stage / "Contents"
        binary = contents / "MacOS" / "OpenComputerUse"
        binary.parent.mkdir(parents=True)
        binary.write_bytes(payload)
        binary.chmod(0o755)
        info = dict(CFBundleIdentifier=BUNDLE_ID, CFBundleExecutable="OpenComputerUse",
                    CFBundleName="Open Computer Use (AtomCode Lease)",
                    CFBundleDisplayName="Open Computer Use (AtomCode Lease)",
                    CFBundlePackageType="APPL", CFBundleInfoDictionaryVersion="6.0",
                    CFBundleShortVersionString="1.3.0", CFBundleVersion="1",
                    OpenComputerUseAppVariant="development", LSMinimumSystemVersion="14.0",
                    LSUIElement=True, NSHighResolutionCapable=True, NSPrincipalClass="NSApplication",
                    AtomCodeDesktopLeaseProtocol="1", AtomCodeUpstreamCommit=PIN,
                    AtomCodeSourcePatchSHA256=patch_digest, AtomCodeBuildManifestSHA256=manifest_digest)
        (contents / "Info.plist").write_bytes(plistlib.dumps(info))
        # Probe the exact copied payload too, closing the probe/copy race.
        probe(binary)
        subprocess.run(["/usr/bin/codesign", "--force", "--sign", "-", "--timestamp=none",
                        "--identifier", BUNDLE_ID, str(stage)], check=True, capture_output=True)
        subprocess.run(["/usr/bin/codesign", "--verify", "--deep", "--strict", str(stage)],
                       check=True, capture_output=True)
        diagnostic = probe(binary)
        require(diagnostic.get("runtime", {}).get("bundle_identifier") == BUNDLE_ID, "unexpected packaged bundle identity")
        require(Path(diagnostic["runtime"]["permission_app"]).resolve() == stage.resolve(), "permission discovery selected another App")
        final_digest = hashlib.sha256(binary.read_bytes()).hexdigest()
        provenance = dict(app=str(output), bundle_identifier=BUNDLE_ID, signing="ad-hoc development",
                          upstream_commit=PIN, source_patch_sha256=patch_digest,
                          executable_sha256=final_digest, input_executable_sha256=input_digest,
                          build_manifest_sha256=manifest_digest, gui_actions=0, installed=False,
                          permissions_granted=False)
        # Reserve the destination atomically. An existing/competing bundle is
        # never replaced, even if it appears after the initial existence check.
        output.mkdir()
        try:
            os.rename(contents, output / "Contents")
        except BaseException:
            output.rmdir()  # Only our still-empty reservation, never recursive.
            raise
        subprocess.run(["/usr/bin/codesign", "--verify", "--deep", "--strict", str(output)],
                       check=True, capture_output=True)
        with report_path.open("x") as file:
            json.dump(provenance, file, indent=2)
            file.write("\n")
    return provenance


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backend", type=Path, required=True)
    parser.add_argument("--build-provenance", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = package(args.backend, args.output, args.report, args.build_provenance)
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        parser.exit(1, f"Packaging failed; no installed App or permissions were changed: {error}\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
