# WebUI 技能刷新与思考强度一致性

技能目录权威仍是 daemon GET /skills 的即时扫描，前端三个入口共用一个可刷新列表与 hook；每次打开刷新，失败与空列表分开，失败可点击重试。失效清除旧项目列表并使迟到响应无效，/reload 成功及 /cd 成功时失效。加载中的请求可合并，但重新打开菜单会请求新数据，不无限重试。

/live/reasoning_effort 复用配置层 REASONING_EFFORT_LEVELS 解析规范值，包含 xhigh；沿用 endpoint clamp 校验、唯一 runtime reload 及 revision 条件回滚，不新增生命周期或延迟应用机制。未声明、空列表或全为未知等级时，保留现有 unrestricted 回退语义，不宣称缺少声明就会拒绝。前端 turn 运行中禁用思考强度选择，保存中禁用重复操作，ActiveTurn 拒绝及其他失败有中英文提示。请求成功后刷新模型列表失败不应回滚已经保存的值；切换模型后迟到的失败不回滚其他模型。

验证共享列表重新打开/失败重试/并发失效，API 的 xhigh 请求与 ActiveTurn 拒绝，规范等级和 endpoint 白名单；运行 WebUI 测试、类型检查、构建及 daemon 测试与编译门禁。
