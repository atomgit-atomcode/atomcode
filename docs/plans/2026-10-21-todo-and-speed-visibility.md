# Todo and generation speed visibility implementation plan

**Goal:** 修复单项待办更新被空数组清空，并让 tok/s 在生成后持续可见且排在缓存命中率前。
**Architecture:** capabilities 的同一参数解析决定全量计划/单项操作，供执行、reducer、TUI 和 coding 侧旁挂投影共同使用；非空完整列表优先，空 todos + add/update 为增量，单独 todos:[] 保留显式清空兼容。TUI Generation 是临时速率投影 owner：首片段起计时，排除 TTFT / 工具时间；完成时优先 provider completion token，等待下一请求时保留最后有效读数，新生成读数就绪后替换；新回合、会话切换、历史重放不捏造速度。显示固定 N tok/s，不加约/上轮约，位于缓存比例之前。
**Tech Stack:** Rust，serde_json，现有 SessionEvent / TUI 注入时钟。

基线 release/v5.2.4 d1b718479fae4a28e2cf35de02fb2b4e56a8347f；保留私有 crypto overlay / Cargo.lock。会话 00295baa-c20d-4dc4-a563-4f08180803b6 中多次 update 带 todos:[]，已核实工具返回 no tasks；不修改用户事件日志或旁挂。

1. todo.rs 共用参数正规化/形状判定，保留非空重规划及显式空计划；更新描述和回归。
2. generation.rs 支持短输出、真实 usage 校准及已结束请求的冻结速率；live.rs 按输入、输出、速度、缓存顺序显示。
3. 测试真实混合参数、执行/回放/面板一致性、速率冻结和等待、短输出、重试、错序、session 隔离及窄屏布局；跑受影响 crate 测试、fmt 和 compile gate。

验证结果：TUI 全量 1795 项、capabilities 默认 feature 全量 1098 项、coding 待办相关 46 项全部通过；`cargo fmt --all -- --check`、`git diff --check` 和 `bash gates/compile.sh` 通过。未做交互终端人工复现；速度为当前回合的临时展示，不从历史事件捏造计时。
