+++
id = "022-rust-add-test"
description = "给 pure function 补单元测试"
timeout_secs = 180
tags = ["test-writing", "rust"]
+++

src/lib.rs 里有几个 pure function（fizzbuzz / is_palindrome / gcd）但没有测试。
请在同一个文件底部加上 `#[cfg(test)] mod tests { ... }`，为每个函数写
至少 2 个测试用例（至少 1 个正常路径 + 1 个边界/特殊值）。

完成后跑 `cargo test` 验证全部通过。
