+++
id = "013-python-fix-bug"
description = "修二分查找的 off-by-one bug"
timeout_secs = 180
tags = ["debug-fix", "python"]
+++

binary_search.py 里的 `bsearch(arr, target)` 有 bug，某些输入会返回错误结果。
test_binary_search.py 是用 unittest 写的测试，当前会失败。

请：
1. 读 test_binary_search.py 理解期望行为
2. 修复 binary_search.py 里的 bug（不要改 test）
3. 用 `python3 -m unittest test_binary_search.py` 验证全部通过
