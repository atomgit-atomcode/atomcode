+++
id = "014-python-add-tests"
description = "给 util 补 pytest 测试"
timeout_secs = 180
tags = ["test-writing", "python"]
+++

mathutil.py 有几个 pure function 但没有测试。
请创建 test_mathutil.py，用 unittest（标准库，不要引入 pytest 避免安装依赖）为
每个公开函数写至少 2 个测试用例（正常路径 + 边界 / 错误路径）。

完成后用 `python3 -m unittest test_mathutil.py -v` 验证所有测试通过。
