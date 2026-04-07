+++
id = "023-rust-trait-impl"
description = "为既有 trait 增加新实现并跑测试"
timeout_secs = 180
tags = ["multi-file", "rust", "trait"]
+++

src/shape.rs 里定义了 `Shape` trait（有 `area()` 和 `name()`）和 `Circle` 的实现。
src/main.rs 里有一段代码会遍历 `Vec<Box<dyn Shape>>` 并打印。

请：
1. 在 src/shape.rs 里新增两个实现：`Rectangle { w, h }` 和 `Triangle { base, height }`
2. 在 src/main.rs 里把这两个新 shape 加入 `shapes` 向量
3. 用 `cargo run --quiet` 验证，应打印出 3 行，每行 `<name>: <area>`，area 保留 2 位小数
