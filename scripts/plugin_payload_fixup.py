#!/usr/bin/env python3
"""把 Python 载荷里扩展的 libpython 依赖名统一成短名。

背景：这个运行时把 `libpython3.12.so.1.0` 的 SONAME 改成了 `libpython3.12.so`，并把
自己包内所有扩展的 DT_NEEDED 一并改写；但**第三方 wheel**（cffi、numpy 之类）里的扩展
仍可能指向长名。bionic 只按 SONAME 匹配已加载的库，长名既匹配不上预加载的那一份，
文件系统里也找不到（运行时的 `lib/` 不在加载器搜索路径上），于是 import 时报
`library "libpython3.12.so.1.0" not found`。

改名是等长替换（短名 + NUL 填充），不动其它字节，也不改变动态段布局。

用法：
  plugin_payload_fixup.py <dir-or-file> [...]
"""
import sys
from pathlib import Path

OLD = b"libpython3.12.so.1.0"
NEW = b"libpython3.12.so" + bytes(len(OLD) - len(b"libpython3.12.so"))


def fix(path: Path) -> bool:
    data = path.read_bytes()
    if OLD not in data:
        return False
    count = data.count(OLD)
    if count != 1:
        raise SystemExit(f"error: {path} 里出现 {count} 次 {OLD.decode()}，不敢盲改")
    path.write_bytes(data.replace(OLD, NEW))
    return True


def main(targets: list[str]) -> int:
    changed = 0
    for target in targets:
        root = Path(target)
        candidates = [root] if root.is_file() else [
            entry for entry in root.rglob("*.so") if entry.is_file()
        ]
        for candidate in candidates:
            if fix(candidate):
                print(f"   rewrote libpython soname reference: {candidate.name}")
                changed += 1
    print(f"   共改写 {changed} 个扩展")
    return 0


if __name__ == "__main__":
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    raise SystemExit(main(sys.argv[1:]))
