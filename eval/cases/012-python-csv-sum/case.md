+++
id = "012-python-csv-sum"
description = "读取 CSV 按类别汇总销售额"
timeout_secs = 180
tags = ["code-gen", "python", "csv"]
+++

sales.csv 里有若干行销售记录，表头是 `category,amount`。
写一个脚本 sum.py 读取 sales.csv，按 category 汇总 amount，
然后按 category 字母序打印 `<category>: <total>`，total 保留两位小数。

用 `python3 sum.py` 验证输出正确。
