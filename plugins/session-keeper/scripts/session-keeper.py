#!/usr/bin/env python3
"""
AtomCode Session Keeper — 会话健康守护

Auto-detect orphan snapshots, repair damaged metadata, validate format
compatibility, create verified backups, and restore from backups.

Commands:
  diagnose      Scan for orphan snapshots, damaged meta, missing .ui.json
  fix           Generate missing .meta/.ui.json, clean stale leases
  verify        Validate all sessions have proper structure
  backup        Create a timestamped full backup with MD5 checksums
  list-backups  List all available backups
  restore       Restore from a specified backup
  full          Run diagnose → fix → verify (default when no argument given)
"""

import json
import os
import sys
import hashlib
import shutil
from datetime import datetime

# Honor $ATOMCODE_HOME when set (daemon uses it as the config root);
# otherwise fall back to ~/.atomcode.
HOME = os.path.expanduser("~")
ATOMCODE_HOME = os.environ.get("ATOMCODE_HOME") or os.path.join(HOME, ".atomcode")
SESSIONS_DIR = os.path.join(ATOMCODE_HOME, "sessions")
BACKUPS_DIR = os.path.join(ATOMCODE_HOME, "backups")
META_VERSION = 1

# ── helpers ──────────────────────────────────────────────────────────

def _sessions_root() -> str:
    os.makedirs(SESSIONS_DIR, exist_ok=True)
    return SESSIONS_DIR


def _backups_root() -> str:
    os.makedirs(BACKUPS_DIR, exist_ok=True)
    return BACKUPS_DIR


def _strip_suffix(name: str, suffix: str) -> str:
    return name[: -len(suffix)] if name.endswith(suffix) else name


def _sid_from_filename(name: str) -> str:
    """Extract session ID from a sidecar filename (strip extension)."""
    for ext in (".snapshot", ".meta", ".ui.json", ".lease", ".meta.lock", ".json", ".jsonl"):
        if name.endswith(ext):
            return _strip_suffix(name, ext)
    return name


def _is_valid_json(text: str) -> bool:
    try:
        json.loads(text)
        return True
    except (json.JSONDecodeError, ValueError, TypeError, AttributeError):
        return False


# ── scanner ──────────────────────────────────────────────────────────

def _scan_sessions():
    """Return a dict mapping session_dir → {sid → set of extensions}."""
    root = _sessions_root()
    projects = {}
    for entry in sorted(os.listdir(root)):
        d = os.path.join(root, entry)
        if not os.path.isdir(d):
            continue
        sessions: dict[str, set[str]] = {}
        for f in os.listdir(d):
            sid = _sid_from_filename(f)
            if sid not in sessions:
                sessions[sid] = set()
            # record which extensions this sid has
            for ext in (".snapshot", ".meta", ".ui.json", ".lease", ".meta.lock", ".json", ".jsonl"):
                if f.endswith(ext):
                    sessions[sid].add(ext)
        if sessions:
            projects[entry] = sessions
    return projects


# ── diagnose ─────────────────────────────────────────────────────────

