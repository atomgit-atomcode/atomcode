+++
id = "040-go-http-handler"
description = "写一个 /health + /echo HTTP handler"
timeout_secs = 180
tags = ["code-gen", "go", "http"]
+++

当前目录是一个 Go module（见 go.mod）。请实现 main.go：
- `GET /health` → 200，body `{"status":"ok"}`，Content-Type `application/json`
- `POST /echo` → 200，body 是请求 body 原样返回，Content-Type 照抄 request 的 Content-Type
- 其他路径 → 404

用 `net/http` 标准库（不要引入第三方依赖）。
监听地址从环境变量 `PORT` 读取，缺省 `:8080`。

完成后用 `go build ./...` 验证编译通过即可（不要求真的跑起服务器）。
