# `/compact` Durable Checkpoint 修复方案

> 状态：最小方案已实现并完成收口验证
>
> 实现起始基线：`release/v5.0.0@6436a0c1431a49b921319d0e13442351935cf4a4`
>
> 收口验证基线：`release/v5.0.0@46d19f219db4ffe92220e169f1f63fe60fc0c4cb`
>
> 关联文档：[compact-native-migration-retrospective.md](compact-native-migration-retrospective.md)

## 1. 结论

Idle `/compact` 不触发 `LifecycleHooks::turn_complete`。旧路径先修改内存 conversation，再发布
`Compacted`，因此用户看到成功后立刻退出或 resume 时，磁盘 snapshot 仍可能是压缩前状态。

本次只补一个局部事务边界：

```text
plan
  -> prepare candidate（不修改 live conversation）
  -> save canonical SessionSnapshot
  -> commit candidate（无 I/O、无 await）
  -> publish success + exact snapshot
```

checkpoint 失败时发布明确失败终态，live messages、`cache_epoch`、turn/request counters 均保持不变。
不恢复 core/bridge compact 协议，不引入 coordinator、CAS、operation log 或全局状态机。

## 2. 成功与失败契约

### 2.1 持久会话的手动 compact

一次 committed manual compact 只有满足下面全部条件才算成功：

1. compaction plan 已通过 kernel 的 sacred-prefix、pairing repair 和净缩减校验；
2. candidate 仍未替换 live conversation；
3. 使用 `RunningAgent::capture_snapshot` 生成完整 snapshot，保留实时 turn/request 高水位；
4. session checkpoint 返回成功，正常 resume 路径已经能读到该 snapshot；
5. candidate 被无失败点地移入 live conversation；
6. 成功事件携带与 checkpoint、live commit 完全相同的 snapshot；
7. 仍维护 core session mirror 的 driver 在展示成功前先写入该 mirror。

### 2.2 失败

checkpoint 返回错误时：

- 发布 `CompactionFailed { trigger, error }`；
- 不发布 `Compacted { committed: true }`；
- 不修改 live conversation 或 `cache_epoch`；
- driver 清理 compact spinner，并显示失败原因；
- 后续可以安全重试 `/compact`。

### 2.3 不写盘的情况

- no-op/refused compact 不写 snapshot，也不递增 epoch；
- auto/overflow compact 保持原有 turn-owned 持久化路径，本修复不改变其语义；
- `SessionMode::Disabled` 是显式 ephemeral，会在内存提交并返回 exact snapshot，但不声称持久化；
- compact 不是新 turn，因此不追加 transcript turn，也不伪造 turn stats。

## 3. 为什么不需要更大的事务系统

当前代码已有两个关键约束：

- 一个 `RunningAgent::session_loop` 串行处理该 conversation 的命令；
- `CodingParts` 明确要求同一 session 最多只有一个 live agent，replace 前必须停止旧 agent。

因此本问题的并发域是单一 agent task。checkpoint 是同步调用，save 返回后到 commit 之间没有
`await` 或可插入的 runtime command；局部 `prepare -> save -> move` 足以形成所需顺序。

本次明确不增加：

- conversation/session revision 或 CAS；
- operation ID、去重日志或 coordinator actor；
- runtime lease/fencing；
- fsync journal、跨文件事务或恢复状态机；
- daemon offline compact 的整体并发重构。

如果未来允许两个进程同时写同一 session，或 checkpoint 改成可取消的异步任务，再单独引入
revision/fencing；当前没有为假设中的并发提前建设。

## 4. 实现边界

### kernel

- `Conversation::prepare_plan`：构造完整 candidate 和 report，不修改 `self`；
- `Conversation::commit_prepared`：只做最终 ownership move；
- `CompactionCheckpoint`：session-bound assembly 注入的单一同步保存接口；
- manual committed 路径先 checkpoint，再 commit；
- `AgentEvent::Compacted` 对 committed manual 携带 exact `SessionSnapshot`；
- `AgentEvent::CompactionFailed` 是 checkpoint 失败的唯一终态。

