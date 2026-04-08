+++
id = "010-rust-refactor"
description = "把臃肿 main.rs 拆成 lib + main"
timeout_secs = 180
tags = ["refactor", "rust", "bash-verify"]
+++

src/main.rs 里所有逻辑都堆在 main() 里。请重构：
- 把求和函数挪到 src/lib.rs，命名为 sum_iter
- main.rs 只负责调用 sum_iter 并打印结果
- 完成后用 `cargo build` 验证编译通过
