## 概述

为现有纯 Rust TUI 增加保守启用的鼠标交互、composer / transcript 文本选择与复制，并在原有 `/resume` SessionPicker 中展示所选会话的只读预览。

本 PR 基于 `release/v5.0.7` 独立抽取，不包含 HOME 项目选择器、媒体预览、OpenTUI/TS/JS 实验或其他后续能力；既有键盘交互和 TUI 布局保持不变。

**建议 PR 标题：**

```text
feat(tuix): add mouse text selection and session preview
```

**核心改动：**

- 终端能力与生命周期：
  - 仅在明确支持的本地终端启用 SGR mouse；SSH、tmux、JediTerm 和未知终端默认 fail closed
  - startup / suspend / resume / shutdown / Drop 对鼠标模式保持单一 owner，并处理部分写失败
  - 保留原 bracketed paste、Kitty keyboard、raw mode 与终端恢复顺序
- 标准化鼠标输入：
  - 保留 Down / Up / Drag / Move / Scroll、按钮、修饰键及 0-based 坐标
  - 非滚轮 pointer 在通用 input prelude 前处理，避免 ignored pointer 改变 App state
  - 滚轮继续复用现有 retained body scroll，并通过 epoch authority 关闭旧命中帧
- InteractionPublisher：
  - 只在完整 frame write + flush 成功后发布可点击坐标
  - generation / surface session / epoch 防止 resize、滚动、异步 worker 和 modal 切换后使用旧坐标
  - 写失败、逻辑帧切换和生命周期命令立即 fail closed
- Composer 文本选择：
  - cell 坐标精确映射到 UTF-8 byte boundary
  - 支持 ASCII、CJK、emoji ZWJ、Tab、soft wrap、hard newline 和反向拖选
  - typing、Backspace、Delete、paste 替换选区；Left/Right 折叠选区
  - history recall、submit、clear 与直接 buffer rewrite 清理旧选区，避免 stale range/panic
  - release 后使用现有 reverse style 绘制选区，不改变 composer 布局
- Transcript 文本选择与复制：
  - 基于稳定 CopyRun / run id，而不是易变的屏幕行号
  - soft wrap 拼接、hard newline 保留；compaction gap 不误拼接不可见内容
  - mouse Up 单次复制，优先使用系统剪贴板，OSC52 仅在终端能力允许时使用
  - worker stale epoch、overlay 覆盖、滚动和 resize 下保持 selection authority
- Session Preview：
  - `/resume` 选择变化时由单一后台 worker 读取 bounded meta + presentation，不加载完整 snapshot
  - 展示 cwd、provider/model 与最多 6 条净化后的最近消息摘要
  - generation + project bucket + session id 三重匹配，过期结果不重画当前 modal
  - 宽屏在现有 SessionPicker 区域内展开预览；窄屏保持原两行卡片布局
  - Enter 恢复、Ctrl+D 删除及键盘过滤语义保持原实现

## 使用方式

### Composer 文本选择

1. 在输入框输入包含中英文或 emoji 的文本
2. 鼠标按下并拖动选择文本
3. 直接输入、粘贴或按 Backspace / Delete，替换当前选区
4. 按 Left / Right 折叠选区并继续键盘编辑

### Transcript 文本复制

1. 在历史对话正文上按下并拖动鼠标
2. 松开鼠标后复制选中的语义文本
3. 软换行不会额外插入换行，真实段落换行会保留
4. Shift + drag 保留给宿主终端原生选择，不触发应用内复制

### Session Preview

1. 输入 `/resume`
2. 使用 Up / Down 或鼠标选择会话
3. 宽终端会在当前卡片内显示 cwd、provider/model 和最近消息摘要
4. Enter 仍执行原会话恢复；Esc 取消；Ctrl+D 两次确认删除

## 范围边界

- 不包含图片/视频/音频预览
- 不包含 HOME 启动项目选择器
- 不引入 TS/JS sidecar 或新 TUI
- 不更改 provider、模型调用或 session snapshot wire schema
- 不替换宿主终端的 Shift + drag 原生选择

## 关联 Issue

N/A

## 变更类型

- [x] ✨ 新功能
- [x] 🐛 稳定性修复

## 测试计划

- [x] `cargo test -p atomcode-tuix --lib --locked` — 1965 passed
- [x] `cargo test -p atomcode-capabilities --features session --lib session --locked` — 169 passed
- [x] `cargo test -p atomcode-daemon --lib legacy_convert --locked` — 57 passed
- [x] `cargo check -p atomcode --all-targets --locked`
- [x] `git diff --check`
- [ ] 手动验证：composer ASCII / CJK / emoji 正向与反向拖选、替换及折叠
- [ ] 手动验证：transcript soft wrap / hard newline 复制及 Shift + drag 宿主选择
- [ ] 手动验证：`/resume` 宽屏预览、窄屏兼容、快速切换不显示过期结果
- [ ] 手动验证：退出、panic、Ctrl+C 后终端 mouse/raw/paste 模式恢复

## 检查清单

- [x] PR 标题遵循 Conventional Commits
- [x] 基于最新 `release/v5.0.7`
- [x] 只包含鼠标文本选择、必要交互基础设施与 Session Preview
- [x] 不包含媒体、HOME picker 或 OpenTUI 实验
- [x] 破坏性变更：无；新增输入能力对不支持终端默认关闭
- [x] 保持现有 Rust TUI 样式、布局和键盘 authority

## 已知说明

- workspace 仍有既有 `retained.rs` `unused_assignments` warning，本 PR 未扩大处理范围。
- transcript drag 的视觉反馈受终端刷新调度影响；复制语义和资源清理由单元测试覆盖，仍建议在目标终端人工验收。