原有公开 `Conversation::apply_plan` 保留，内部复用 prepare/commit，因此 auto、overflow 和现有调用方
没有行为迁移。

### capabilities / coding assembly

- `SnapshotHook` 同时实现 `LifecycleHooks` 和 `CompactionCheckpoint`；
- 两个 trait object 共享同一个 `Arc<SnapshotHook>` 和 `SessionManager`；
- `SessionMode::Fresh/Resume` 注入 checkpoint，`Disabled` 不注入；
- coding runtime 将失败映射为 `CompactionCompletion::Failed`，并转发 exact committed snapshot。

### drivers

- TUI foreground：先把 exact snapshot 写入 core session mirror，再画 success marker；mirror 保存失败时
  显示保存错误并抑制 marker；
- TUI background：用同一 snapshot 更新并保存对应 background session，checkpoint 或 mirror 保存失败时
  保留现有终态并把仍在运行的 slot 标记为 `Error`；
- daemon live：更新 live conversation，并通过可信 compact 写入绕开通用 stale-write 启发式；
- daemon `/chat`：若收到该事件，先更新 conversation，最终仍由既有 terminal save 落盘；
- CLI/clix：显示 checkpoint failure，不把它伪装成 compact success；
- daemon native translator：保留 snapshot 和 typed failure，不退回 bridge compact handler。

native `.snapshot` 是 v2 resume 的 canonical working set。core `<id>.json` 仍只是 TUI/daemon 的过渡
mirror；本次保证正常路径在成功提示前收敛，但不宣称两个文件族具备崩溃级原子双写。

## 5. 保留与清理

本修复没有新增或恢复以下 legacy surface：

- `atomcode_core::agent::AgentCommand::Compact`；
- compact 专属 core event；
- `atomcode-bridge::runtime::on_command` compact handler；
- v1 compact fallback。

因此 `/compact` 仍处于项目定义的第 4 级：legacy 接口面已退役。本次是退役后的正确性加固，
实际删除 legacy 项为零。

`<id>.meta` 的 message count 属于派生列表信息，仍由下一次 turn completion 刷新；`<id>.jsonl`
是原始 turn transcript，不因 compact 重写。这两项不参与“成功后可立即 resume”的判定。

## 6. 回归测试

必须覆盖：

1. checkpoint 成功时，持久 snapshot、成功事件 snapshot、live snapshot 三者相等；
2. checkpoint 失败时，messages、counters、epoch 完全不变，且没有成功事件；
3. `SnapshotHook` 保存返回后可立即由 `SessionManager::load_snapshot` 读取；
4. coding runtime 不把 native compact 事件泄漏回 legacy adapter；
5. TUI 在 mirror 保存成功前不显示 success marker，失败终态清理 spinner；
6. background TUI 和 daemon translator 保留 exact snapshot；
7. daemon 重复 compact 即使 synthetic summary 数量不增长，也允许可信 snapshot 覆盖；
8. workspace 全目标编译通过，compact legacy variant/handler 搜索仍为空。

实际验证命令与结果记录在本次交付说明中；测试过滤器匹配 0 条时不计为通过。

## 7. 后续边界

本修复完成后没有继续扩展事务架构的必要。只有出现下面任一真实需求时再立项：

- 同一 session 支持多进程或多 live agent 写入：引入 revision/CAS；
- 要承诺断电级 durability：评估 unique temp、file/directory fsync 和恢复测试；
- 要删除 core session mirror：让 TUI/daemon 的 resume/list 全部读取 native session store；
- 要让 meta 在 idle compact 后立即反映 message count：增加独立的派生 metadata 更新，但不得阻塞
  canonical snapshot commit。

本修复已完成实现、最新基线合入与验证；后续按项目流程进行同行 Review 和推送/合入。
