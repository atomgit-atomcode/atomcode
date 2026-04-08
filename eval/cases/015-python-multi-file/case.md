+++
id = "015-python-multi-file"
description = "写一个多文件的 mini todo CLI"
timeout_secs = 180
tags = ["multi-file", "python", "cli"]
+++

在当前目录实现一个命令行 todo 工具，结构如下：

- `todo/__init__.py`（空即可）
- `todo/store.py`：定义 `TodoStore` 类，支持 `add(text)` / `list_all()` / `done(idx)`，
  数据持久化到 `todos.json`（当前目录）。
- `todo/cli.py`：用 argparse 实现子命令 `add <text>` / `list` / `done <idx>`，调用 store。
- `main.py`：入口，执行 `python3 main.py <subcommand>` 应能工作。

list 输出格式：`<idx>. [<x_or_space>] <text>`，x 代表已完成。

最后用 bash 跑一遍验证：
```
python3 main.py add "buy milk"
python3 main.py add "write code"
python3 main.py done 0
python3 main.py list
```
应看到两条任务，第一条已勾选。
