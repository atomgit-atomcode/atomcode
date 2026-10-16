# Anthropic 原生缓存核心修复

缓存字段属于 provider 的 wire 编码，不进入 kernel 消息或 session 存储。参考 OpenCode 与 OMP，在最终消息合并之后设置断点：最后一个工具、system 文本块、最多两个最近的可缓存消息边界；总数不超过4。system 和需要标记的文本消息转为 Anthropic 内容块数组，文字不变。thinking、redacted thinking 与签名不改写，只有普通 text/image/tool_use/tool_result 块可标记。使用默认 ephemeral 有效期。

message_delta 的 usage 按字段存在性覆盖累计输入、输出、缓存读取和缓存写入量；显式零有效，缺失字段保留开始事件数值。

本次不新增运行时 owner、协议字段、用户设置、WebUI 展示或兼容端点降级。原生协议端点需要支持 cache_control；真实命中还受长度、有效期和前缀一致性影响，不通过真实付费请求验证。

验证：断点数量与位置、thinking 跳过和签名保持、用量累计覆盖与缺失保留；受影响 crate 单测、编译闸门和格式检查。
