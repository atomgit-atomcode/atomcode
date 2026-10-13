# One-shot Agent 任务失败感知

基线：release/v5.2.3，3911951f1，tracked 工作区开始时为空。

## 边界

run_to_completion 仍是 kernel AgentHandle 的 one-shot 适配器，Agent 任务与事件由原先的 owner 管理。它记录是否收到 TurnComplete 并检查 JoinHandle 的结果，同时监听任务结束，避免外部保留 RequestCtx 使失败任务永远等待事件通道关闭。已排队事件优先消费，以保留完整输出和终态。panic、异常 abort/cancel、正常任务退出但没有终态，返回 InternalError 与具体原因及 internal_error 错误码；已有正文、工具结果和此前错误保留。

不吞 provider 错误，不把正常协作取消改为内部错误，不新增运行时 owner、hook、压缩状态机或持久化 writer。Release 的 panic=abort 仍结束进程；此修复针对 unwind 时可以取得 JoinError 的情况。

## 受影响入口

生产调用方包括 review_tool 和 parallel_edit，它们已有非 Stopped 或 error 检查。CLI/TUI/daemon 的 coding 主循环不直接调用此方法，但新终态补齐 headless 非零退出、通知错误、TUI 内部错误文案、daemon internal_error、ACP 错误和 StopFailure hook 分类。harness 的 CC hook 桥保留 InternalError，而非映射成 Stopped。

StopReason 新增中立 InternalError 枚举，不改变已有 variant 的序列化名称。one-shot 的 task 失败不伪造已落盘的 TurnComplete 事件；其 Outcome 是失败返回值。

## 判据

实际 hook panic 的 run_to_completion 测试；middleware 保留事件发送器后 panic 仍有界结束且保留正文；异常取消和缺失终态测试；终态后的 panic 仍失败且保留内容/旧错误；已有 kernel 正常完成/provider 错误回归。验证 CLI 退出码和通知、TUI 文案、daemon wire 与 ACP/hook 映射；运行 kernel 全部测试和编译闸门。
