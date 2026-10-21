# WebUI restart connection shutdown implementation plan

**Goal:** 修复 stop / login / 再启动后，新 token 被旧 HTTP 连接拒绝。
**Architecture:** 进程内 server owner 通过 watch 发出停止信号，由现有 axum graceful shutdown 关闭 listener/keepalive；同一信号结束旧响应体（包括 SSE）。保留 token 轮换及原鉴权规则，不复用旧 token store，不取消 coding runtime。WebUI / App 使用同一 HTTP 生命周期实现；独立 daemon 保持现有信号与 HTTP shutdown 行为。
**Tech Stack:** Rust，tokio watch，axum，已有 futures stream。

基线 release/v5.2.4 61e9f395d2427a05831f80cd5e85d0ee7c1fb49f。仅有无关 untracked 文件。已用最小 axum/reqwest 用例确认 abort listener 后原客户端仍收到旧实例响应。

1. ServerOpts 增加可选宿主停止 sender（与 AppState 原有 shutdown 通道共用）；WebuiHandle / AppServerHandle 持 sender，停止时通知，不直接 abort listener。
2. run_server 共用 serve_http 将宿主信号接入 graceful shutdown；同一信号关闭旧请求/响应体，避免 SSE 无期限保留实例。
3. 更新 standalone / CLI / 集成测试装配，均使用 None，不引入第二 runtime owner。
4. 测试旧 HTTP client 重连新服务、旧 token 拒绝 / 新 token 接受、SSE 停止及 bootstrap 前停止；跑 daemon / CLI 相关测试、fmt 和 compile gate。不使用截图凭据，不退出真实账号。

验证：最小用例确认旧 abort 路径会继续响应旧 HTTP 连接；新路径覆盖复用客户端跨实例鉴权、SSE 停止和启动前停止。daemon 360 项、CLI 590 项测试、compile gate 与格式检查通过。未手工执行真实网页登录退出链路。
