#!/usr/bin/env python3
"""Unpack a Termux `data.tar` into a `$PREFIX`-shaped tree.

A plain `tar -x` is wrong on a Windows build host for two reasons, and both are
about the archive rather than about tar:

* The tree is rooted at `data/data/com.termux/files/usr` — the path Termux has on
  a phone, not a portable tree. It is stripped here so `bin/lld` is where the
  packaging scripts look for it.
* Symlinks are package bookkeeping (`bin/ld -> ld.lld -> lld`) and Windows only
  creates them with Developer Mode or administrator rights, and never for a
  target that has not been written yet — the common case while extracting. They
  are materialised as real copies after the regular files land. That costs a few
  bytes per link and is what Android packaging wants anyway: an APK and a data
  tarball carry file contents, not links.

Usage: extract.py <data.tar[.xz|.zst]> <root> [strip-prefix]
"""
import os
import posixpath
import shutil
import sys
import tarfile

DEFAULT_STRIP = "data/data/com.termux/files/usr"


def archive_path(name: str) -> str:
    """Normalise a tar member name to `a/b`, without `./` and without a leading `/`."""
    name = name.replace("\\", "/")
    while name.startswith("./"):
        name = name[2:]
    return name.lstrip("/")


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__, file=sys.stderr)
        return 2
    archive, root = sys.argv[1], sys.argv[2]
    strip = (sys.argv[3] if len(sys.argv) > 3 else DEFAULT_STRIP).strip("/")
    os.makedirs(root, exist_ok=True)

    regular: dict[str, str] = {}
    links: list[tuple[str, str]] = []
    hardlinks: list[tuple[str, str]] = []
    files = 0

    with tarfile.open(archive) as tar:
        for member in tar:
            name = archive_path(member.name)
            if name != strip and not name.startswith(strip + "/"):
                continue
            rel = name[len(strip):].lstrip("/")
            if not rel:
                continue
            dest = os.path.join(root, *rel.split("/"))
            if member.isdir():
                os.makedirs(dest, exist_ok=True)
            elif member.issym():
                links.append((rel, member.linkname))
            elif member.islnk():
                # A hard link's target is archive-relative, not sibling-relative.
                target = archive_path(member.linkname)
                if target.startswith(strip + "/"):
                    target = target[len(strip) + 1:]
                hardlinks.append((rel, target))
            elif member.isfile():
                stream = tar.extractfile(member)
                if stream is None:
                    continue
                os.makedirs(os.path.dirname(dest) or root, exist_ok=True)
                with open(dest, "wb") as out:
                    shutil.copyfileobj(stream, out)
                # Termux marks executables +x; Windows has no mode bits but the
                # packaging step reads nothing from them, so keep it simple.
                try:
                    os.chmod(dest, member.mode | 0o600)
                except OSError:
                    pass
                regular[rel] = dest
                files += 1

    # Resolve symlink chains to the regular file they ultimately name, then copy.
    index = dict(links)
    materialised = 0
    misses = 0
    for rel, _target in links:
        seen: set[str] = set()
        cur = rel
        while cur in index and cur not in seen:
            seen.add(cur)
            base = posixpath.dirname(cur)
            nxt = posixpath.normpath(posixpath.join(base, index[cur]))
            if nxt in regular:
                cur = nxt
                break
            cur = nxt
        if cur not in regular:
            misses += 1
            print(f"  warning: {rel} -> no regular file to copy", file=sys.stderr)
            continue
        dest = os.path.join(root, *rel.split("/"))
        os.makedirs(os.path.dirname(dest) or root, exist_ok=True)
        shutil.copyfile(regular[cur], dest)
        materialised += 1

    for rel, target in hardlinks:
        if target not in regular:
            misses += 1
            print(f"  warning: hard link {rel} -> {target} missing", file=sys.stderr)
            continue
        dest = os.path.join(root, *rel.split("/"))
        os.makedirs(os.path.dirname(dest) or root, exist_ok=True)
        shutil.copyfile(regular[target], dest)
        materialised += 1

    print(
        f"→ {files} files, {materialised} links materialised"
        + (f", {misses} unresolved" if misses else ""),
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())