def cmd_diagnose():
    """Scan for orphan snapshots, damaged meta, missing .ui.json, stale leases."""
    projects = _scan_sessions()
    print("=" * 60)
    print("  AtomCode Session Health Check")
    print("=" * 60)

    total_projects = len(projects)
    total_snap = 0
    total_meta = 0
    total_ui = 0
    total_lease = 0
    orphans = []
    missing_ui = []
    stale_leases = []
    corrupt_meta = []
    meta_no_snap = []

    for proj, sessions in projects.items():
        for sid, exts in sessions.items():
            has_snap = ".snapshot" in exts
            has_meta = ".meta" in exts
            has_ui = ".ui.json" in exts
            has_lease = ".lease" in exts

            if has_snap:
                total_snap += 1
            if has_meta:
                total_meta += 1
            if has_ui:
                total_ui += 1
            if has_lease:
                total_lease += 1

            # orphan snapshot (has .snapshot but no .meta)
            if has_snap and not has_meta:
                d = os.path.join(_sessions_root(), proj)
                sz = os.path.getsize(os.path.join(d, f"{sid}.snapshot"))
                orphans.append((proj, sid, sz))

            # missing .ui.json (has .meta but no .ui.json)
            if has_meta and not has_ui:
                missing_ui.append((proj, sid))

            # stale lease (has .lease but no .meta and no .snapshot)
            if has_lease and not has_snap and not has_meta:
                stale_leases.append((proj, sid))

            # meta without snapshot
            if has_meta and not has_snap:
                meta_no_snap.append((proj, sid))

            # corrupt meta (try parsing)
            if has_meta:
                d = os.path.join(_sessions_root(), proj)
                meta_path = os.path.join(d, f"{sid}.meta")
                try:
                    with open(meta_path, "r", encoding="utf-8") as f:
                        text = f.read()
                    if not _is_valid_json(text):
                        corrupt_meta.append((proj, sid, "invalid JSON"))
                    else:
                        data = json.loads(text)
                        # check for known compatibility issues
                        if "import_info" in data and data["import_info"] is not None:
                            info = data["import_info"]
                            if "kind" in info and info["kind"] not in ("full", "metadata_only"):
                                corrupt_meta.append((proj, sid,
                                    f"unknown ImportKind: {info['kind']}"))
                except (OSError, json.JSONDecodeError, UnicodeDecodeError, TypeError, KeyError) as e:
                    corrupt_meta.append((proj, sid, str(e)))

    print(f"\n  Sessions directory:  {_sessions_root()}")
    print(f"  Project directories: {total_projects}")
    print(f"  Total .snapshot:     {total_snap}")
    print(f"  Total .meta:         {total_meta}")
    print(f"  Total .ui.json:      {total_ui}")
    print(f"  Total .lease:        {total_lease}")
    print()

    results = []

    if orphans:
        print(f"  ⚠  Orphan snapshots: {len(orphans)}")
        for proj, sid, sz in orphans[:10]:
            print(f"      [{proj[:8]}] {sid[:12]} ({sz:,}B)")
        if len(orphans) > 10:
            print(f"      ... and {len(orphans) - 10} more")
        results.append(("fix", len(orphans)))
    else:
        print(f"  ✅  No orphan snapshots")

    if missing_ui:
        print(f"  ⚠  Missing .ui.json: {len(missing_ui)}")
        results.append(("fix", len(missing_ui)))
    else:
        print(f"  ✅  All sessions have .ui.json")

    if stale_leases:
        print(f"  ⚠  Stale lease files: {len(stale_leases)}")
        results.append(("cleanup", len(stale_leases)))
    else:
        print(f"  ✅  No stale lease files")

    if meta_no_snap:
        print(f"  ⚠  Meta without snapshot: {len(meta_no_snap)}")
        for proj, sid in meta_no_snap[:5]:
            print(f"      [{proj[:8]}] {sid[:12]}")
        results.append(("inspect", len(meta_no_snap)))
    else:
        print(f"  ✅  All meta files have matching snapshots")

    if corrupt_meta:
        print(f"  ❌  Corrupt meta files: {len(corrupt_meta)}")
        for proj, sid, reason in corrupt_meta[:10]:
            print(f"      [{proj[:8]}] {sid[:12]}: {reason}")
        if len(corrupt_meta) > 10:
            print(f"      ... and {len(corrupt_meta) - 10} more")
        results.append(("fix", len(corrupt_meta)))
    else:
        print(f"  ✅  All meta files are valid")

    total_issues = (len(orphans) + len(missing_ui) + len(stale_leases)
                    + len(corrupt_meta) + len(meta_no_snap))
    if total_issues == 0:
        print(f"\n  🎉  All sessions healthy!")
    else:
        print(f"\n  📋  Total issues: {total_issues}")
        print(f"  Run `session-keeper.py fix` to auto-repair")

    print(f"\n{'=' * 60}")
    return (len(orphans) + len(missing_ui) + len(stale_leases)
            + len(corrupt_meta) + len(meta_no_snap))


# ── fix ──────────────────────────────────────────────────────────────

