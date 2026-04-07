+++
id = "033-js-refactor-callbacks"
description = "把 callback hell 重构成 async/await"
timeout_secs = 180
tags = ["refactor", "javascript", "async"]
+++

pipeline.js 里有一个 callback 风格的数据处理流水线：
`loadData(cb) -> transform(data, cb) -> save(data, cb)`，main 里是典型 callback hell。

请将它重构成 Promise + async/await：
- 保留这三个底层函数的**外部行为**（参数 / 返回 / 错误语义），但返回 Promise（或做 util.promisify）
- 顶层 main 用 async/await 串起来
- 错误处理要用 try/catch，失败时 `process.exit(1)` 且打印 "PIPELINE FAILED: <msg>"
- 成功时打印 "PIPELINE OK"

完成后用 `node pipeline.js` 验证，应看到 "PIPELINE OK"。
