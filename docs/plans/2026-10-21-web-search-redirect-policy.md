# web_search 重定向策略加固

基线：release/v5.2.4，4be7097efb4d97ff894a7750fcf6e11ceb78c715。工作树只有无关未跟踪文件。

共享 WebSearchTool HTTP 客户端拥有 Exa / DuckDuckGo 请求策略；固定 HTTPS 端点和代理策略保持。禁止自动重定向、只允许 HTTPS、关闭自动 Referer。两种后端遇到 3xx 立即失败，不读取 Location 或响应体。错误不包含请求 URL。此变更关闭重定向入口，不替代 DNS / 网络出口层的完整 SSRF 防护。

验证：生产客户端拒绝非 HTTPS；本地 HTTP fixture 仅在测试中放宽 HTTPS，验证共享策略不会跟随重定向；覆盖 3xx 安全错误。运行 capabilities web feature 测试、格式检查及 compile gate。