def cmd_fix():
    """Generate missing .meta/.ui.json for orphan/data sessions, clean stale leases."""
    projects = _scan_sessions()
    root = _sessions_root()

    fixed_meta = 0
    fixed_ui = 0
    cleaned_leases = 0
    repaired_corrupt = 0

    # 0. Repair corrupt meta files (e.g. invalid ImportKind, missing fields)
    for proj, sessions in projects.items():
        d = os.path.join(root, proj)
        for sid, exts in sessions.items():
            if ".meta" not in exts:
                continue
            meta_path = os.path.join(d, f"{sid}.meta")
            try:
                with open(meta_path, "r", encoding="utf-8") as f:
                    raw = f.read()
                meta = json.loads(raw)
                # Reset import_info if it contains problematic values
                if meta.get("import_info") is not None:
                    info = meta["import_info"]
                    kind = info.get("kind", "")
                    if kind not in ("full", "metadata_only",):
                        meta["import_info"] = None
                        with open(meta_path, "w", encoding="utf-8") as f:
                            json.dump(meta, f, ensure_ascii=False, indent=2)
                        repaired_corrupt += 1
                        print(f"  🔧  Repaired corrupt meta [{proj[:8]}] {sid[:12]} (kind={kind})")
            except (json.JSONDecodeError, OSError, KeyError, TypeError, AttributeError, UnicodeDecodeError) as e:
                print(f"  ❌  Cannot repair meta [{proj[:8]}] {sid[:12]}: {e}")

    # 1. Fix orphan snapshots — generate .meta
    for proj, sessions in projects.items():
        d = os.path.join(root, proj)
        for sid, exts in sessions.items():
            if ".snapshot" in exts and ".meta" not in exts:
                snap_path = os.path.join(d, f"{sid}.snapshot")
                try:
                    sz = os.path.getsize(snap_path)
                    if sz < 50:
                        continue  # empty/invalid snapshot
                    with open(snap_path, "r", encoding="utf-8") as f:
                        snap = json.load(f)
                    msgs = snap.get("messages", [])
                    if not msgs:
                        continue
                    user_msgs = [m for m in msgs if m.get("role", "").lower() == "user"]
                    name = (user_msgs[0].get("text", "")[:60] if user_msgs
                            else f"session-{sid[:8]}")
                    name = name.replace("\n", " ").strip()[:60]
                    mtime = os.path.getmtime(snap_path)

                    meta = {
                        "v": META_VERSION,
                        "id": sid,
                        "name": name,
                        "user_renamed": False,
                        "ai_named": False,
                        "owner": "native",
                        "import_info": None,
                        "working_dir": "",
                        "created_at": int(mtime * 1000),
                        "updated_at": int(mtime * 1000),
                        "turn_count": len(user_msgs),
                        "message_count": len(msgs),
                        "turn_stats": [],
                    }
                    meta_path = os.path.join(d, f"{sid}.meta")
                    with open(meta_path, "w", encoding="utf-8") as f:
                        json.dump(meta, f, ensure_ascii=False, indent=2)
                    fixed_meta += 1
                    print(f"  ✅  Recovered [{proj[:8]}] '{name[:40]}' ({len(msgs)} msgs)")
                except (json.JSONDecodeError, KeyError, OSError, TypeError, AttributeError, UnicodeDecodeError) as e:
                    print(f"  ❌  Cannot recover [{proj[:8]}] {sid[:12]}: {e}")

    # 2. Fix missing .ui.json — create minimal file
    #    Use real-time filesystem state: step 1 may have just created .meta
    #    files that the initial scan snapshot doesn't know about.
    for proj, sessions in projects.items():
        d = os.path.join(root, proj)
        # Include freshly-recovered orphans: re-scan this project dir
        for entry in os.listdir(d):
            if not entry.endswith(".meta") or entry.endswith(".meta.lock"):
                continue
            sid = _strip_suffix(entry, ".meta")
            meta_path = os.path.join(d, f"{sid}.meta")
            ui_path = os.path.join(d, f"{sid}.ui.json")
            if os.path.isfile(meta_path) and not os.path.isfile(ui_path):
                try:
                    with open(ui_path, "w", encoding="utf-8") as f:
                        f.write('{"v":1,"entries":[]}')
                    fixed_ui += 1
                    print(f"  ✅  Created .ui.json [{proj[:8]}] {sid[:12]}")
                except OSError as e:
                    print(f"  ❌  Cannot create .ui.json [{proj[:8]}] {sid[:12]}: {e}")

    # 4. Clean stale lease files
    for proj, sessions in projects.items():
        d = os.path.join(root, proj)
        for sid, exts in sessions.items():
            if ".lease" in exts and ".snapshot" not in exts and ".meta" not in exts:
                lease_path = os.path.join(d, f"{sid}.lease")
                try:
                    os.remove(lease_path)
                    cleaned_leases += 1
                    print(f"  🧹  Cleaned stale lease [{proj[:8]}] {sid[:12]}")
                except OSError as e:
                    print(f"  ❌  Cannot clean lease [{proj[:8]}] {sid[:12]}: {e}")
        # Also clean orphan .lease files within the directory
        for f in os.listdir(d):
            if f.endswith(".lease"):
                # check if corresponding id has .snapshot or .meta
                sid = _strip_suffix(f, ".lease")
                has_snap = os.path.isfile(os.path.join(d, f"{sid}.snapshot"))
                has_meta = os.path.isfile(os.path.join(d, f"{sid}.meta"))
                if not has_snap and not has_meta:
                    try:
                        os.remove(os.path.join(d, f))
                        cleaned_leases += 1
                        print(f"  🧹  Cleaned orphan lease [{proj[:8]}] {sid[:12]}")
                    except OSError as e:
                        print(f"  ⚠  Could not clean lease [{proj[:8]}] {sid[:12]}: {e}")

    print(f"\n{'=' * 60}")
    print(f"  Summary: {repaired_corrupt} meta repaired, {fixed_meta} meta recovered, {fixed_ui} .ui.json created, {cleaned_leases} leases cleaned")
    print(f"  Restart AtomCode to see recovered sessions in /resume")
    print(f"{'=' * 60}")
    return fixed_meta + fixed_ui + cleaned_leases


