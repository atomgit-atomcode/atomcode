+++
id = "042-go-table-test"
description = "把重复的 Go 测试重构成 table-driven 风格"
timeout_secs = 180
tags = ["refactor", "test-writing", "go"]
+++

calc_test.go 里有 5 个测试几乎完全相同，只是输入/期望输出不同。
请把它们重构成 1 个 table-driven 测试（使用 `t.Run(tc.name, ...)` 子测试）。

要求：
- 所有原有 case 必须被覆盖（不要删除 case）
- 新测试的命名：`TestAdd`（一个函数，内部跑所有 case）
- 保留原函数签名；不要动 calc.go
- 最后 `go test -v ./...` 应列出 5 个子测试并全部通过
