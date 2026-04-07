+++
id = "002-bash-verify"
description = "代码生成 + bash 验证"
timeout_secs = 180
tags = ["code-gen", "bash-verify"]

[seed_files]
"NOTES.md" = "Use Python 3. The script must print exactly 'OK' on success."
+++

写一个脚本 check.py：检查 NOTES.md 是否存在，存在就打印 "OK"，否则打印 "MISSING"。
然后用 bash 执行 `python check.py` 验证它输出 "OK"。
