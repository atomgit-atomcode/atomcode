# Active sharing detach implementation plan

**Goal:** 修复运行中 logout / stop / 再启动共享返回 ActiveTurn。
**Architecture:** CodingRuntime 仍是执行 owner；live hub 仅管理可撤销的共享投影。显式 detach 清除指定绑定及投影，不取消 runtime，不复用“已关闭 runtime”的业务语义。CLI 与事件转发使用同一绑定锁，hub 解绑成功后才清本地绑定；失败保留以便重试。旧绑定不能清除 successor，旧事件不能污染重绑。
**Tech Stack:** Rust；CLI + daemon live hub。

基线 release/v5.2.4，13686bfb877d97d9a4a1768c09b2c985a0938db0。保留工作区 skill-status 等无关改动。近期 dbfee2ccb 已允许运行中 attach，但 detach 仍走拒绝 active turn 的 unbind。

1. live_hub.rs 增加明确的 detach 语义，复用 scoped 清理；unbind 保留活跃回合保护，unbind_retired 保留原有契约。
2. native_live.rs 让 embedded 注销使用 detach。tui_share.rs 在绑定锁内完成远端撤销后清除本地与 pump，stop 不吞错误。attach 的命令 sink / pump 注册也位于同一绑定锁内，避免 detach 后迟到注册。
3. 测试运行中/等待审批中重复 detach 与重绑、旧事件/旧绑定拒绝、任务未被取消；原 unbind 仍拒绝活跃回合。
4. daemon / CLI 测试、fmt 与 compile gate；不启动真实服务器或退出用户登录。

验证：daemon 357 项、CLI 590 项测试通过，包含运行中/等待审批时多轮解绑与重绑、旧绑定/迟到事件拒绝及不发送取消命令。手工登录退出及真实浏览器链路未执行。审计补充：sink/pump 注册与 detach 串行化，防止解绑后重新注册；私有 crypto overlay 和 Cargo.lock 不纳入提交。
