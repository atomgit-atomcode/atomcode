# models.dev 模型价格目录设计

## 目标

AtomCode 在用户未显式配置模型价格时，从 `https://models.dev/api.json`
获取按 Provider、模型区分的公开价格，用于 `/cost` 的本地估算。价格目录不可
影响模型调用、运行时创建或已有 Token 统计。

## 状态所有权与边界

- `ProviderConfig::pricing` 仍是最高优先级的用户显式覆盖。
- AtomGit CodingPlan 下发的全零价格仍表示套餐额度内，不使用公网 API 价格。
- models.dev 客户端属于 `atomcode-capabilities::provider` 的外部 Provider 元数据
  能力；它不进入 kernel，也不新增 runtime owner。
- `atomcode-coding` 在创建或替换 runtime generation 时解析价格，并将结果冻结到
  `CodingAgentConfig::pricing`。session usage 继续保存当时的价格快照，`/cost`
  不使用当前目录重算历史费用。

## 匹配规则

依次尝试：

1. 用户显式 `ProviderConfig::pricing`；
2. AtomGit CodingPlan 的显式全零价格；
3. 配置了 `base_url` 时，将它与目录 Provider 或模型 API 地址规范化后精确匹配；
4. 没有配置 `base_url` 时，才用 AtomCode provider key 与 models.dev Provider ID 精确匹配；
5. 在唯一 Provider 内精确匹配模型 key 或模型 `id`。

URL 规范化只处理大小写、默认端口、末尾斜杠，以及末尾 `/v1`。不做域名相似、
模型名称相似或代理上游猜测。匹配不唯一、价格字段缺失或数值非法时均返回无价格。

## 缓存与失败语义

- 缓存保存到 AtomCode cache 目录的 `models-dev.json`；
- 24 小时内直接使用；
- 有过期缓存时通过 async HTTP 最多等待 3 秒刷新，失败继续使用旧数据；
- 首次无缓存时最多等待 3 秒；
- HTTP 等待不阻塞 Tokio executor；解析、写盘和 try-lock 失败均降级；
- 无可用价格时 `/cost` 保留 Token 明细，但不展示预估费用。

## 验证

- Provider ID 精确匹配；
- 任意 provider key 通过官方 base URL 匹配；
- `/v1` 与末尾斜杠规范化；
- 自定义代理、歧义 Provider、未知模型不匹配；
- 显式价格优先，显式全零保持免费；
- 缓存新鲜度、损坏缓存和刷新失败降级；
- CLI/TUI/daemon runtime 构建、模型 reload、task 子代理使用相同解析入口。
