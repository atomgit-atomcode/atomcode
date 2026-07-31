# Session transitions are committed by Coding Runtime

Fresh、resume 和 change-directory 统一使用 Coding Runtime 的可等待 Session Transition；driver 只能在匹配的成功终态后提交 View Projection，失败时保留旧 Runtime 和旧投影。我们拒绝 fire-and-forget 命令与 driver 侧乐观清屏/改目录，因为它们会让首个 prompt 竞争 `Reconfiguring`，也会在候选构建失败时制造两个不一致的状态所有者。

候选 Runtime 必须在中断当前 agent 前完成所有可失败的构建步骤；fresh session 先留在内存 staging，完整 replacement assemble 成功后才以 catalog 写入作为持久化提交点，失败时旧 generation 继续可用。`Reconfiguring` 对所有 driver 都是不可提交态，旧 generation 的迟到输入仍 fail-closed；View Projection 或 worktree 后置动作失败时保持 pending，不允许入口通过清屏、改目录、删除 worktree 或第二套会话生命周期制造假成功。
