# Provider 表单窄屏编辑设计

## 问题

`/provider` 的账号 Add/Edit 表单把名称、Base URL 和 API Key 保存为普通
`String`。输入只能追加到末尾，Backspace 只能尾删；左右键仅在协议字段切换。
表单值作为普通 `PluginInfo` 菜单行渲染，终端较窄时右侧被裁掉，因此长 URL
虽然仍完整保存在内存中，用户却看不到末尾，也无法移动到中间修改。

## 设计

Add/Edit 表单各维护当前文本字段的 UTF-8 字节光标。切换焦点时，光标移动到新
字段末尾。名称、Base URL 和 API Key 统一支持字符插入、粘贴、Backspace、
Delete、Left、Right、Home 和 End；协议字段继续使用 Left/Right 切换协议。

渲染保持现有 `MenuPayload` 和 `PluginInfo` 协议不变。Provider 面板根据当前终端
列数计算字段值预算，并生成一个包含可见光标 `│` 的临时显示投影。值超宽时以
光标为锚点选择窗口；左侧或右侧存在隐藏内容时分别显示 `…`。这只改变表单显示，
不会截断或重写配置值。

## 验证

- ASCII 与多字节字符下的插入、移动和删除保持 UTF-8 边界安全；
- 粘贴发生在当前光标位置，粘贴后光标位于新增文本之后；
- 窄屏且光标位于 URL 末尾时显示 URL 尾部和左省略号；
- 光标位于中间时同时显示左右省略号；
- 宽屏时显示完整值；
- 既有 Provider Panel 测试与 `atomcode-tuix` 库测试保持通过。
