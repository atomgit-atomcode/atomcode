# Plan / Build 模式模型

在 TUI 的 `/config` 中搜索 `modes` 或“模式与模型”，可以为 Plan 和 Build 分别选择已配置的模型及其支持的思考强度。WebUI 的模式菜单 →“配置模式模型…”和模型设置 →“模式与模型”提供相同设置。

保存只更新偏好，不立即改变当前模型，也不会中断正在运行的任务。下一次真正跨模式切换时应用绑定；重复选择当前模式保留当前选择。未配置时沿用原有行为。

## 配置文件

`model` 引用已有 `[models.<id>]` 的选择 ID，也兼容已有的 legacy provider 选择 ID。强度使用已有名称 `reasoning_effort`。

```toml
[provider_accounts.my-server]
provider = "openai"
base_url = "https://your-server.example/v1"
api_key = "${MY_SERVER_API_KEY}"

[models.planner]
account = "my-server"
model = "your-pro-model"
reasoning_effort_levels = ["low", "high"]

[models.executor]
account = "my-server"
model = "your-flash-model"
reasoning_effort_levels = ["low", "high"]

[modes.plan]
model = "planner"
reasoning_effort = "high"

[modes.build]
model = "executor"
reasoning_effort = "low"
```

- Plan 模型留空：跟随进入前的模型，仅应用显式设置的强度。
- Build 模型留空：从 Plan 返回时恢复进入 Plan 前的模型；显式 Build 强度覆盖恢复的强度。
- 强度留空：使用模型配置；`default` 表示 API 默认。只接受目标模型支持的档位。
- `/model` 或强度菜单的手动选择临时覆盖模式绑定，下次跨模式切换重新应用对应偏好。手动选择仍保留其原有配置保存规则；绑定自动切换不会写入 `default_model`。
- 模型不存在、强度不支持或 provider 准备失败：明确报错并保留原模式、模型与强度。
- 当前任务运行中且切换涉及模式绑定：先停止任务，再切换。完全未配置模式绑定的用户保留原有模式切换行为。
- 目前绑定仅针对 Plan 与 Build；Accept Edits 和 Auto 不配置独立模型偏好。

WebUI 同步会话由当前 CodingRuntime 完成切换；独立会话将选择随每次请求传递，不写入全局默认模型。失败时界面保留原选择并显示错误。
