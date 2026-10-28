# 原生 shell 预览的工具主说明

基线：release/v5.2.5，3e4e2559f。已有原生 output_tail_lines 通过机制回归，
但同模型四个探针仅 2 次采用；真实 astropy-13453 复验 0 次采用，仍以管道缩短输出。

按用户确认，仅在 bash / bash_start / bash_poll 的工具主说明中给出可执行的替代用法。
构建、安装和测试不要仅为缩短结果追加 head/tail/tee，也不要以 echo 状态收尾；
前台直接运行原命令并使用 output_tail_lines，后台原样启动、在 poll 设置预览。
不禁止用于真正数据处理的管道，不改变 shell 语义、退出码、审批或后台生命周期。

采用本地 OMP 工具主说明的直接指导方式；OpenCode/DSH 同样在主说明介绍自动裁剪，
Codex 在工具参数中提供输出预算，退出状态独立于裁剪。未发现这些运行路径统一开启
pipefail。全局 pipefail 会改变已有管道行为，不能解决以 echo 收尾的包装，因此不采用。

说明由工具自身提供，不在系统提示或 describe_self 再加副本。沿用 OutputArtifactsSvc
动态 provider：artifact 能力存在时说明具体参数和 fetch_output，不存在时不得广告。
共享 store、执行结果和 job owner 均保持不变；本轮其他 CLI/config/TUI 改动不在范围内。

验证：相关 crate 回归，实际装配的三工具说明随 artifact 行启停；同模型、原提示、
全新工作区运行原四个探针和 astropy-13453。保留负面结果，不按结果重抽样，分别报告
参数采用、真实退出状态、最终结论及官方评分，不以单题通过证明行为已可靠改善。

## 四家源码依据

- OpenCode e5a557efee：packages/core/src/tool/plugin/shell.ts，主说明建议依靠自动裁剪；shell/select.ts 使用原 shell -c。
- Codex 806d9732c9：codex-rs/core/src/tools/handlers/shell_spec.rs 的 max_output_tokens；tools/context.rs 独立渲染退出状态；shell.rs 组装 -c/-lc。
- OMP 4585e9da4a：packages/coding-agent/src/prompts/tools/bash.md 第 6 行直接禁止 head/tail/重定向；tools/bash.ts 渲染该主说明。
- DeepSeek Harness 687f282e86：packages/shell/tool-bash/src/index.ts 主说明介绍尾部裁剪和保存；bash-local/src/index.ts 原样执行 bash -c。

## 机制验证

Capabilities 1129/1129、Harness 271/271、Coding 1168 通过（9 跳过），gates/compile.sh 通过；fmt 与 diff 检查通过。Cargo.lock 未变，无私有依赖污染。测试覆盖 artifact 行撤下后三个工具不再广告不可用的参数，现有状态、游标和装配判据保持通过。

模型验证使用 /tmp/atomcode-preview-guidance-20261022 的独立检出，仅应用本轮五个源码/测试文件的补丁，排除其他 CLI/config/TUI 改动；二进制和补丁哈希在 SWE-bench logs/comparison/20261022-shell-preview-guidance/manifest.json。四个原始提示与 fixture 逐一校验一致，未根据结果重抽样。

## 同模型验证结果

四个原探针均采用新参数（4/4，前次 2/4），直接执行原命令；失败状态 23 和成功状态 0 均保留，捕获结果可读取，fixture 未改且仅执行一次。最终回答均正确区分成功/失败。

astropy-13453 全新工作区复验官方评分通过（resolved=true），推理 480.403 秒，76 次工具调用、11 次后台启动、13 次原生预览，无显式 shell sleep。前次同题原生预览为 0 次。本次处理依赖和运行更大范围 ASCII 测试，不能将耗时差异归因于说明改变。

实际命令仍有 build/test 的 tail 管道与重定向后 echo 收尾；部分 poll 虽使用原生预览，收到的依然是管道命令状态，错误输出与状态 0 并存。另有 poll 看到 EXIT:1 时仍报告 running，不能把打印值当作已收到真实进程终态。故只确认本样本采用改善，不能保证复杂任务退出状态已正确传回。

预测补丁包含 HTML 修改和两个自动 artifact 文件，评分前未人为剔除；原始样本、提示、补丁和评分报告均保留在独立 SWE-bench 产物目录。未根据结果修改提示、重抽样或扩展本轮生产改动。未验证 Windows 实机。随后按用户要求，将工具主说明与结果提醒一并审查提交。