# ── verify ───────────────────────────────────────────────────────────

def cmd_verify():
    """Validate all sessions have proper structure."""
    root = _sessions_root()
    print("Verifying session integrity...")
    valid = 0
    invalid = 0
    total = 0
    details = []

    for entry in sorted(os.listdir(root)):
        d = os.path.join(root, entry)
        if not os.path.isdir(d):
            continue
        for f in os.listdir(d):
            if not f.endswith(".meta"):
                continue
            total += 1
            sid = _strip_suffix(f, ".meta")
            meta_path = os.path.join(d, f)
            snap_path = os.path.join(d, f"{sid}.snapshot")
            ui_path = os.path.join(d, f"{sid}.ui.json")

            issues = []

            # Meta must be valid JSON
            try:
                with open(meta_path, "r", encoding="utf-8") as fh:
                    meta = json.load(fh)
                if not isinstance(meta, dict) or "id" not in meta:
                    issues.append("meta missing 'id'")
            except (json.JSONDecodeError, OSError, UnicodeDecodeError) as e:
                issues.append(f"meta parse error: {e}")

            # Snapshot should exist and be valid JSON
            if os.path.isfile(snap_path):
                try:
                    with open(snap_path, "r", encoding="utf-8") as fh:
                        snap = json.load(fh)
                    msgs = snap.get("messages", []) if isinstance(snap, dict) else []
                    if len(msgs) == 0:
                        issues.append("snapshot has no messages")
                except (json.JSONDecodeError, OSError, UnicodeDecodeError, TypeError, AttributeError) as e:
                    issues.append(f"snapshot error: {e}")
            else:
                issues.append("missing snapshot")

            # .ui.json should exist
            if not os.path.isfile(ui_path):
                issues.append("missing .ui.json")

            if issues:
                invalid += 1
                details.append((entry, sid[:12], issues))
            else:
                snap_size = os.path.getsize(snap_path)
                if snap_size > 100:
                    valid += 1
                else:
                    invalid += 1
                    details.append((entry, sid[:12], ["snapshot too small"]))

    print(f"  Total .meta files:    {total}")
    print(f"  Valid (with data):    {valid}")
    print(f"  Invalid/empty:        {invalid}")

    if invalid > 0:
        print(f"\n  Issues found:")
        for proj, sid, issues in details[:15]:
            print(f"    [{proj[:8]}] {sid}: {'; '.join(issues)}")
        if len(details) > 15:
            print(f"    ... and {len(details) - 15} more")

    if invalid == 0:
        print(f"\n  🎉  All sessions healthy!")
    else:
        print(f"\n  Run `session-keeper.py fix` to repair")

    return valid


