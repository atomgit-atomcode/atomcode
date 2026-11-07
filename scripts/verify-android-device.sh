#!/usr/bin/env bash
# 真机验收：装 APK、启机、确认它在跑、读 daemon 的 /health。
#
# 为什么单独一个脚本：这条链路每次都要重复「装 → 拉起 → 找端口 → 带 token 查健康」，
# 而且坑很多——`adb install` 的流式安装在部分 provider APK 上会挂死（用 --no-streaming），
# daemon 每次启动监听的是**随机端口**、token 也是随机的（只能从 logcat 里捞），
# 而「gradle 说 BUILD SUCCESSFUL」并不等于「APK 里的 .so 是新的」（.so 旧时 gradle 也绿）。
#
# 用法：
#   bash scripts/verify-android-device.sh                 # 验收当前 APK
#   GRADLE=1 bash scripts/verify-android-device.sh        # 先 webui→.so→APK 再验收
#
# 退出码：0 = 装上了并跑起来且 /health 正常；1 = 任一步失败（每步都有明确的说法）。
set -uo pipefail

APK="android/app/build/outputs/apk/debug/app-debug.apk"
PKG="com.atomcode.app"
ACTIVITY="$PKG/.MainActivity"

say() { printf '%s\n' "$*"; }
fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

if [ "${GRADLE:-0}" = "1" ]; then
  say "== 重建 webui → .so → APK（改了页面就必须走这一步，只跑 gradle 会装到旧页面）=="
  (cd webui && npm run build) || fail "webui 构建失败"
  ANDROID_HOME="${ANDROID_HOME:-$PWD/.android-sdk}" bash scripts/build-android.sh || fail ".so 构建失败"
  (cd android && gradle assembleDebug --no-daemon) || fail "APK 打包失败"
fi

[ -f "$APK" ] || fail "$APK 不存在"

say "== 设备 =="
adb devices | tail -n +2 | grep -q "device$" || fail "没有在线设备（USB 掉了？adb devices 为空）"
serial="$(adb devices | tail -n +2 | awk '$2=="device"{print $1; exit}')"
say "serial=$serial"

say "== 安装（--no-streaming：流式安装在这台设备上会挂到超时）=="
adb -s "$serial" install --no-streaming -r "$APK" | tail -n 1 | grep -q "Success" ||
  fail "安装失败"

say "== 启机 =="
adb -s "$serial" shell am force-stop "$PKG"
adb -s "$serial" logcat -c
adb -s "$serial" shell am start -n "$ACTIVITY" >/dev/null 2>&1 || fail "启动失败"

# daemon 起来要一会儿：等它把监听地址打进日志（最多 60 秒）。
endpoint=""
for _ in $(seq 1 30); do
  sleep 2
  endpoint="$(adb -s "$serial" logcat -d 2>/dev/null | grep -o "listening at http://127.0.0.1:[0-9]*/?token=[a-f0-9]*" | tail -1)"
  [ -n "$endpoint" ] && break
done
[ -n "$endpoint" ] || fail "60 秒内没看到 daemon 监听（看 adb logcat | grep AtomCode）"

port="$(printf '%s' "$endpoint" | sed -E 's#.*:([0-9]+)/.*#\1#')"
token="$(printf '%s' "$endpoint" | sed -E 's#.*token=([a-f0-9]+).*#\1#')"
say "port=$port"

say "== 进程与崩溃 =="
adb -s "$serial" shell pidof "$PKG" | grep -q . || fail "应用进程不在"
crashes="$(adb -s "$serial" logcat -d 2>/dev/null | grep -cE "FATAL EXCEPTION.*$PKG|UnsatisfiedLinkError")"
[ "$crashes" = "0" ] || fail "日志里有 $crashes 条崩溃（adb logcat -d | grep FATAL）"

say "== /health =="
adb -s "$serial" forward "tcp:$port" "tcp:$port" >/dev/null 2>&1 || fail "端口转发失败"
health="$(curl -s --max-time 15 "http://127.0.0.1:$port/health" -H "Authorization: Bearer $token")"
[ -n "$health" ] || fail "/health 没有响应"

printf '%s' "$health" | python -c '
import json, sys
body = json.load(sys.stdin)
print("  status:", body.get("status"))
print("  version:", body.get("version"))
print("  pending_approvals:", body.get("pending_approvals"))
' || fail "/health 返回的不是 JSON"

say "== 安装的包版本（应与 Cargo.toml 一致）=="
expected="$(sed -n '/\[workspace.package\]/,/^\[/p' Cargo.toml | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)"
installed="$(adb -s "$serial" shell dumpsys package "$PKG" 2>/dev/null | sed -n 's/.*versionName=\([^ ]*\).*/\1/p' | head -1)"
say "  Cargo.toml=$expected  APK/设备=$installed"
[ "$expected" = "$installed" ] || say "  WARN: 版本不一致（APK 不是按当前 Cargo.toml 打的？）"

say "OK"