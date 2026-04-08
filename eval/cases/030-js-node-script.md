+++
id = "030-js-node-script"
description = "Node 脚本读 stdin 输出 JSON 统计"
timeout_secs = 180
tags = ["code-gen", "javascript", "node"]
+++

写一个 Node.js 脚本 count.js：
- 从 stdin 读取多行文本
- 统计 total_lines / total_words / total_chars（chars 包括所有换行和空白）
- 以单行 JSON 形式打印结果，字段按字母序：`{"total_chars":N,"total_lines":N,"total_words":N}`

然后用 bash 验证：
```
printf 'hello world\nfoo bar baz\n' | node count.js
```
应该输出 `{"total_chars":24,"total_lines":2,"total_words":5}`。
