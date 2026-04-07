+++
id = "001-fizzbuzz"
description = "最小代码生成 smoke"
timeout_secs = 180
tags = ["code-gen", "python", "smoke"]
# provider omitted — uses config.toml's default_provider
# (override with: ./scripts/eval/run.sh --provider <name>)
+++

写一个 Python 脚本 output.py，打印 1 到 30 的 fizzbuzz。
不要加额外的说明，只要能直接 `python output.py` 跑出正确结果。
