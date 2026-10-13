# 实时思考快捷键

基线：release/v5.2.3，8171ae4f1。已有 coding/on_harness、runtime、harness/lib、agent_loop 与 diagnostics 的独立诊断改动，保留。

TUI 的 AssistantChunk 已经在流式阶段建立并更新 ModelThought，不依赖 AssistantMessage 或回合结束。体验缺口是 Ctrl+O 的通用折叠循环第一步只从 Hidden 变为 Folded。

Ctrl+O、Alt+R、/reasoning 共用 ToggleFold(reasoning) 入口，专门切换 Open/Hidden；通用展示循环及鼠标单块折叠保留。开启时尚无思考块，提示尚未收到内容，且保留 Open 设置，后续片段直接展示。不生成不存在的思考内容，不改 provider、会话事实、持久化或生命周期。

对照本地 Codex：接收思考片段更新缓冲与状态摘要，思考块完成后纳入详细历史；不要求任何客户端展示服务端尚未发送的内容。本产品已有更直接的实时思考块，因此复用它。

验证真实 AssistantChunk 只有思考没有正文/终态时首按显示、流式追加、空提示返回值、再按隐藏/重新展开及原有底部跟随测试。同步帮助、结束提示及中英文 changelog。
