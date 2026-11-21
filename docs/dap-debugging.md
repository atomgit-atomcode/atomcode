# DAP 调试（最小闭环）

DAP 动态调试默认开启，通过 `debug` 工具按需调用本机 stdio 调试适配器。不需要先填写 DAP 配置，也不会在启动 AtomCode 时自动运行调试器或安装软件。协议实现位于 capabilities，装配位于 coding；kernel 不管理调试业务。

## 直接使用

项目 Python 环境已有 debugpy 时，直接要求 Agent：“调试 target.py，在第 3 行设置断点，检查 fee，单步后继续至结束。”不需要写配置，也不需要知道适配器名。

Python 目标（.py / .pyw）自动选择项目 `.venv` / `venv`，其次选择 VIRTUAL_ENV 指定的环境，再查 PATH 中的 python3/python。缺少 debugpy 时会返回对应解释器的安装命令；安装后直接重试，无需重启或重载配置。不会静默改用其他 Python 环境，也不会自动安装依赖。

Rust/C/C++ 等原生程序先构建带调试信息的可执行文件，然后让 Agent 调试该文件；会在 PATH 中查找 lldb-dap，兼容 lldb-vscode。传源码而非可执行文件时会提示先构建。其他语言目前需要显式配置适配器。

Agent 在排查运行时错误、失败测试，需要断点/单步/变量等动态证据时可自行选择 `debug`；不会每次任务都自动执行，也不会在启动时运行目标程序。实际调用沿用执行审批。`status` 只检查本地文件路径，不启动进程；launch 才检查 Python 是否安装 debugpy。

## 可选配置

可以通过 config.toml 指定适配器，覆盖同名内置发现结果。例如指定另一套 Python：

```toml
[dap]
enabled = true

[dap.adapters.python]
# 使用装有 debugpy 的 Python 完整路径；Windows 可写 C:/venv/Scripts/python.exe。
command = "/path/to/venv/bin/python"
args = ["-m", "debugpy.adapter"]
```

修改配置后重启会话或重载配置。默认提供 `debug`；设置 `[dap] enabled = false` 可关闭，`--no-tools` 仍关闭此工具。适配器命令直接执行，不经过 shell。配置只支持 command/args，不提供保存 API key 或 token 的字段；不要在命令参数中放凭据。调试程序继承宿主环境，具备宿主权限，应遵守与执行项目代码相同的信任要求。

首版实现通用 stdio 协议，并以 debugpy 实测；其他适配器需要自行验证其 launch 参数兼容性。无远程 attach、自动构建、调试器安装、子进程调试、runInTerminal、内存写入或专用调试 UI。LLDB 只提供本地命令发现，尚需各平台及适配器实测。

## 让 Agent 跑一个闭环

例如要求：“在 target.py 第 3 行设置断点，检查 fee 的值，单步后继续运行，结束调试。”对应调用顺序：

1. `debug(action="launch", program="target.py", breakpoints=[{"file":"target.py","line":3}])`。
2. 从返回值 `session.session_id` 取得会话标识，随后调用 `wait`，等到 `session.state="stopped"`。
3. 用当前的 `session.stop_id` 调用 `stack_trace`，取 frame id；接着 `scopes(frame_id=...)`，取 `variablesReference`；再 `variables(variables_reference=...)`。
4. 可用 `evaluate(expression="fee", frame_id=...)` 求值。求值可能执行函数或改变状态，不是只读操作。
5. `step_over` / `step_in` / `step_out` / `continue` 后调用 `wait`。新的暂停使用新的 stop_id，重新获取 frame/variable 引用。
6. `terminate` 断开并清理适配器及所属进程树。

除 launch/status 外，必须提供 session_id。检查暂停状态及执行单步/continue 时还必须提供当前 stop_id。`threads` 可列线程；未返回暂停线程时，显式传 thread_id。`set_breakpoints(file=..., lines=[...])` 替换该文件的所有断点，空数组清除。行号从 1 开始。

## 生命周期与限制

- 每个已装配工具只持有一个调试会话；launch 不会偷偷替换现有会话。
- 所有操作保守标记为 Risky，沿用现有审批和 plan 模式约束。读取变量也可能触发适配器的 getter 或 pretty-printer。
- 每次调用默认超时 15 秒，timeout_secs 可设置 1–30 秒。wait 超时、进行中的调用取消、协议/请求错误会关闭会话；错误参数也可能关闭当前会话。旧 session_id 不会关闭新会话。
- 一次工具调用最多执行一步；没有自动单步循环。未命中断点时可 status、pause 或 terminate。wait 超时不会留下后台调试任务。
- 仅切换 provider 或原地刷新技能时保留现有工具；实际重建能力、配置或会话，以及 runtime 释放时，会释放工具并清理其调试进程。调试状态不落盘，不会随会话恢复。普通回合结束可以保留暂停的调试会话，以便下一回合继续。
- 栈和变量支持 start/count 分页，count 上限 50；响应有大小、深度和字符串长度限制。truncated=true 表示结果被裁剪，缩小查询范围。output_tail 是有限、尽力保留的输出尾部，可能在输出洪峰中丢弃片段，不作为完整日志。
- Windows 使用现有 Job Object 清理，Unix 使用独立进程组；主动脱离进程组的目标不在清理保证内。首版实际验证平台为 macOS，Windows/HarmonyOS 仍需真机验证。

## 验证

普通 fixture 测试不需要安装 debugpy。真实测试使用临时环境，不依赖模型服务：

```sh
python3 -m venv /tmp/dap-venv
/tmp/dap-venv/bin/python -m pip install debugpy
ATOMCODE_TEST_DEBUGPY_PYTHON=/tmp/dap-venv/bin/python \
  cargo nextest run -p atomcode-capabilities --features tools \
  --run-ignored only -E 'test(real_debugpy)'
```

该测试使用默认设置，在项目 `.venv` 中发现 Python/debugpy，不传 adapter，实际启动 Python、设置断点，检查 fee=42、求值、单步、继续至结束并清理。协议与启动顺序参考 [DAP 官方说明](https://microsoft.github.io/debug-adapter-protocol/overview)；实现分层参考本地 oh-my-pi 的 dap/client.ts、dap/session.ts 和 tools/debug.ts，并未复制其完整适配器目录或多会话系统。
