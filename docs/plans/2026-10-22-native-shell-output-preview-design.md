# Shell 原生预览：现状、验证和最小实现边界

检查基线：release/v5.2.5，946bcb7f239a8cf3db3de3935ae82a8e9218ab6c。
最初先增加机制回归，确认现有 artifact 不丢状态；随后按用户确认实现可选原生预览。
命令执行语义、默认调用、审批和持久化格式不变。

## 四家对照后的事实

OpenCode（e5a557efee）的 shell 描述明确告知自动预览和完整输出文件；
Codex（806d9732c9）在结果头单独渲染进程状态，并对输出体应用预算；
OMP（4585e9da4a）将 raw artifact 与预览区分；DeepSeek Harness（687f282e86）
通过 spillPath 保存可获取的长输出，并把退出状态放在输出标记之外。
位置详见 `2026-10-22-shell-result-guidance.md` 的四家源码表。
共同点是让输出组件缩短结果，而不是让模型为节省输出自行构造 shell 管道。
它们也不能恢复任意管道中未被 shell 返回的左侧退出状态。

AtomCode 已有同类主干，不能把它描述为缺少 artifact 或 fetch 工具：

- `capabilities/tools/bash.rs`：通过 Shell 运行命令，用真实退出状态生成结果。
- `harness/plugins/policy.rs::OutputArtifact`：结果超过预算时交给现有 ArtifactMiddleware。
- `capabilities/tools/output_artifact.rs`：默认超过 50KiB 保存捕获结果，保留首 32KiB
  和尾 12KiB；4MiB 为存储上限。保存失败或超过上限时明确说全文不可用。
- `tool-output-artifact` 行挂载 fetch_output，coding 传入 session 的 artifact 目录。
- `capabilities/tools/bash/background.rs`：原始输出只有 256KiB 有界尾缓冲；被丢弃的
  字节到不了 artifact 行，因此该行不能令后台输出恢复为完整日志。

机制不等于端到端已经解决：模型为满足“只返回末尾 12 行”会先运行 `cmd | tail`
或 `cmd; echo status`，此时工具收到的原本就是过滤/包装命令的状态。
实现前没有按调用请求有限行预览的工具参数；本轮补齐此能力，见下文实际装配。

## 本轮机制验证

新增 coding `mechanism_rows` 集成回归：直接运行一个输出约 116KiB 的失败命令，
不接管道、不打印伪状态，以 23 退出；错误标记位于自动预览省略的中间。
验证预览有真实退出码和 fetch_output 入口、捕获文件保留全部首尾字节与退出码，
通过现有 ArtifactStore 的分页接口读回中间错误。
整个 mechanism_rows 的 23 项测试通过。
这证明前台现有输出层不必依赖 shell 管道，也不会在这一场景丢掉退出状态；
不证明模型已会正确使用它，不覆盖 Windows 实机或后台超过 256KiB 的完整性。

## 实现边界

推荐由现有输出组件提供按调用的可选尾部预览，例如 output_tail_lines，默认不变。
命令依原样执行，结果体在执行后做预览；真实退出/取消/超时标记必须独立保留。
该组件复用 session 的现有 ArtifactStore 和 fetch_output，不另建日志存储或 job owner。
schema、参数验证、实际预览和存储必须配套，不能只宣称一个底层忽略的参数。
没有对应输出组件时不能广告此参数；非法或不支持的参数必须在执行前明确失败。

前台与 bash_poll 均应纳入设计；poll 的游标、回收、wait_ms 和状态终态不受预览影响。
后台只能保存“本次仍然捕获到的结果”，有丢弃时必须继续保留原截断告知，
不得承诺该 artifact 是任务的全部原始输出。后台连续原始落盘另作资源生命周期设计。

验证标准：用户不需要 shell 过滤或 echo/exit 包装即可请求短预览；
真实失败仍返回对应状态，成功/取消/超时可区分；预览缺失内容可分页读取，
存储失败/超过容量不能伪称有全文；相同模型探针不应以文字猜测替代工具状态。
新增参数涉及工具协议，需要覆盖默认调用兼容性、实际装配与各 driver 共用入口。
不修改 kernel ToolResult 公共持久化格式、不启用全局 pipefail、不重写用户命令。

## 不选的路线

继续叠加抽象“保留状态”提示：P1 对照已有失败证据，不能算工具能力修复。
靠错误文本推断执行失败：输出不可信，不能覆盖真实进程状态。
直接提高后台缓冲到“足够大”：会按并发 job 数放大内存，仍不能保证完整。
一次补齐后台全文落盘：涉及磁盘上限、保留期限、取消与清理，风险高于本步。

## 实际装配

bash 与 bash_poll 新增可选 output_tail_lines（1–2000），预览体另限 12KiB，
UTF-8 对齐；真实进程状态独立于预览保留，不从输出内容猜测失败。
请求短预览时，省略的已捕获结果存入已有 ArtifactStore，4MiB 上限和保存失败告知沿用。
后台保留原缓冲截断告知，仍不承诺恢复已丢弃字节。

原 tool-output-artifact 行是唯一 store 与 fetch_output 的装配所有者，
通过 Core 类型 OutputArtifactsSvc 提供原有 Arc，不建立第二个目录或日志管理器。
两个 shell 行声明可选 uses，使用宿主传入的 provider 按 schema/调用实时解析 store，
不在挂载时缓存一个当时可能尚不存在的可选服务。通用 harness 无 artifact 行仍正常运行；
无 store 不声明参数、强行传入则在启动或消费输出前报错。
关闭 artifact 行后，参数随实时 schema 撤下；不修改 ToolBox 的工具身份或授权委托。
capabilities 不依赖 harness，宿主 provider 只传 ArtifactStore，不携带 runtime 生命周期。

工具参数自己的 description 说明用法，不再另写一份动态系统提示；
原 P1 的“自行保存日志/拼 shell 返回码”指导已替换为简短的保留状态原则。

新增回归覆盖前台真失败、成功、显式管道不改写、非法参数无副作用；
后台预览/游标/原截断告知；UTF-8、单行字节预算、保存失败和容量上限；
两种 shell 行及 artifact 行关闭后的 schema 和 fetch_output 生命周期。

## 实现验证结果

- Capabilities 默认回归 1129/1129；Harness 271/271；Coding 1168 通过、9 跳过。
- 新增的直接退出/成功/显式管道补充断言单独复验通过。
- tools-only lib 编译、完整 gates/compile.sh、fmt 与 diff 检查通过；Cargo.lock 未变。
- 同模型原四个 P1 探针使用固定新二进制复验：后台失败和前台成功采用新参数，
  原退出码 23/0 保留且捕获存档；两次前台失败仍自行拼接管道或 echo，未采用参数，
  虽然最终回答正确识别失败，工具收到的仍是包装状态 0。不能宣称模型遵循已获保证。
- 模型数据在 SWE-bench `logs/comparison/20261022-native-output-preview/`，
  有源码补丁和二进制哈希；未覆盖此前 P0/P1 数据，未重跑 SWE-bench 题目或 Windows 实机。

本次仅完成原生可选预览。没有全局 pipefail、命令改写或输出文本推断；
用户/模型显式发出的管道保留其原本 shell 语义。