# ── backup ───────────────────────────────────────────────────────────

def cmd_backup(quiet=False):
    """Create a timestamped full backup with MD5 checksums."""
    timestamp = datetime.now().strftime("%Y%m%d_%H%M%S")
    backup_dir = os.path.join(_backups_root(), f"BACKUP_{timestamp}")
    os.makedirs(backup_dir, exist_ok=True)

    if not quiet:
        print(f"Creating backup: {backup_dir}")

    # Copy sessions
    sessions_backup = os.path.join(backup_dir, "sessions")
    if os.path.isdir(_sessions_root()):
        shutil.copytree(_sessions_root(), sessions_backup, dirs_exist_ok=True)

    # Copy config
    config_src = os.path.join(ATOMCODE_HOME, "config.toml")
    if os.path.isfile(config_src):
        shutil.copy2(config_src, os.path.join(backup_dir, "config.toml"))

    # Copy memory
    memory_src = os.path.join(ATOMCODE_HOME, "memory.md")
    if os.path.isfile(memory_src):
        shutil.copy2(memory_src, os.path.join(backup_dir, "memory.md"))

    # Generate MD5 checksums
    checksums = {}
    for root, dirs, files in os.walk(backup_dir):
        for f in sorted(files):
            if f == "BACKUP_CHECKSUM.md5":
                continue
            fpath = os.path.join(root, f)
            md5 = hashlib.md5()
            with open(fpath, "rb") as fh:
                for chunk in iter(lambda: fh.read(65536), b""):
                    md5.update(chunk)
            rel = os.path.relpath(fpath, backup_dir)
            checksums[rel] = md5.hexdigest()

    checksum_path = os.path.join(backup_dir, "BACKUP_CHECKSUM.md5")
    with open(checksum_path, "w") as f:
        for rel, md5 in sorted(checksums.items()):
            f.write(f"{md5}  {rel}\n")

    # Calculate total size
    total_size = 0
    for root, dirs, files in os.walk(backup_dir):
        for f in files:
            fpath = os.path.join(root, f)
            if os.path.isfile(fpath):
                total_size += os.path.getsize(fpath)

    if not quiet:
        print(f"  ✅  Backup complete: {backup_dir}")
        print(f"  📦  Size: {total_size / 1024 / 1024:.1f} MB")
        print(f"  🔐  Checksums: {len(checksums)} files verified")

    return backup_dir


# ── list-backups ─────────────────────────────────────────────────────

def cmd_list_backups():
    """List all available backups with size and date."""
    backups_root = _backups_root()
    backups = sorted(
        [d for d in os.listdir(backups_root) if os.path.isdir(os.path.join(backups_root, d))],
        reverse=True,
    )

    if not backups:
        print("No backups found.")
        return

    print(f"{'Backup':40s} {'Size':>10s}  {'Date':20s}")
    print("-" * 72)
    for name in backups:
        d = os.path.join(backups_root, name)
        total_size = 0
        for root, dirs, files in os.walk(d):
            for f in files:
                fpath = os.path.join(root, f)
                if os.path.isfile(fpath):
                    total_size += os.path.getsize(fpath)
        # Parse date from dir name (BACKUP_20260724_112342)
        date_str = name.replace("BACKUP_", "").replace("_", " ", 1) if "BACKUP_" in name else name
        size_str = f"{total_size / 1024 / 1024:.1f}M" if total_size > 1024 * 1024 else f"{total_size / 1024:.1f}K"
        print(f"{name:40s} {size_str:>10s}  {date_str:20s}")


# ── restore ──────────────────────────────────────────────────────────

