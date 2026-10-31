#!/system/bin/sh
# v2 清单的自动核验脚本：打印每一步的命令与实测输出，供人工/报告引用。
cd /data/user/0/com.atomcode.app/files/atomcode || exit 1
export HOME="$PWD"
export PATH="$PWD/bin:$PATH"
export TMPDIR="$PWD/tmp"
unset LD_PRELOAD
mkdir -p tmp

say() { printf '\n### %s\n' "$1"; }

say "0a 产物哈希"
sha256sum /sdcard/Download/*.tar.gz 2>/dev/null

say "0b 已装标记"
printf 'shmem 计数: '; ls packages/cpython/lib/python3.12/lib-dynload | grep -c shmem
printf '运行时 lib 里的 openblas/libc++ 计数: '; ls packages/cpython/lib | grep -Ec 'openblas|libc\+\+'
printf 'python-packages/.libs: '; ls python-packages/.libs 2>/dev/null | tr '\n' ' '; echo
printf 'packages: '; ls packages | tr '\n' ' '; echo

say "1 Python"
python3 -V
python3 -c 'import sys; print(repr(sys.executable))'
python3 -c 'import subprocess,sys
r = subprocess.run([sys.executable, "-c", "print(42)"], capture_output=True, text=True)
print("re-exec", r.returncode, r.stdout.strip())'
cat > tmp/spawncheck.py <<'PY'
import multiprocessing as mp, os
def work(p): open(p, "w").write(f"child {os.getpid()}")
if __name__ == "__main__":
    out = os.path.join(os.environ["HOME"], "tmp", "spawn.txt")
    ctx = mp.get_context("spawn")
    p = ctx.Process(target=work, args=(out,))
    p.start(); p.join(10)
    print("spawn", p.exitcode, os.path.exists(out))
PY
python3 tmp/spawncheck.py

say "2 SQLite"
sqlite3 :memory: "create virtual table b using fts5(x); insert into b values('hello'); select count(*) from b;"
sqlite3 :memory: "select sqrt(16), pow(2,10);"
sqlite3 :memory: "create virtual table r using rtree(id,minx,maxx); insert into r values(1,0,10); select count(*) from r;"
python3 -c 'import sqlite3
c = sqlite3.connect(":memory:")
print("enable_load_extension", hasattr(c, "enable_load_extension"))
c.enable_load_extension(True)
print("enabled", sqlite3.sqlite_version)'

say "3 Lua"
lua -e "print(package.cpath:match('^[^;]+'))"
lua -e 'local c=require("cjson"); local s=c.encode({name="atomcode",nums={1,2,3}}); print(s); print(c.decode(s).name)'

say "4 tcc + cheaders"
P="$PWD/packages/cheaders"
cat > tmp/probe.c <<'EOF'
#include <stdio.h>
#include <string.h>
int probe(int x) { char b[64]; snprintf(b, sizeof b, "v=%d", x); return (int)strlen(b) + x; }
EOF
tcc -shared -fPIC -nostdlib -DBIONIC_IOCTL_NO_SIGNEDNESS_OVERLOAD \
    -I"$P/include" -I"$P/builtin" -I"$P/include/python3.12" -L"$P/lib" -lc \
    -o tmp/probe.so tmp/probe.c && echo "tcc compile ok"
python3 -c 'import ctypes
l = ctypes.CDLL("tmp/probe.so")
l.probe.restype = ctypes.c_int
print("probe", [(x, l.probe(x)) for x in (5, 7, 100)])'

say "5 numpy"
ls -l python-packages | grep -E 'numpy|\.libs'
python3 -c 'import numpy as np
print("numpy", np.__version__)
a = np.arange(9, dtype=float).reshape(3, 3)
print("matmul", (a @ (np.eye(3) * 2)).tolist())
print("det/sum", float(np.linalg.det(a)), float(a.sum()))'

say "6 node"
node -e 'console.log(process.version, process.platform, process.arch, 1+1)'
node -e 'console.log([1,2,3].map(x=>x*2), require("fs").existsSync("."))'
node -e 'console.log("execPath", process.execPath)'
node -e 'console.log("execSync id:", require("child_process").execSync("id",{encoding:"utf8"}).split("\n")[0])'
node -e 'const cp=require("child_process"); const r=cp.spawnSync(process.execPath,["-e","1"]); console.log("self re-exec status", r.status)'

say "7 Zig"
export ZIG_LIB_DIR="$HOME/zig/lib"
zig version
cat > tmp/zigprobe.c <<'EOF'
#include <stdio.h>
#include <string.h>
int zigprobe(int x) { char b[64]; snprintf(b, sizeof b, "zig=%d", x); return (int)strlen(b) + x; }
EOF
zig cc -shared -fPIC -o tmp/zigprobe.so tmp/zigprobe.c 2>&1 | tail -2
python3 -c 'import ctypes
l = ctypes.CDLL("tmp/zigprobe.so")
l.zigprobe.restype = ctypes.c_int
print("zigprobe(5)", l.zigprobe(5))'
printf '不设 ZIG_LIB_DIR 时: '
( unset ZIG_LIB_DIR; zig cc -shared -fPIC -o tmp/zx.so tmp/zigprobe.c 2>&1 | tail -1 )

say "end"
