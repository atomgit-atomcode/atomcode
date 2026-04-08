+++
id = "052-bash-find-dedupe"
description = "用 find + 管道找出重复的 basename"
timeout_secs = 180
tags = ["code-gen", "bash", "find"]

[seed_files]
"tree/a/foo.txt" = "one"
"tree/b/foo.txt" = "two"
"tree/c/bar.txt" = "three"
"tree/d/baz.txt" = "four"
"tree/e/foo.txt" = "five"
"tree/f/bar.txt" = "six"
+++

写一个脚本 dupes.sh，递归扫描 `tree/` 目录下所有文件，找出 **basename 重复**
（同名但路径不同）的文件，按 basename 字母序输出，每个 basename 一行：
`<basename>: <count>`，只输出 count >= 2 的。

期望输出：
```
bar.txt: 2
foo.txt: 3
```

完成后 `bash dupes.sh` 验证。
