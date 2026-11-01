#!/usr/bin/env bash
# 量一次「长回合 + Doze」下的内存与存活情况。
#
# 这是路线图 P7 唯一的空白项，也是唯一没法在构建机上回答的问题：设备进 Doze 后
# 前台服务会不会被掐、Rust 运行时占多少内存、回合能不能跑完，只有真机说了算。
#
# 脚本做的事：
#   1. 从 logcat 取当前 daemon 的端口与 token，转发到主机；
#   2. 记一次空闲基线（PSS / RSS）；
#   3. 起一个回合，让模型用 bash 跑一条长命令（这样回合会持续一段时间）；
#   4. 回合进行中每隔几秒采一次内存，记峰值；
#   5. 强制设备进 Doze（`dumpsys deviceidle force-idle`），等回合结束；
#   6. 检查：回合是否真的结束、进程 PID 是否没变、内存相比基线涨了多少。
#
# 用法：bash scripts/verify-android-doze.sh
#   APP_ID=com.atomcode.app    (default)
#   TURN_SECONDS=90            回合里那条命令睡多久
#
# 注意：`force-idle` 在不允许的设备上会被拒绝，脚本会明确说「未能进入 Doze」而不是
# 假装测过。
set -euo pipefail
cd "$(dirname "$0")/.."
APP_ID=${APP_ID:-com.atomcode.app}
TURN_SECONDS=${TURN_SECONDS:-90}
SAMPLE_INTERVAL=${SAMPLE_INTERVAL:-10}

say() { printf '\n=== %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
ADB=$(command -v adb || printf '%s' ".android-sdk/platform-tools/adb.exe")

[ -n "$("$ADB" devices | sed -n '2p')" ] || fail "no device on adb"

# 端口与 token 只在进程启动时打一次，所以看最近的 logcat 就够；不看 pid，
# 因为服务可能已经自愈重启过，那正是我们想拿到的那个新端口。
line=$("$ADB" logcat -d -s AtomCode:* 2>/dev/null | grep "agent server listening" | tail -1 || true)
[ -n "$line" ] || fail "没有在 logcat 里找到 'agent server listening'，先启动一次应用"
PORT=$(printf '%s' "$line" | sed -E 's#.*127\.0\.0\.1:([0-9]+)/.*#\1#')
TOKEN=$(printf '%s' "$line" | sed -E 's#.*token=([0-9a-f]+).*#\1#')
[ -n "$PORT" ] && [ -n "$TOKEN" ] || fail "没能从日志里解析端口/token：$line"
say "daemon http://127.0.0.1:$PORT/"
"$ADB" forward "tcp:$PORT" "tcp:$PORT" >/dev/null
API="http://127.0.0.1:$PORT"
auth=(-H "Authorization: Bearer $TOKEN")

# `dumpsys meminfo` 的 TOTAL 行就是我们要的数；TOTAL PSS 在不同版本上有时叫 TOTAL。
pss() {
  "$ADB" shell "dumpsys meminfo $APP_ID" 2>/dev/null |
    awk '/TOTAL PSS|^ *TOTAL/ {print $2; exit}'
}
pid_of() { "$ADB" shell pidof "$APP_ID" 2>/dev/null | tr -d '\r'; }

say "清一次旧回合状态"
curl -s "${auth[@]}" "$API/health" >/dev/null || fail "daemon 不响应 /health"

PID_BEFORE=$(pid_of)
[ -n "$PID_BEFORE" ] || fail "$APP_ID 没有在跑"
BASE=$(pss)
[ -n "$BASE" ] || fail "读不到 $APP_ID 的 PSS"
say "基线：pid=$PID_BEFORE PSS=${BASE} KB"

say "起一个长回合（bash sleep $TURN_SECONDS）"
printf '%s' "{\"message\":\"Use the bash tool to run exactly: sleep $TURN_SECONDS; echo doze-probe-$TURN_SECONDS. Then report the output. Do not do anything else.\"}" > /tmp/doze-turn.json
curl -s -N --max-time $((TURN_SECONDS + 240)) -X POST "${auth[@]}" \
  -H "Content-Type: application/json" --data-binary @/tmp/doze-turn.json \
  "$API/chat" > /tmp/doze-turn.out 2>&1 &
TURN_PID=$!

# 等回合真的开始（active 数不再为 0），否则后面的采样全是在量空闲。
say "等回合开始"
started=0
for _ in $(seq 1 30); do
  active=$(curl -s "${auth[@]}" "$API/health" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("active_chats",0))' 2>/dev/null || echo 0)
  if [ "${active:-0}" -gt 0 ]; then started=1; break; fi
  sleep 2
done
[ "$started" = 1 ] || fail "回合没有真的开始（active_chats 一直为 0），看 /tmp/doze-turn.out"

say "回合进行中：采样内存"
PEAK=$BASE
samples=0
for _ in $(seq 1 $((TURN_SECONDS / SAMPLE_INTERVAL))); do
  now=$(pss)
  if [ -n "$now" ]; then
    [ "$now" -gt "$PEAK" ] && PEAK=$now
    samples=$((samples + 1))
    printf '  PSS %s KB\n' "$now"
  fi
  sleep "$SAMPLE_INTERVAL"
done
[ "$samples" -gt 0 ] || fail "一次内存都没采到"

say "强制进入 Doze"
if "$ADB" shell "dumpsys deviceidle force-idle" 2>&1 | grep -qi "idle"; then
  DOZE=entered
else
  DOZE=refused
  printf '  设备拒绝了 force-idle：这次不算测过 Doze\n'
fi
# 屏幕常亮会让 force-idle 立刻退出；关屏才是真实场景。
"$ADB" shell "input keyevent KEYCODE_SLEEP" >/dev/null 2>&1 || true
state=$("$ADB" shell "dumpsys deviceidle | grep -m1 mState" 2>/dev/null | tr -d '\r')
printf '  %s\n' "${state:-（读不到 deviceidle 状态）}"

say "等回合结束"
wait "$TURN_PID" || true
tail -c 400 /tmp/doze-turn.out

"$ADB" shell "input keyevent KEYCODE_WAKEUP" >/dev/null 2>&1 || true
"$ADB" shell "dumpsys deviceidle unforce" >/dev/null 2>&1 || true

say "结果"
PID_AFTER=$(pid_of)
FINAL=$(pss)
finished=$(curl -s "${auth[@]}" "$API/health" |
  python3 -c 'import json,sys;d=json.load(sys.stdin);t=d.get("last_turns") or [];print(t[0].get("outcome") if t else "none")' 2>/dev/null || echo "unknown")
printf '  Doze          : %s\n' "$DOZE"
printf '  回合终态       : %s\n' "$finished"
printf '  pid 前后       : %s → %s\n' "$PID_BEFORE" "$PID_AFTER"
printf '  PSS 基线/峰值/结束 : %s / %s / %s KB\n' "$BASE" "$PEAK" "${FINAL:-?}"

[ "$PID_AFTER" = "$PID_BEFORE" ] || fail "进程换了（$PID_BEFORE → $PID_AFTER）：回合中途被杀并重启"
[ "$finished" = "ok" ] || fail "回合没有以 ok 结束（$finished）"
[ "$DOZE" = entered ] || fail "未能进入 Doze，这一项没有真正验证"

say "内存与 Doze 检查通过"