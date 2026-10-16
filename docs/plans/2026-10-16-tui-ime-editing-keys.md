# 新 TUI 的删除按键兼容

基线：`release/v5.2.4`，`3d1eedccf1367cafd280e65f5ba2412e4dd1fe08`，工作区干净。

## 问题与范围

用户反馈 5.2.3 d8918f0 在 Windows SSH 中文输入时，行首字符无法 Backspace 删除；具体 SSH 客户端、远端 OS，以及是已上屏文字还是候选拼音仍未确认。不能把兼容项补齐直接宣称为该实机问题的根因修复。

本地 Codex 806d9732c9 对照发现：非 ASCII 首字即时插入，非文本编辑前先提交粘贴缓冲，接受 Shift+Backspace 和 Repeat，按 Unicode 字素边界删除。AtomCode 当前新旧输入路径已有“文字在前、结束聚合的删除键在后”的顺序；经典主输入也支持 Shift+Backspace/正常路径 Repeat。此次仅补新 TUI 缺失的编辑按键，保留粘贴判定、短暂聚合窗口、classic 与字符删除边界。

## 实现

- 默认 keymap 将 Shift+Backspace 绑定到 Backspace；保留修饰键，不在输入边界把所有 Shift 或 Ctrl 一律抹掉。
- crossterm 转换仅放行删除、光标移动的无修饰/Shift Repeat，以及 Ctrl+H / Ctrl+? 删除别名的 Repeat。普通字符、Enter、Esc、Tab、功能键和面板快捷键的 Repeat 维持忽略，Release 维持忽略。
- 指针交回终端时，重复的上下箭头与普通箭头走相同 ArrowBurst 路径，避免因事件类型不同而跳过既有滚动仲裁。
- 不增加 IME composition 状态机，不改 runtime、会话、协议持久化或产品目录。范围限于 driver 输入事件与默认按键。

## 验证

通过真实 pump -> keymap -> composer 的零间隔脚本验证中文首字删空、Shift/Repeat/Ctrl+H、过量删除为空操作、重新输入不复活旧字；通过多行粘贴接连续 Backspace 验证先落文字再删除并跨越换行。额外检查自动换行首字删除、Repeat 不触发提交/退出/面板快捷键、Release 不产生编辑，以及箭头重复在指针交回前后的路由。零间隔脚本只断言顺序，不断言真实时间，使用暂停时钟；现有真实间隔判据保持原样。

需运行 atomcode-tui crate 测试、编译闸门、fmt 与 diff 检查。模拟事件不能替代 Windows SSH + IME 实机验收。

验证结果：TUI 全套 1687 项通过，无跳过；fmt/diff 检查通过。首次编译闸门的实际 workspace/feature 编译通过，但 feature 阴性对照自检失败；保留每次 Cargo 调用原始输出重新运行完整脚本后通过，确认 workspace 编译成功、两个 canary 分别按预期判红，feature canary 在 workspace 默认检查下不报错。本任务未修改闸门或降低其判据；首次自检失败未能重现。
