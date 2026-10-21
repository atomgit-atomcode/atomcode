# 纯文本块显示

范围：TUI Markdown 中标为 text / txt / plaintext（忽略大小写）的围栏块。直接显示字面文本，继承正文样式，保留换行、缩进和自动折行，不画语言标签、代码底色或语法高亮。不重新解析块内 Markdown。已闭合和流式未闭合块使用相同策略；其他代码语言和无语言围栏保持现状。

基线：release/v5.2.4，6ccb69242。保留现有 review_tool.rs / commands.rs 等其他改动。验证渲染回归、TUI crate 测试和格式检查。
