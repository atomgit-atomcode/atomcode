+++
id = "021-rust-fix-lifetime"
description = "修 lifetime / borrow 错误让 cargo build 通过"
timeout_secs = 180
tags = ["debug-fix", "rust", "lifetime"]
+++

src/main.rs 当前无法编译：`longest_word` 函数的 lifetime 标注不对，
并且 main 里有一个 borrow-after-move 错误。

请：
1. 先跑 `cargo build` 看报错
2. 修复 src/main.rs 让它编译通过
3. 不要改变函数签名里的参数数量 / 命名 / 返回类型结构（只补全 lifetime）
4. 程序行为：从输入的 &str 中找出最长的单词并打印
5. 最后用 `cargo run --quiet` 验证，能打印出最长的单词（"refactoring"）