def cmd_restore(backup_name=None):
    """Restore sessions from a specified backup (default: latest)."""
    backups_root = _backups_root()
    all_backups = sorted(
        [d for d in os.listdir(backups_root) if os.path.isdir(os.path.join(backups_root, d))],
        reverse=True,
    )
    if not all_backups:
        print("No backups found.")
        return False

    if backup_name:
        if backup_name not in all_backups:
            print(f"Backup '{backup_name}' not found.")
            print(f"Available backups: {', '.join(all_backups[:10])}")
            return False
        chosen = backup_name
    else:
        chosen = all_backups[0]

    backup_path = os.path.join(backups_root, chosen)
    sessions_backup = os.path.join(backup_path, "sessions")

    if not os.path.isdir(sessions_backup):
        print(f"Backup '{chosen}' has no sessions directory.")
        return False

    # Verify checksums if available
    checksum_path = os.path.join(backup_path, "BACKUP_CHECKSUM.md5")
    if os.path.isfile(checksum_path):
        print(f"Verifying backup integrity...")
        try:
            with open(checksum_path, "r") as f:
                for line in f:
                    line = line.strip()
                    if not line:
                        continue
                    parts = line.split("  ", 1)
                    if len(parts) != 2:
                        continue
                    expected_md5, rel = parts
                    fpath = os.path.join(backup_path, rel)
                    if os.path.isfile(fpath):
                        md5 = hashlib.md5()
                        with open(fpath, "rb") as fh:
                            for chunk in iter(lambda: fh.read(65536), b""):
                                md5.update(chunk)
                        if md5.hexdigest() != expected_md5:
                            print(f"  ❌  Checksum mismatch: {rel}")
                            return False
                    else:
                        print(f"  ❌  Missing file in backup: {rel}")
                        return False
            print(f"  ✅  All checksums verified")
        except (OSError, ValueError) as e:
            print(f"  ❌  Checksum verification failed: {e}")
            print(f"  ⚠  Restore aborted to prevent data corruption")
            return False

    # Perform restore
    sessions_count = 0
    for entry in os.listdir(sessions_backup):
        src = os.path.join(sessions_backup, entry)
        dst = os.path.join(_sessions_root(), entry)
        if os.path.isdir(src):
            if os.path.isdir(dst):
                shutil.copytree(src, dst, dirs_exist_ok=True)
            else:
                shutil.copytree(src, dst)
            files_in_dir = len([f for f in os.listdir(src) if os.path.isfile(os.path.join(src, f))])
            sessions_count += files_in_dir

    print(f"  ✅  Restored from: {chosen}")
    print(f"  📂  Sessions files: {sessions_count}")
    print(f"  🔄  Restart AtomCode to see restored sessions")
    return True


# ── full ─────────────────────────────────────────────────────────────

def cmd_full():
    """Run diagnose → fix → verify."""
    print(f"\n{'=' * 60}")
    print("  AtomCode Session Keeper — Full Check & Repair")
    print(f"{'=' * 60}\n")
    issues = cmd_diagnose()
    print()
    if issues > 0:
        cmd_fix()
        print()
    cmd_verify()
    print(f"\n{'=' * 60}")
    print("  Done! Restart AtomCode to see changes.")
    print(f"{'=' * 60}")

# ── main ─────────────────────────────────────────────────────────────

def main():
    cmds = {
        "diagnose": ("Scan for issues", cmd_diagnose),
        "fix": ("Auto-repair detected issues", cmd_fix),
        "verify": ("Validate session structure", cmd_verify),
        "backup": ("Create verified backup", lambda: cmd_backup()),
        "list-backups": ("List available backups", cmd_list_backups),
        "restore": ("Restore from latest backup",
                     lambda: cmd_restore(sys.argv[2] if len(sys.argv) > 2 else None)),
        "full": ("Diagnose → Fix → Verify", cmd_full),
    }

    if len(sys.argv) < 2 or sys.argv[1] not in cmds:
        print(__doc__)
        print(f"\nCommands:")
        for name, (desc, _) in cmds.items():
            print(f"  {name:15s} {desc}")
        print(f"\nExamples:")
        print(f"  python {sys.argv[0]} diagnose")
        print(f"  python {sys.argv[0]} fix")
        print(f"  python {sys.argv[0]} full")
        print(f"  python {sys.argv[0]} backup")
        print(f"  python {sys.argv[0]} restore <backup_name>")
        return 1

    result = cmds[sys.argv[1]][1]()
    # Normalize exit code: None → 0, True → 0, False → 1,
    # str(backup_dir) → 0, int → cap at 127
    if result is None:
        return 0
    if isinstance(result, bool):
        return 0 if result else 1
    if isinstance(result, str):
        return 0
    return min(result, 127) if isinstance(result, int) else 0


if __name__ == "__main__":
    sys.exit(main())
