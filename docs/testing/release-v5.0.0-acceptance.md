# atomcode release/v5.0.0 —— 真机验收测试清单

对应改动：v4.26.0 → v5.0.0，共 122 个提交，涵盖 A–I 九个变更区。

本清单分区说明：
- **A–C、E–G** 是默认行为或默认开启的功能，直接启动即可测。
- **B 区（daemon kernel 引擎路径）** 需要 `ATOMCODE_DAEMON_ENGINE=kernel` 环境变量，且必须**新会话**才走 kernel 路径。
- **C 区（memory 工具）** 默认开启；设 `ATOMCODE_MEMORY_TOOL=0` 可关闭。
- **D 区（prompt 行为）** 靠观察弱模型（deepseek-v4-flash）在上下文/回合压力下的反应，无法用断言验收，需真机感性判断。
- **I 区（内部重构）** 不需逐条功能测，只需整体回归 sanity：构建通过、启动正常、基本 chat 流程不炸即可。

---

## 环境准备

- [ ] 使用**已编译的 release/v5.0.0 二进制**，确认 `atomcode --version` 输出 v5.0.0
- [ ] **默认引擎**（v2，无需任何 env）用于 A、C、D、E、F、G、H、I 区测试
- [ ] **B 区**：`export ATOMCODE_DAEMON_ENGINE=kernel`，每次测试前**新建会话**（已存在会话会走旧路径）
- [ ] **D 区行为测试**：配置弱模型 `deepseek-v4-flash`（或类似弱模型），构造上下文高压/回合数接近上限场景
- [ ] webui 测试需要 `atomcode daemon` 在后台运行，打开 `http://localhost:<port>`
- [ ] Windows 专项（F 区 /desktop 不闪 console）：需 Windows 机器

---

## §1 TUI 视觉重构 —— bash 命令渲染 [P0]

bash 命令按 shell 边界智能折行，并在 `● Bash` 头部下方以 `  └ <cmd>` 缩进块渲染（无 `$` 前缀）。

- [ ] 发送一条简单命令（`ls -la`）：确认 TUI 显示 `● Bash` 头部 + `  └ ls -la`（无 `$` 符号）
- [ ] 让模型执行含 `&&` 链接的长命令（如 `cd /tmp && make build && make test`）：
  确认命令按 `&&` 边界折成多行显示，`&&` 出现在下一行开头，每行有 `  └` 缩进
