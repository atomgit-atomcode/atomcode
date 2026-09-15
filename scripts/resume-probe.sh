#!/bin/bash
# 抓 atui 的 resume 缓存实验。用法：
#
#   ./scripts/resume-probe.sh                       # resume 最近一个会话
#   ./scripts/resume-probe.sh <session-id>          # resume 指定的会话
#
# **在一个独立的终端窗口里跑。** 不能从另一个正在运行的 TUI 里起：两个全屏
# 界面抢同一个 tty，后起的那个会 `failed to mount: entry 'surface' failed to
# apply: cannot take the terminal` 然后立刻退出——目录建好了、dump 是空的，
# 看起来像脚本坏了，其实是没拿到终端。
#
# 会在 /tmp/atui-probe/ 下落一份带时间戳的证据包，跑完 atui 退出后打印汇总。
# 设 ATOMCODE_DUMP_REQUEST 是全部意义所在：它让 harness 在每次组装请求时记
# 一行「这个进程此刻持有多少事件 + 每条消息的哈希」，这是唯一能区分
# 「进程读到的 log 就是短的」和「log 是长的、别处裁掉了」的东西。
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="/tmp/atui-probe/$STAMP"
mkdir -p "$OUT"

SESSION="${1:-}"
ATUI="$REPO/target/debug/atui"
[ -x "$ATUI" ] || { echo "先编译：cargo build -p atomcode-tui --bin atui"; exit 1; }

# 仪表必须在二进制里，否则跑完什么也抓不到——这是最容易白跑一次的地方。
if ! strings "$ATUI" | grep -q ATOMCODE_DUMP_REQUEST; then
  echo "!! 这个 atui 里没有请求仪表（agent_loop.rs 的 dump_request）。"
  echo "!! 先重新编译：cargo build -p atomcode-tui --bin atui"
  exit 1
fi

DUMP="$OUT/requests.jsonl"
: > "$DUMP"

# resume 的目标：显式给 id，或取最近的那个。
if [ -n "$SESSION" ]; then
  ARGS=(--resume "$SESSION")
  echo "resume 指定会话：$SESSION"
else
  ARGS=(--continue)
  echo "resume 最近一个会话（--continue）"
  SESSION="<最近>"
fi

echo "证据包：$OUT"
echo "dump  ：$DUMP"
echo
echo "──── 现在随便说一句话。说完按 ctrl-d 退出，我会汇总。 ────"
echo

ATOMCODE_DUMP_REQUEST="$DUMP" "$ATUI" "${ARGS[@]}"

echo
echo "════════ 汇总 ════════"
python3 - "$DUMP" "$OUT" <<'PY'
import json, sys, os, hashlib, glob

dump, out = sys.argv[1], sys.argv[2]
rows = []
for line in open(dump):
    line = line.strip()
    if not line:
        continue
    try:
        rows.append(json.loads(line))
    except Exception:
        pass

print(f"抓到 {len(rows)} 个请求")
if not rows:
    print("!! 一个请求都没有。是不是没在会话里说话？")
    sys.exit(0)

first, last = rows[0], rows[-1]
print()
print(f"{'#':>3} {'events':>8} {'max_seq':>8} {'n_msgs':>7} {'chars':>9}")
for i, r in enumerate(rows):
    flag = ""
    if i == 0:
        flag = "  <- resume 后第一个请求"
    print(f"{i:>3} {r['events']:>8} {r['max_seq']:>8} {r['n_msgs']:>7} {r['total_chars']:>9}{flag}")

print()
print("──── 关键判据 ────")
# 进程持有的 log 与会话文件对比
sess = first.get("session")
print(f"进程报的 session: {sess}")
# 找同名日志文件，比较 events 数
cands = glob.glob(os.path.expanduser("~/.atomcode/sessions/*/*.jsonl"))
hit = [c for c in cands if sess and sess in c]
if hit:
    p = hit[0]
    n = sum(1 for l in open(p) if l.strip() and '"header"' not in l)
    print(f"日志文件: {p}")
    print(f"  文件里的非 header 行: {n}")
    print(f"  进程首个请求报的 events: {first['events']}")
    print(f"  差: {n - first['events']}  （>0 且很大 = 进程读到的 log 比文件短）")
else:
    print(f"没找到 {sess} 的日志文件")

# 首个请求的消息形状
msgs = first.get("msgs") or []
sysmsg = [m for m in msgs if m["role"] == "System"]
with_r = [m for m in msgs if m.get("rlen", 0) > 0]
print()
print(f"首个请求: {len(msgs)} 条消息，其中带 reasoning 的 {len(with_r)} 条")
print(f"  System {len(sysmsg)} 条；正文总长 {first['total_chars']}")
print()
print("把这张表发给我，配上最后一次 usage 的 prompt/cached 就行。")
print(f"原始 dump: {dump}")
PY

echo
echo "证据包留在：$OUT"
