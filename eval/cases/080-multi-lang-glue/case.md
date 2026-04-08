+++
id = "080-multi-lang-glue"
description = "Python 写产物 + Bash 写验证脚本，协同完成"
timeout_secs = 180
tags = ["multi-file", "python", "bash"]
+++

请实现两个互相配合的脚本：

1. `generate.py`：读取环境变量 `COUNT`（缺省 5），生成一个 `numbers.txt`，
   内容是 1 到 COUNT 的平方数，每行一个。

2. `verify.sh`：bash 脚本，`set -euo pipefail`。先执行 `python3 generate.py`，
   然后验证：
   - `numbers.txt` 存在
   - 行数 == COUNT（缺省 5）
   - 每一行都是该位置的平方（第 n 行是 n*n）
   验证通过输出 `VERIFY OK`，任何一条失败输出 `VERIFY FAIL: <reason>` 并 exit 1。

最后用 `bash verify.sh` 跑一次验证，应该看到 `VERIFY OK`。
再用 `COUNT=10 bash verify.sh` 再跑一次，也应该 OK。
