# 无工具运行契约的目录收口

## 问题

`PrepareOptions.tools=false` 已声明发布空工具目录，CLI 的 `--no-tools` 使用该选项。但它此前只约束 prepare-time 注册，原生 plexus 行仍可自行挂工具。隔离产品目录与本地假 provider 的探针能够复现：关闭工具后仍发布文件和 shell 工具，且可以读取仅测试创建的 fixture。

## 实现

Owner 保持为 CodingRuntime 的 `tree_inputs`。在用户和宿主层之后，用既有 `tools` 行设置 `exclude=["*"]`，统一约束发布与执行，不引入第二套 registry/gate。显式关闭工具不能被 host include 重新开放。工具开启时保留原有层顺序与策略。

模型、能力、配置及目录重建继续经过同一装配边界，关闭状态随 prepare 保留。不修改工具协议、会话格式、运行模式或审批生命周期；不把“隐藏描述”当作执行隔离。

## 验证

RecordingProvider 检查真实请求中的工具定义，覆盖初次请求、provider swap、capability reload、config reprepare、change_directory，以及显式 include bash 的宿主层。运行时、挂载、工具策略与宿主扩展测试共131项通过。

可复用原生离线探针：`evals/design-convergence/no_tools_probe.py`，说明见同目录 `OFFLINE-PROBE.md`。三个检查分别验证空目录、伪造调用不能读取 fixture、正常启用工具时仍可读取。探针不使用真实模型、凭据、用户会话或用户文件，超时和契约违反均以非零状态退出。

该修复解决 headless 契约与评测隔离，不宣称能使 TUI 的设计讨论自动收敛。生产 persona、默认模型、推理强度及超时设置均未修改。