- [ ] 让模型执行含 `\` 续行的命令（如 `sed -i '' \ -e 's/a/b/' \ -e 's/c/d/'`）：
  确认续行折开，每个 `-e` 段各占一行，续行部分有 4 空格缩进
- [ ] 让模型执行一条超宽命令（命令长度超过终端宽度）：确认命令在 token 边界软折行，**不被截断**、不显示省略号，可完整阅读
- [ ] **批次（Batch）子命令渲染**：让模型并行执行多条 bash 工具调用（同一轮多个 bash tool call）：
  每个子命令独立渲染为 `  └ Bash <cmd>` 一行（灰色，和 `  └ Grep(...)` 等其他 child 一致，**无洋红、无 `$`**），
  命令文本可读、按终端宽度干净裁剪（不再硬 `(truncated)` 截断）；工具完成后原地追加 `→ N lines`
- [ ] 在 bash 命令执行期间按 Ctrl+O 触发提示：确认提示行与 `● Bash` 头部一体，命令完成后整体消失，不留孤立 spinner（⠙ 转圈符不残留在 `●` 旁边）

---

## §2 TUI 视觉重构 —— diff 渲染 [P0]

edit_file 产生的 diff 现在是真实 unified diff，带行号 gutter，红绿按深/浅主题柔化。

- [ ] 让模型 `edit_file` 修改一个文件：确认 diff 块以 `<旧行号> <新行号> <+/-> <内容>` 格式渲染，能看到真实的行号
- [ ] diff 中有删除行（以 `-` 开头的内容，如删除一行 `--` 或 `- foo`）：确认删除行**不被丢掉**，正常显示在 diff 块中
- [ ] **浅色主题**下查看 diff（若支持）：红/绿色应使用深红/深绿（DarkRed / DarkGreen）而非亮色，确保可读性
- [ ] **深色主题**下查看 diff：使用鲜艳红/绿，确认颜色鲜明可辨
- [ ] diff 有上下文行（context line）：context 行以 Muted 灰色渲染，行号 gutter 对齐

---

## §3 TUI 视觉重构 —— 持久 todo 面板 [P1]

todo 面板固定在输入框上方，执行中多行显示，全部完成后隐藏，无颜色装饰（colorless）。

- [ ] 启用 `ATOMCODE_TODO` 环境变量，发起一个多步骤任务（让模型在多轮使用 todowrite 工具）：
  确认 todo 面板**固定在输入框正上方**，显示类似 `☑ Todos · N/M` 的标题行
- [ ] 面板中 in-progress 任务**加粗**显示，pending 任务普通显示，completed 任务**无颜色**（不带装饰色）
- [ ] 任务全部完成后（M/M done）：确认 todo 面板**自动消失**，不再显示
- [ ] `/bg` 切到后台再切回来：确认 todo 面板已**清空重置**
- [ ] `/session` 切换到空会话：确认 todo 面板**清空重置**
- [ ] 在含有 todo 列表的会话上执行 `/resume`：确认 todo 面板被**正确 seed**（从历史消息中恢复到最新状态），不是空白

---

## §4 TUI 视觉重构 —— 间距与行内代码 [P1]

- [ ] 让模型输出一段助手文字，紧接着触发一个工具调用：
  确认助手文字末尾与第一个工具块之间有**1 行空行**（视觉呼吸感）
- [ ] 让模型输出含行内代码（ \`code\` ）的文字：
  确认行内代码**仅着色**（亮青色），**不加粗**（de-flare）

---

## §5 TUI 视觉重构 —— 审批弹窗硬化 [P0]

审批提示 (Y/n/A) 确认后，不会把随后的模型正文误消化。

- [ ] 触发一个需要审批的工具（如写文件），按 `y` 批准：
  确认审批提示消失，**之后的助手回复正文**正常显示，未被静默丢弃
- [ ] 触发审批提示，按 `n` 拒绝：确认审批块被移除，后续模型输出显示正常
- [ ] 终端窗口 **resize 期间**恰好有审批提示：确认 resize 后审批提示行数正确重置，再按 y/n 不吃后续正文

---

## §6 TUI 其他改进 [P1]

- [ ] 输入 `/model` 后在过滤框输入关键字（如 `deep`）：
  确认过滤框中**显示已键入的关键字**（过去会隐藏输入）
- [ ] 在非 Unicode 终端（设 `TERM=dumb` 或关闭 Unicode 符号）下运行：
  确认 `│`、`└`、`●` 等特殊字符正常降级为 ASCII，不出现豆腐块

---

## §7 daemon kernel 引擎路径 [P0 opt-in] ⚠️ 需 `ATOMCODE_DAEMON_ENGINE=kernel`

> 每次测试**必须新建会话**，已有会话不受影响（kernel 路径只对新对话生效）。
> 测完后 `unset ATOMCODE_DAEMON_ENGINE` 回到默认路径，跑一遍相同用例做 A/B 对比。

- [ ] `export ATOMCODE_DAEMON_ENGINE=kernel`，启动 daemon，打开 webui **新建会话**，发一条消息：
  确认流式回复正常显示，无报错
- [ ] **工具调用**：让模型 `read_file` 读一个存在的文件：确认工具结果正常返回，webui 显示正常
- [ ] **Usage / Context-stats 显示**：确认 webui 状态面板中有 token 用量统计（非全零）
- [ ] **压缩（compaction）**：在 kernel 路径下触发 `/compact`：确认流程完成，对话可继续
- [ ] **TurnComplete**：一轮对话完成后，webui 输入框恢复可用（不卡在 streaming 状态）
- [ ] **SetConversation resume（resuming 重连）**：在 kernel 路径新建会话后关闭 webui 标签，重新打开该会话：
  确认历史消息正常恢复（SetConversation 重放）
- [ ] **ReloadConfig 切换**：在 kernel 路径下 `/model` 切换模型：确认切换成功，下一轮对话走新模型
- [ ] **ChangeDir**：执行 `/cd /tmp`：确认 working directory 更新，工具调用以新目录为基准
- [ ] **审批 round-trip**：触发需审批工具 → 按 `y` → 工具执行 → 确认正常流转，不卡住
- [ ] **A/B 对比**：`unset ATOMCODE_DAEMON_ENGINE`，重复以上核心步骤（发消息/工具调用/审批）：
  确认行为一致，无明显差异

---

## §8 model-facing memory 工具 [P1]

> 默认开启（`ATOMCODE_MEMORY_TOOL` 未设或设为 `1`）。

- [ ] 打开新对话，让模型调用 `memory` 工具记录一条信息（如"使用 tabs 缩进"），操作：`remember`、scope=`project`：
  确认工具调用成功，模型回复确认已记录
- [ ] 下一轮对话（**同一会话**）让模型 `list` memory：确认刚才记录的内容出现在列表中
- [ ] 再次让模型记录**完全相同的内容**（去重测试）：确认内容只出现一次，未重复追加
- [ ] 让模型 `forget` 刚才的记录（按关键词）：确认返回已删除，再 `list` 时该条消失
- [ ] **全局 memory**：让模型 `remember` 一条内容到 scope=`global`；切换到**另一个项目目录**新建会话，让模型 `list` memory：确认全局条目可见
- [ ] **TUI 直连命令**：输入 `/remember 我喜欢 TypeScript`：确认成功写入，提示已记录
- [ ] 输入 `/forget TypeScript`：确认成功删除
- [ ] 输入 `/memory`：确认列出当前所有记忆条目（或提示为空）
- [ ] **关闭 memory 工具**：设 `ATOMCODE_MEMORY_TOOL=0` 重启，在新会话中让模型调用 memory 工具：
  确认工具**不在工具列表中**，模型无法调用

---

## §9 prompt/persona 行为 [P1] ⚠️ 需观察弱模型行为

> 用 deepseek-v4-flash（或类似弱模型）测试以下场景。

- [ ] **状态提醒不含时钟**：观察每轮注入的 `<system-reminder>`：
  应只含 `Current date: YYYY-MM-DD (Weekday)` + Context window 用量 + `Turn round: N`，**不含 `local time HH:MM` 字样**
- [ ] **回合计数无上限倒计时**：状态提醒中的回合数格式应为 `Turn round: 3`（只有当前轮号），**不含 `of M (max)` 格式**
- [ ] **反假完成护栏**：进行一个上下文用量接近 70–80% 的长任务，让弱模型在多轮完成：
  确认模型**不会草率宣布完成**（"已为你做好了"、"任务完成"）然后实际未执行；若 context 压力大应继续执行，不应闲聊时间
- [ ] **闲聊时间测试**：与弱模型进行编码任务，观察其是否会发出"快 1 点了/要休息了吗"等时间感知闲聊：
  预期：不再出现此类评论（时钟已移除）
- [ ] **todowrite 引导**：用弱模型（如 GLM）执行一个 5+ 步骤任务，观察是否自动开启 todowrite；
  引导行为改善后，GLM 应比之前更倾向主动使用 todo

---

## §10 /init LLM 驱动 [P1]

`/init` 命令现在把分析请求提交给 agent，由模型自己分析仓库、生成 AGENTS.md。

- [ ] 在一个**没有 AGENTS.md** 的项目目录启动 atomcode，执行 `/init`：
  确认 TUI 开始一个 agent 轮次（可见 streaming 输出），模型开始分析仓库
- [ ] 等 agent 完成后：确认项目根目录生成了 `AGENTS.md` 文件，内容是针对该项目的结构描述（而非静态模板）
- [ ] 在**已有 AGENTS.md** 的目录执行 `/init`：确认 agent 同样运行（不是静态拒绝），可以更新或重写该文件

---

## §11 /desktop 命令 [P2]

- [ ] 在**已安装桌面 app** 的机器上执行 `/desktop`：确认桌面 app 被启动（或前台激活）
- [ ] 在**未安装桌面 app** 的机器上执行 `/desktop`：确认输出一条下载 URL，引导用户安装
- [ ] **Windows 专项**：在 Windows 上执行 `/desktop`：确认**不出现控制台窗口闪烁**（黑框一闪而过）

---

## §12 webui 修复 [P1]

- [ ] **远程 HTTP MCP servers 显示**：配置至少一个远程 HTTP MCP server，打开 webui 状态面板（MCP 面板）：
  确认远程 HTTP 类型的 MCP server 出现在列表中（过去只显示本地 server）
- [ ] **围栏代码块渲染为 artifact**：在 webui 中触发一轮让模型输出含三重反引号代码块（如 markdown 或 python 代码）的回复：
  确认 daemon strip 掉的围栏代码块被渲染成 artifact 卡片形式，而非显示为原始 \`\`\` 文本
- [ ] **刷新中断会话保护**：在一轮请求进行中（模型正在 streaming）**刷新浏览器**：
  确认已完成的对话轮次**不丢失**；即使本轮未完成，之前的会话内容在重新加载后仍可见
- [ ] **侧栏登录指示真实有效性**：
  - 在**已登录**状态下打开 webui：侧栏登录状态显示为已登录（绿色/勾选）
  - **token 过期或从未登录**时：侧栏显示未登录或过期状态，与实际认证状态一致
- [ ] **系统临时目录过滤**：在 `/tmp` 下启动一个 atomcode 会话，打开 webui 项目列表：
  确认 `/tmp`、`/private/tmp`（macOS）等系统临时目录**不出现在侧栏项目列表**中
- [ ] **无会话目录过滤**：在侧栏查看项目列表：确认从未有过任何会话的目录不出现

---

## §13 capabilities 稳定性修复 [P1]

- [ ] **开流瞬态失败自动重建**：
  模拟或等待一次网络连接偶发中断（如 VPN 重连、网络切换），导致请求失败：
  确认 atomcode **自动重试**（不需要 `/login` 刷新），下一次请求正常；日志/界面无需用户干预
- [ ] **os error 110 / TimedOut 归类为瞬态**：
  在高延迟网络环境（可用 `tc` 或慢代理模拟）发起请求，若触发 `TimedOut (os error 110)`：
  确认被当作瞬态传输错误处理（自动重试或友好提示），而非报出红色永久错误
- [ ] **读 .env.example 不误弹审批**：让模型读取项目中的 `.env.example`、`.env.template` 或 `.env.sample` 文件：
  确认**不弹出敏感文件审批提示**（这类模板文件不含真实密钥）
- [ ] **读真实 .env 仍弹审批**：让模型读取 `.env` 文件（非 `.example`）：
  确认**仍然弹出**敏感文件审批（安全回归）
- [ ] **edit-diff 超时保护**：对一个极长文件执行 `edit_file`（如 >10000 行文件的大范围修改）：
  确认操作能完成或在合理时间内返回错误，**不卡死**

---

## §14 内部重构 —— 回归 sanity [P0 回归]

> 这些是纯内部重构（v1 引擎退役、config 拆包、crate 合并），行为应无变化。
> 不需逐条功能测，验证整体链路没坏即可。

- [ ] **构建通过**：`cargo build --release` 无错误（0 error, 0 warn 阻塞）
- [ ] **启动正常**：`atomcode` 冷启动，显示欢迎界面和输入框，无 panic
- [ ] **基本 chat 流程**：发一条消息，模型正常回复，工具调用可用，会话可保存
- [ ] **会话持久化**：退出后重新进入，`/resume` 能恢复历史会话
- [ ] **配置加载**：`~/.atomcode/config.toml`（或等效路径）中的设置（如 `ui.theme`）正常生效
- [ ] **daemon 模式**：`atomcode daemon` 启动正常，webui 可访问
- [ ] **v1 引擎已退役**：运行 `atomcode --engine v1 chat`（或类似旧参数）：
  确认命令被拒绝或报告 `--engine v1` 已不支持，**不能正常启动 v1 路径**
- [ ] **/issue 已删除**：输入 `/issue`：确认提示"未知命令"或该命令不存在，**不能正常执行**
- [ ] **fixissue 功能已删除**：确认无 `atomcode fixissue` 子命令（或等效命令）

---

## §15 已知删除项核对 —— 确认彻底消失

以下条目在 v5.0.0 中已被移除。若还能触发，则是回归 bug。

- [ ] `--engine v1` 参数：**应不可用**（v1 AgentLoop 已删）
- [ ] `/issue` 命令：**应不存在**（fixissue 功能和 core::atomgit 已删）
- [ ] bash 命令的 `$` 前缀（如 `└ $ ls`）：**应已去掉**；若 TUI 中仍看到 `  └ $ ls` 形式（`$` 在命令内容之前独立显示），是回归
- [ ] `Turn round: N of M (max)` 格式的回合倒计时：**状态提醒中应不出现**

---

## 最小冒烟（时间紧只跑这些）

P0 核心链路 + 最高价值 P1 场景：

- [ ] §1.1（bash 命令基本渲染，无 `$` 前缀）
- [ ] §2.1（edit_file diff 有行号 gutter）
- [ ] §3.1（todo 面板固定在输入框上方）
- [ ] §5.1（审批 Y 后正文不丢失）
- [ ] §7（ATOMCODE_DAEMON_ENGINE=kernel 新会话能聊天）
- [ ] §8.1–8.3（memory 工具 remember/list/去重）
- [ ] §10.1（/init 触发 agent 生成 AGENTS.md）
- [ ] §12.3（webui 刷新中断不丢之前轮次）
- [ ] §14（v1 退役回归：构建/启动/基本 chat 正常；`/issue` 报未知命令）
- [ ] §15（bash 无 `$` 前缀，无 `of M (max)` 回合倒计时）

---

## 备注

- 相关分支：`release/v5.0.0`，基于 `v4.26.0` tag，共 122 个提交
- kernel 路径默认关闭：`ATOMCODE_DAEMON_ENGINE=kernel` 为 opt-in，v5.0.0 默认仍走旧 v2 bridge 路径
- memory 工具路径：project memory 存储于 `.atomcode/memory.md`（项目根），global memory 存储于 `$ATOMCODE_HOME/memory.md`
- diff 渲染使用 `similar` crate 计算真实 unified diff；行号 gutter 宽度按最大行号自适应
- bash 命令渲染：`format_shell_command` 函数在 `event_loop/mod.rs`；`PAD_COL=2` 对应 `  └` 缩进（2 空格 + 字形 + 空格，共 4 列）
