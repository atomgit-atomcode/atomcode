# AtomGit 工具按需发现

已批准的第一阶段仅涵盖 atomgit_repo、atomgit_issue、atomgit_pr、atomgit_api。
保留 ToolBox 注册、权限和执行入口；通过 AgentRequest 投影减少发送的 schema。
常驻 builtin_search 展示当前获准工具的短目录，搜索结果只记录名字和短描述，下一次请求发送完整 schema。
与 MCP 共用选择算法但使用独立搜索名、结果配对与作用域；当前 agent 的 ToolsSvc 是目录权威。
不增加会话状态或持久化格式。压缩移除配对后恢复短目录，可重新搜索。
搜索不可用时保留全量 schema；配置 tools.atomgit.tool_search=false 回退全量。
失败不执行被搜索工具，不放宽原有权限。核心工具和其他内置工具保持现状。

基线：release/v5.2.5 / a8290fd5e4f7c49a0c599805da9f5172f8a92ae7，tracked clean。
验证：原 MCP 回归、AtomGit 选择/压缩/权限/子 agent 隔离、配置与行装配、crate 测试、编译闸门、fmt。

## 测量与验证说明

生产 runtime mount 的离线测试（quiet_options，未启用 MCP/Web/Memory/Review）对比同一组工具：
完整工具定义 JSON 41,910 → 35,034 字节，净减少 6,876 字节（16.4%），已包含新增搜索工具及短目录。
这是 JSON 字节测量，不是模型 tokenizer 或网关计费结果；不代表全部输入上下文减少 16.4%。
搜索后模型收到的 AtomGit 完整定义与 eager 请求完全一致（包括原有 intent 参数装饰）。

配置 tools.atomgit.tool_search 默认为 true，在 /config 可见；false 恢复 eager。
配置应用沿现有 CapabilityReprepare 路径，编辑文件不会直接修改正在运行的 generation。

开启 atomgit feature 时，原有产品身份测试把合法第三方工具说明里的 AtomGit 也判为身份泄漏；
仅在该测试夹具关闭 AtomGit 工具，保留原身份断言，生产身份与能力注册行为不变。
第一阶段离线验证未运行真实模型/SWE-bench 对比；后续真实模型探针见下。

最终验证：atomcode-config 339 项通过；atomcode-coding --features atomgit 1177 项通过、9 项跳过；
包含原 MCP 搜索回归、真实生产 mount 对比与无 AtomGit 的装配回归。
gates/compile.sh、cargo fmt --all -- --check、git diff --check 通过；Cargo.lock 无改动且私有依赖判据为 0。

## 真实模型发现探针

TaoToken/deepseek-v4.1-flash-1017，沿用本机模型的 max effort；三个预先确定的自然语言提示各跑一次，未提示使用搜索工具。
使用生产 runtime mount + 原生 OpenAI-compatible provider，目录为空且隔离；仅真实派发 builtin_search，其他模型工具调用在派发前拦截，未创建远端仓库。
三次首次请求均无 AtomGit schema，三次均主动搜索。参数齐全的中文私有项目、英文公开仓库两例，均在第二次请求拿到 atomgit_repo schema 后发起 create，名称/描述/private 正确。
简短请求亦先搜索，但还尝试本地目录/仓库列表查询；这些调用被拦截，没有验证后续补问或创建行为。
发现精度问题：精确搜 atomgit_repo 会因其他工具描述提及它而多加载 atomgit_api；英文组合查询加载四个 AtomGit schema。下一步宜先优化匹配精度，再考虑扩展隐藏范围。
原始请求、调用与探针代码在 SWE-bench/logs/comparison/20261022-atomgit-discovery，report.json 记录范围、结果与代码散列；未验证实际 AtomGit API 创建、鉴权和所有模型的发现可靠性。

## 搜索精度调整

基于自然语言探针的结果，仅对 AtomGit 内置发现增加明确工具名匹配：查询中包含完整工具名时，只保留点名工具；可同时点名多个工具。
名称匹配忽略大小写，允许标点/中文作为边界；不使用子串猜测。未点名时保留关键词搜索与分页浏览，MCP 匹配语义不变。
仍只在当前获准目录内搜索，点名不存在/被禁用的工具返回空结果，不替换成描述中提及它的其他工具。预算约束和完整 schema 发送保持原路径。
本阶段基线 HEAD f32fe488fb121a617ff4140eec612d3ab09622a0；保留上一阶段未提交改动和其他工作区文件。

精度调整验证：11 项搜索专项测试通过；完整 coding（atomgit feature）1178 项通过、9 项跳过；编译闸门与 fmt/diff 检查通过，Cargo.lock 无改动。
同样三个自然语言用例各复测一次，均主动搜索且只加载 atomgit_repo；两个完整请求仍正确发起 create（执行前拦截）。
英文用例第二轮 AtomGit schema JSON 从 7663 字节降至 1980 字节，中文两个用例从 2926 降至 1980 字节；仅统计 AtomGit 定义，非总输入/计费 token。
结果存于 SWE-bench/logs/comparison/20261022-atomgit-discovery-precise/report.json；临时网络探针源文件已移出仓库并随结果保留。

## 提交前审计（2026-10-23）

发现并修复环境变量覆盖边界：ATOMCODE_ATOMGIT=1 能覆盖原配置启用工具，但发现行原先再次检查旧 atomgit_enabled，导致已挂载的工具退回 eager。
行配置现在只传递 atomgit_tool_search；是否挂载发现工具由实际获准 ToolBox 目录决定，避免重复判断功能是否启用。新增生产 mount 回归先确认旧实现失败，再验证修复通过。
预算不足提示保留 MCP 原有 off 配置值，并为内置发现明确 tools.atomgit.tool_search=false。
复核搜索与执行权限隔离、子 agent 目录、MCP 独立选择、配置回退和事件日志：不新增持久化状态，不改变审批路径或凭据归属。
最终 coding（atomgit feature）1179 项通过、9 项跳过；config 339 项通过；编译闸门、fmt/diff 检查通过；Cargo.lock 无改动且私有依赖判据为 0。
