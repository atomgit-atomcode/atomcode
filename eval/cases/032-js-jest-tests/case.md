+++
id = "032-js-jest-tests"
description = "给 util 补 assert-based 单测"
timeout_secs = 180
tags = ["test-writing", "javascript"]
+++

strutil.js 有几个字符串工具函数但没测试。
为了避免安装 jest，请用 Node 内置的 `assert` 和 `node:test` 写测试到 `strutil.test.js`，
覆盖所有公开函数，每个函数至少 2 个 case。

完成后用 `node --test strutil.test.js` 验证全部通过。
