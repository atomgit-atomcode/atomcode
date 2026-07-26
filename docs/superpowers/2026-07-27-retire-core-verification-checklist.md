# 退役 atomcode-core — 真机验证清单（release/v5.0.3）

> 全部代码改动已 push，三路独立 code-review 均 SHIP（无 correctness bug）。
> 但**全程未真机**。上线前请人工过一遍以下路径（每条勾选）。
> 环境：webui + 真 provider（GLM/deepseek/openrouter 等）。⚠️ 用一个**文字模型**做主模型才能验到 vision 的图片剥离逻辑。

## A. Vision 图片预处理（B + review 修复的重点）
需先在配置里设 `vision_preprocessor_provider` 指向一个**视觉模型**（如 qwen-vl / glm-4v），主模型选**文字模型**。

- [ ] webui `/chat` 贴一张图 → 应生成 `[图片内容（由 <vl_model> 识别）]\n<描述>`，且原图**不发给文字模型**（不报 400）
- [ ] webui `/live` 贴图 → 同上
- [ ] **错配 case（review 修复点）**：把 `vision_preprocessor_provider` 改成一个 **config 里不存在的名字**，贴图 → 应显示 `[图片识别失败]` 且**不报 400**（修复前会把原图漏给文字模型触发 `not a multimodal model` 400）
- [ ] `vision_preprocessor_provider` 留空/不配 → 贴图应**原样透传**（不加 marker；若主模型是视觉模型则正常多模态）
- [ ] vl_model 名带 `vendor/` 前缀（如 `Qwen/Qwen3-VL`）→ toast/marker 里应显示**去前缀**的 `Qwen3-VL`
- [ ] TUI（cli）贴图 → 同样验一遍（cli 走 `VlImagePreprocessor`，独立实现）

## B. Permission 审批流（D）
webui 用 **Build 模式**（会弹审批）：

- [ ] 触发一个需审批的工具（如工作区外 bash / 写文件）→ 弹审批
- [ ] 点 **Allow**（一次）→ 放行，同类下次**再弹**
- [ ] 点 **Always allow** → 放行，同类下次**不再弹**（本会话记住）
- [ ] 点 **Deny** → 拒绝，工具不执行
- [ ] Plan 模式：只读放行、写操作被挡

## C. 会话传输（C2，webui 命脉，改动最大）
- [ ] 新建会话，发几轮 → 正常
- [ ] **续聊历史会话**（/resume 选旧会话）→ 历史正确渲染（工具调用/结果/图片缩略图/thinking）
- [ ] 会话中 **Esc / Cancel 中止** turn → 干净中止，不卡死，历史不丢
- [ ] **压缩后续聊**（跑到高上下文触发压缩，或 /compact）→ 压缩后继续对话正常，旧摘要（cold summary）保留
- [ ] **页面刷新重连** → 会话恢复，不丢轮次/时间戳不跳
- [ ] 两个 tab 同一会话并发 → 不串台

## D. Skill 列表（E1）
- [ ] webui `/skills`（或技能菜单）→ 列出用户可调用技能
- [ ] **装一个插件**（含 skill）后 `/skills` → 插件技能出现（带 `<plugin>:<skill>` 命名空间）
- [ ] ⚠️ 已知 LOW 差异：若你用 `ATOMCODE_HOME` 重定位过 `~/.atomcode` 或 sudo 跑，webui `/skills` 可能漏掉 `.atomcode/*` 或真实 home 的技能（capabilities reload 用 `dirs::home_dir()` 单一解析；见 review 发现 #E1）。普通安装不受影响

## E. /compact（A）
- [ ] webui 触发 `/compact` → 返回 applied + before/after tokens 合理，无 panic
- [ ] 用一个**不存在的 provider 名** /compact → 应干净报错 `Provider 'x' not found`（review 修复点）

## F. Trace / 诊断（plan A，trace 搬到 daemon）
- [ ] 设 `ATOMCODE_TUIX_LOG=/tmp/atomcode.log` 起 webui/TUI，跑一轮 → 日志文件正常写入（daemon 的 `ctrace!` + tuix 各自 append 同文件不冲突）

## G. 冒烟
- [ ] webui 正常起、登录、选模型、发消息、工具执行全链路
- [ ] TUI（cli）正常起、发消息、工具执行
- [ ] 插件安装/卸载/marketplace（E2，plugin 迁 capabilities）
- [ ] LSP 诊断（若你的构建开了 `lsp` feature —— 默认不开，属 pre-existing opt-in）

---

## 已知非阻塞项（review 发现，可上线后清理）
1. **[LOW] cli/daemon/tuix 仍声明 `atomcode-core` 死依赖** —— 零符号使用，可删（见下方"core 可整删"）
2. **[LOW] daemon 少声明 capabilities 的 `skills`/`session` feature** —— 现靠 coding 传递启用，脆但不崩
3. **[LOW] E1 loose-skill home 解析忽略 ATOMCODE_HOME/sudo**（见 D 第 3 条）
4. **[FINE] ~25 处过期 doc 注释**提到已删的 `core::*`（仅注释）
