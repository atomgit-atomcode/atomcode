+++
id = "031-js-fix-async"
description = "修一个 Promise.all 误用的 bug"
timeout_secs = 180
tags = ["debug-fix", "javascript", "async"]
+++

fetch_all.js 模拟一个并发获取多个 URL 的函数 `fetchAll(urls)`，但有两个 bug：
1. 它在 for 循环里 await 每个请求，失去了并发（应该用 Promise.all）
2. 其中一个请求失败时，整个函数会挂死而不是抛错

test.js 是手写的测试，当前会超时/失败。

请：
1. 读 test.js 理解期望
2. 修 fetch_all.js 让所有测试通过（使用 Promise.all / Promise.allSettled 之一，按 test 要求）
3. 用 `node test.js` 验证输出 "ALL TESTS PASSED"
