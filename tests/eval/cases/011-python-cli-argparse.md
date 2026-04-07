+++
id = "011-python-cli-argparse"
description = "写一个带 argparse 的 Python CLI"
timeout_secs = 180
tags = ["code-gen", "python", "cli"]
+++

写一个 Python 脚本 greet.py，使用 argparse 解析参数：
- 必需参数：--name <str>
- 可选参数：--count <int>，默认 1
- 可选参数：--upper，布尔 flag

运行时按 count 次数打印 "Hello, <name>!"，如果 --upper 则全部大写。

例如 `python3 greet.py --name Alice --count 2 --upper` 应输出两行 "HELLO, ALICE!"。
完成后用 bash 跑一次验证。
