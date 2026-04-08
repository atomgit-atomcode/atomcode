+++
id = "041-go-fix-nilpanic"
description = "修一个 nil pointer deref panic"
timeout_secs = 180
tags = ["debug-fix", "go"]
+++

main.go 当前跑起来会在某个输入上 nil pointer deref。
user_test.go 有 3 个测试（unittest 风格），当前会 panic / fail。

请：
1. 先跑 `go test ./...` 看失败原因
2. 修 main.go 里的 bug（nil check 缺失 + map 未初始化之一或多处）
3. 不要修改 test
4. 最后 `go test ./...` 全部通过
