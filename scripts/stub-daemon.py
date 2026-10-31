"""桩 daemon：冒充 `atomcode daemon`，用来验证 App 的外部守护进程发现与切换。

真实 CLI 目前编不到 Android（arboard 没有 Android 实现），但 App 侧要验证的东西
——扫汇合文件、校 /health 指纹、拿 token、切换页面、失效回落——都只依赖 HTTP 与
文件，所以一个几十行的桩足够。它做三件事：

  1. 在 127.0.0.1:<PORT> 上监听；
  2. `/health` 返回 `{"service":"atomcode-daemon", ...}`（App 的指纹）；
  3. 在共享目录写 `daemon-<PORT>.json`（App 的凭据来源）。
"""
import http.server
import json
import os
import socketserver

PORT = int(os.environ.get("STUB_PORT", "8787"))
TOKEN = os.environ.get("STUB_TOKEN", "stubtoken")
# 1 = 首页要令牌（模拟默认鉴权）；0 = 免鉴权（模拟 --no-auth）
REQUIRE_AUTH = os.environ.get("STUB_REQUIRE_AUTH", "0") == "1"
WORKSPACE = os.environ.get("STUB_WORKSPACE") or os.environ.get("HOME") or "/sdcard/AtomCode"

PAGE = (
    "<!doctype html><html><head><meta charset='utf-8'>"
    "<meta name='viewport' content='width=device-width,initial-scale=1'>"
    "<title>外部守护进程</title></head>"
    "<body style='font-family:sans-serif;padding:28px;line-height:1.7'>"
    "<h1 style='font-size:1.3rem'>外部守护进程 stub</h1>"
    f"<p>端口 <code>{PORT}</code> — App 已经把界面切到外部 daemon。</p>"
    "<p>/health 会返回 <code>service=atomcode-daemon</code>。</p>"
    "</body></html>"
)


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802 - stdlib naming
        if REQUIRE_AUTH and not self.path.startswith("/health"):
            if self.headers.get("Authorization") != f"Bearer {TOKEN}":
                self.send_response(401)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
        if self.path.startswith("/health"):
            body = json.dumps(
                {"status": "ok", "service": "atomcode-daemon", "version": "stub-1"}
            ).encode()
            content_type = "application/json"
        else:
            body = PAGE.encode()
            content_type = "text/html; charset=utf-8"
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


os.makedirs(WORKSPACE, exist_ok=True)
handoff = os.path.join(WORKSPACE, f"daemon-{PORT}.json")
with open(handoff, "w", encoding="utf-8") as fh:
    json.dump({"pid": os.getpid(), "port": PORT, "token": TOKEN}, fh)
print(f"stub daemon on 127.0.0.1:{PORT}, handoff {handoff}", flush=True)

socketserver.TCPServer.allow_reuse_address = True
with socketserver.TCPServer(("127.0.0.1", PORT), Handler) as httpd:
    httpd.serve_forever()
