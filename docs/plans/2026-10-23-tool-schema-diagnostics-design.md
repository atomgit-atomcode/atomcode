# 工具定义开销统计

目标：在扩展内置工具按需化前，用实际请求定位剩余开销。当前已有 provider 输入分类和投影决策日志，本次补齐缺失信息，不另建统计状态或修改请求内容。

- provider 的既有 InputProfile 增加 largest_core_tools：最终 wire JSON 中最大的十个非 MCP 工具，仅含工具名和单个定义的 JSON 字节数，降序排列、同大小按名称排序。它是 tools_core 的子集视图，不加入分类总和。
- 在现有请求投影点记录投影前、保留、隐藏的工具数量与定义 JSON 字节数；分别用 catalog 区分 builtin_search 和 mcp_search。已挂载的 auto 投影无需隐藏，或发现入口被策略禁用时，记录保留全部、隐藏为零。显式关闭搜索不额外挂投影处理，仍由 provider 输入统计显示实际全量开销。
- 不记录描述、参数正文或凭据；不改变工具注册、选择算法、预算、审批和会话格式；不增加模型输入。
- provider wire 字节和中立 ToolDef 字节因封装、参数装饰不同而不等价，不混加、不当作计费 token。

基线 release/v5.2.5 / 60173252c，tracked clean。验证跨 provider 排行、限额与隐私、分类精确计量、原投影测试，以及受影响 crate 测试和编译/格式闸门。

验证结果：input_profile 5 项专项测试、capabilities 1134 项、coding（atomgit feature）1181 项通过（9 项跳过）；编译闸门、fmt/diff 检查通过。Cargo.lock 无改动，私有依赖判据为 0。
未重复真实模型调用：本次只改日志计量与文档，不改变模型输入、工具选择或执行。
之前隔离实测的中立 ToolDef 样本中，较大项为 memory 3162、bash 2943、request_user_input 2925、todowrite 2593 字节；该样本不含 MCP/Web/Review，不能代替具体用户请求的最终 wire 排行。
