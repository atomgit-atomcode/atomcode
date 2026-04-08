+++
id = "020-rust-cli-parse"
description = "用 clap 写一个简单 echo-like CLI"
timeout_secs = 180
tags = ["code-gen", "rust", "cli"]
+++

当前目录有一个 `Cargo.toml`（已包含 clap 依赖）和空的 `src/main.rs`。
请实现 main.rs，用 clap derive API 定义 CLI：

- 位置参数 `text: String`（必填，要打印的内容）
- `--upper` flag：打印前转大写
- `--repeat <N>` 选项，默认 1：重复打印 N 次

然后用 `cargo run --quiet -- hello --repeat 2 --upper` 验证输出 `HELLO` 两行。
