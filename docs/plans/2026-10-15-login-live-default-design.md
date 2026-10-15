# 登录后当前窗口采用默认模型

基线：`release/v5.2.4`，`eac6caa716db1d7c6eef0a173d0a47a4d3647bc8`。工作区已有其他修改，保持原样。

`/login` 已使用 `AdoptServerDefault` 保存 CodingPlan 的服务端默认选择；自配模型仍按现有策略保留。普通 `Reload` 通过文件指纹决定是否重建，并继续应用启动模型参数，无法表达登录后明确采用刚保存的选择。

登录完成改发已有的 `HostCommand::SignIn`。宿主配置通过 `after_sign_in` 读取持久化选择，文件宿主不再套用启动模型参数。SignIn 通过 `CodingRuntimeHandle::reprepare_config` 应用完整配置，保留原会话并让 runtime 重新发布模型描述。普通 reload 的指纹优化不变。宿主事件转发将 `Reconfigured` 同模型切换一样交给 `redescribe`，补上完整重建后底栏仍显示旧模型的共同根因。

唯一生命周期 owner 仍是 CodingRuntime；宿主只在 runtime 返回成功后更新配置和指纹。配置解析或重建失败沿原错误路径报告，不回退到 fresh，也不提前宣布切换成功。登录报告描述服务端配置，当前模型由运行时 Described 事件更新。

验证覆盖启动参数与持久化默认不同、文件指纹未变但运行时模型不同、登录前后的模型请求、会话保留和模型描述。已有普通 reload、取消、审批和登录控制判据继续执行。经典 tuix 已在登录后解除窗口 pin，不修改它；daemon 的后台同步仍保留原选择。
