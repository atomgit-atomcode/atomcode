+++
id = "051-bash-safe-script"
description = "给老脚本加 safety 保护并修隐藏 bug"
timeout_secs = 180
tags = ["refactor", "bash", "safety"]
+++

legacy.sh 是一个老脚本，有以下问题：
1. 没有 `set -euo pipefail`，错误会被静默
2. 变量未加引号，文件名含空格会炸
3. 循环里 `rm $f` 拼接可能误删（实际上现在没炸只是走运）
4. 未检查必需的环境变量 `DATA_DIR`

请重构 legacy.sh（原地修改或改名为 safe.sh 也可，**只要最终有一个 safe.sh**）：
- 加 `set -euo pipefail`
- 所有变量展开加双引号
- 如果 `DATA_DIR` 未设置或指向不存在的目录，打印错误并退出 1
- 逻辑本身不变：列出 `$DATA_DIR` 下所有 `.tmp` 文件并打印（不要真的删，改成 echo 即可）

最后用 bash 验证：
```
mkdir -p /tmp/eval-safe-test && touch "/tmp/eval-safe-test/a b.tmp" "/tmp/eval-safe-test/c.tmp"
DATA_DIR=/tmp/eval-safe-test bash safe.sh
```
应该看到两个 .tmp 文件被列出。
