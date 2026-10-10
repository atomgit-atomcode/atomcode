# AtomCode 更新日志

<!--
给编辑这份文件的人看的，这一段不会展示给用户。

- 每个版本一节，以 `## vX.Y.Z` 开头，后面可以跟同源版本和日期：
  `## v5.2.2（v5.2.0） (2026-10-10)`。
- 新版本加在最上面（顺序写错也没关系，展示时按版本号排）。
- 每节分三段，用这三个小标题（`/changelog` 里按页签展示）：
  - `### 概览`：一两段话，说这一版的核心是什么。显示在「概览」页最上面。
  - `### 更新内容`：功能与修复的列表，接在概览下面显示。里面还可以再用别的小标题分组
    （如 `### 修复`），都算更新内容。
  - `### Issues`：这一版关联的 issue，一行一个：`- [#1182 Windows 终端二维码显示异常](链接)`。
    界面上只显示「#编号 标题」，点击打开链接。有这一段才会出现「Issues」页签。
  - 三个小标题都不写也可以，整节当作更新内容。
- 升级后首次启动的一行提示，取「更新内容」前 3 条**顶层**列表项的加粗标题，
  所以把最想让人知道的写在前面，并给它一个加粗的短标题：
  `- **网页端按服务商管理模型**：……`
- 这份文件编译进二进制；改完要重新构建才看得到。
- Issues 段可以直接从 AtomGit 里程碑整理：只列已关闭的 issue，按编号排序，标题照原文。
- 英文版在 `CHANGELOG.en.md`，按同样的版本号逐节对应（小标题用 Overview / Changes）；
  英文界面读它，某个版本没有英文节时显示这里的中文节。Issues 只写在这份里，英文界面也显示这里的。
-->

## v5.2.2（v5.2.0） (2026-10-10)

### 概览

这一版的核心是**架构与终端界面的重构**：运行时重写为分层结构，所有前端共用一个运行时，会话改为可完整恢复的事件记录；全新的全屏终端界面成为默认，设置、模型、MCP、插件、后台会话都收进了面板。

### 更新内容

- **全新架构**：运行时按「智能体循环 / 能力装配 / 编码运行时」分层重写，旧引擎整体退役；终端、网页、VS Code、ACP 共用同一个运行时，各端行为一致。会话改为逐条事件记录，撤销、回退、压缩都作为新记录追加，不改写历史，中断或崩溃后可以完整恢复。
- **全新终端界面成为默认**：直接运行 atomcode 即打开新版全屏界面，旧界面可用 --classic 或配置 [ui] screen 切回；支持鼠标点击、双击选词、三击选行、右键菜单，跨屏拖拽选中并自动滚动，链接和文件路径点击即可打开；终端标题和标签页显示会话名，并标出哪个会话在等你。
- **设置与管理面板化**：/config 打开设置面板（设置 / Config / Status / Usage / Stats），可查看额度、每日用量和各模型用量；/provider 管理账号与模型，保存后自动检测连通性；/mcp 面板逐个启用、停用 MCP 服务器；/toolbox 在会话中开关单个工具；/plugin 管理插件；/status 显示登录账号与订阅到期时间。
- **后台会话与团队协作**：/background（/bg）把当前会话放到后台继续跑，可分组查看、切换、取回；/review 默认在后台评审，不占用当前对话；后台会话的提问会出现在前台，可就地作答；团队面板列出主会话、成员和子代理的状态、耗时与上下文用量，成员会话会保存，重启后随主会话一起恢复。
- **会话更可靠**：/resume 可预览会话最后聊了什么，按两次 Ctrl+D 删除会话；项目文件夹改名后会话不再丢失；双击 Esc 打开回退面板；在 Git 仓库里，/rewind 默认就能连代码一起回退（不需要再手动开启，设 ATOMCODE_CODE_REWIND=0 可关闭）。
- **新增与增强命令**：/changelog 查看各版本更新内容；/worktree 新建独立工作副本并切换过去；/diff 与 /diff git 查看本会话和工作区的改动；/loop 5m 按固定间隔循环执行；/todo add、/todo clear 直接修改计划；/cost 按模型分列 token 账单；/copy 默认复制最后一条回复，/copy code 复制代码块；/raw 把整段对话输出到终端，方便用终端自己的选择；!cmd 在本机直接执行命令；/cd 支持书签和最近目录；新界面也支持 /proxy、/schedule、/openrouter、/app、/webui、/sync，/upgrade 可就地升级并重启；斜杠菜单按使用频率排序。
- **网页端模型配置重做**：按服务商分组，一张卡片填完服务商和模型，保存后自动检测连通性；可为模型设置可选的推理强度档位与默认档位。
- **模型与思考强度**：F2 / Shift+F2 快速切换模型，切换后可直接选择思考强度，/effort 只列出当前模型支持的档位；保留 GLM、Qwen 的思维链；两个账号用同名模型时，状态行标出是哪个服务商；OpenRouter 授权过就不再每次跳浏览器，已下架的模型不再出现在列表里。
- **交互体验优化**：输入框支持粘贴截图和图片路径，大段粘贴自动折叠；写入和编辑文件的工具调用直接展开显示 diff；Ctrl+R 搜索本项目的输入历史；Ctrl+X 中断当前回合并立即发出排队的消息；工具里的 sudo、ssh 会在输入框里请你输入密码，不再卡住。
- **安全、稳定性与 Windows 兼容，及若干修复**：读取凭据、密钥、.env 等敏感文件前先请求审批；委派出去的子代理有明确边界；重复调用熔断更温和，可用 [coding] repeat_stop_rounds 调整；一次请求的图片数量有上限并自动压缩，不再因请求过大失败；Windows 传统控制台、颜色、按键与框线兼容，控制台画不了新界面时自动退回经典界面并说明原因；Windows 上 /upgrade 不再因旧版本占用而「拒绝访问」；bash 后台进程的输出与结束更可靠；切到严格的服务商后不再每次请求都报 400。

### Issues

- [#1564 Windows Terminal 下 Ctrl+V 无法粘贴剪贴板图片（Cline CLI 同环境正常）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1564)
- [#1565 刚上的deepseek-flash模型报错了](https://atomgit.com/atomgit_atomcode/atomcode/issues/1565)
- [#1570 bug：读取图片后，服务连接失败且当前会话中无法恢复](https://atomgit.com/atomgit_atomcode/atomcode/issues/1570)
- [#1573 [共创大赛][Bug] V5.1.0更新说明中修复ESC逻辑，实测未修复，反倒更糟糕](https://atomgit.com/atomgit_atomcode/atomcode/issues/1573)
- [#1574 [共创大赛][Bug] 表格渲染不完整](https://atomgit.com/atomgit_atomcode/atomcode/issues/1574)
- [#1576 [共创大赛][Bug] 系统提示末尾注入 Today's date，跨零点后约 91.7% 的 prompt 前缀缓存失效](https://atomgit.com/atomgit_atomcode/atomcode/issues/1576)
- [#1577 [memory] 记忆注入无预算上限,长期使用 token 成本线性上涨并稀释系统提示](https://atomgit.com/atomgit_atomcode/atomcode/issues/1577)
- [#1578 [mcp] 外部输入边界存在大量 unwrap/panic,单个 MCP server 故障可致整个会话崩溃](https://atomgit.com/atomgit_atomcode/atomcode/issues/1578)
- [#1579 一直提示 工具输出一直被截断 之类的，要怎么解决的？](https://atomgit.com/atomgit_atomcode/atomcode/issues/1579)
- [#1581 [共创大赛]-[Feature] 提供 AtomGit 工具的运行时开关](https://atomgit.com/atomgit_atomcode/atomcode/issues/1581)
- [#1583 [v5.1.0] POST /chat 不挂载 MCP 工具，GET /live 挂载：是有意取舍还是缺口？](https://atomgit.com/atomgit_atomcode/atomcode/issues/1583)
- [#1585 [共创大赛][Bug] 长任务偶发"突然结束"，UI 却显示为成功完成](https://atomgit.com/atomgit_atomcode/atomcode/issues/1585)
- [#1586 基于 daemon HTTP API 做垂直发行版的一批实测问题汇总（v5.1.0，33 条，附最小复现）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1586)
- [#1588 [v5.1.0] 工具返回 16KB 截断阈值写死在源码里，没有配置入口（截断后模型会用记忆补数）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1588)
- [#1589 [v5.1.0] GET /live?session_id= 被「孤儿 runtime」占住后无法恢复，只能重启 daemon](https://atomgit.com/atomgit_atomcode/atomcode/issues/1589)
- [#1590 [v5.1.0] switch_session 之后 MCP 工具全部消失，而 /mcp/status 仍显示 connected](https://atomgit.com/atomgit_atomcode/atomcode/issues/1590)
- [#1591 [v5.1.0] .hooks.json 解析失败是静默的，hooks list 仍显示文件 ✓ 存在](https://atomgit.com/atomgit_atomcode/atomcode/issues/1591)
- [#1592 [v5.1.0] hook 的 command 不做环境变量展开，而 .mcp.json 会做](https://atomgit.com/atomgit_atomcode/atomcode/issues/1592)
- [#1593 [v5.1.0] docs/hooks.md 与 docs/webhook-guide.md 与实现不符（13 个扩展点 / webhook 均已失效）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1593)
- [#1594 [v5.1.0] 被 PreToolUse 拒绝的调用，PostToolUseFailure 不带 tool_name，审计里记不下是哪个工具](https://atomgit.com/atomgit_atomcode/atomcode/issues/1594)
- [#1595 [v5.1.0] latest.json 在 release tag 上写的是上一版的校验值，按 tag 锁版本会拿错 sha256](https://atomgit.com/atomgit_atomcode/atomcode/issues/1595)
- [#1596 [共创大赛][Bug] todowrite 单条 action:update 报成功但状态不落盘，叠加 tool-loop 守卫导致会话死锁](https://atomgit.com/atomgit_atomcode/atomcode/issues/1596)
- [#1597 [共创大赛][Bug] Linux版的 AtomCode 在询问是否执行时，选择 全部允许 依然重复询问](https://atomgit.com/atomgit_atomcode/atomcode/issues/1597)
- [#1598 tool-loop guard 误报频繁：todowrite 状态对账 / 长任务轮询 / 任务列表重置 三类合法重复被拦截](https://atomgit.com/atomgit_atomcode/atomcode/issues/1598)
- [#1601 [bug] Ctrl+O 无法切换查看详细思考过程：仅 Streaming 阶段生效，Idle 阶段被 Buffer::apply 静默吞为 NoOp](https://atomgit.com/atomgit_atomcode/atomcode/issues/1601)
- [#1602 [Bug][新TUI] 输出区滚动鼠标切换的是输入框 prompt 历史——鼠标交还态下终端把滚轮转成 ↑/↓](https://atomgit.com/atomgit_atomcode/atomcode/issues/1602)
- [#1603 [Bug] /resume 会话列表 Ctrl+D×2 无法删除历史会话（删除确认提示未出现）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1603)
- [#1604 [Bug][TUI] 推荐输入（下一句猜测）无消失手势且与真输入同形——左移光标/退格后读起来像「已被确认进输入框」](https://atomgit.com/atomgit_atomcode/atomcode/issues/1604)
- [#1605 [Bug] 已命名的历史会话（AI/自动命名）切换后输入框上方名字徽章不显示，与 picker 列表可见性不一致](https://atomgit.com/atomgit_atomcode/atomcode/issues/1605)
- [#1606 [Bug] /mcp reload 在 Agent 回合中被当作普通消息转发给模型，模型编造"不认词/+mcp withdraw"假报错；reload 异步结果反馈弱](https://atomgit.com/atomgit_atomcode/atomcode/issues/1606)
- [#1607 [Bug] TUI 输出区 URL 链接边界越界：吞入尾随 `)` 与后续中文内容（markdown 链接未渲染且无 OSC 8 超链接）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1607)
- [#1608 [Bug] VL 折叠行的“点击展开”提示用 muted 样式与正文同色，用户无法辨识可点击（应用 Accent 交互色）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1608)
- [#1609 [Bug] Ctrl+R 历史搜索：打开即带入上一条 prompt（Enter×2 可误重发），且过滤只显示单条命中、计数为全历史条数而非匹配数](https://atomgit.com/atomgit_atomcode/atomcode/issues/1609)
- [#1610 harness 会话日志重复追加：同一会话每条事实落盘约 6 份，顺序哨兵判据确定性失败（release/v5.2.0）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1610)
- [#1611 [Bug] fresh 会话启动竞态：首轮事实提交先于 publish_staged_session，撞 NotFound(index) 后 session-store 判死并 cancel，日志永久丢写](https://atomgit.com/atomgit_atomcode/atomcode/issues/1611)
- [#1612 [Docs][TUI] 中断并发快捷键已改绑 Ctrl+X：物料按 e1f32e135 标题引用会带上旧键名 Ctrl+B](https://atomgit.com/atomgit_atomcode/atomcode/issues/1612)
- [#1614 [Bug] 新 TUI：Ctrl+O 思考过程文本颜色由终端实测调色板计算而非固定灰，macOS iTerm2 下不是灰色](https://atomgit.com/atomgit_atomcode/atomcode/issues/1614)

## v5.1.0（v5.2.1） (2026-09-18)

### 概览

这一版扩展了接入面：支持 OpenAI Responses API 和 OpenRouter 免费模型，AtomGit 工具与代码图谱能力增强，并加入可选的代码回溯与每日工作复盘。

### 更新内容

- **Provider 能力扩展**：新增 OpenAI Responses API 支持。
- **AtomGit 工具与代码图谱增强**：atomgit_issue 支持更新与关闭，atomgit_pr 支持列出本人的 PR（list_mine）与修改 PR（update）；代码图谱工具（list_symbols / read_symbol / trace_callers）新增 Kotlin 支持。
- **会话与记忆增强**：新增 list_sessions 工具，可列出本项目的历史会话；新增机器本地记忆层（global > project > local）。
- **Code Rewind 代码回溯（需手动开启）**：设置环境变量 ATOMCODE_CODE_REWIND 后，可回退会话中的代码改动。
- **审批体验与安全加固**：新增「允许所有 Bash」选项，再次询问时会说明原因；审批面板可按 Tab 展开完整的 bash 命令。
- **OpenRouter 一键接入（含免费模型）**：用 /openrouter [key] 或 OAuth（PKCE）登录即可接入 OpenRouter，自动发现并配置免费模型。
- **/worklog 每日工作复盘**：跨项目汇总当天的工作，生成今日小结。
- **性能与 TUI 体验优化，及若干修复**：/resume 列表改为并行扫描，速度提升约 4 倍；-c / resume 启动时只扫描当前会话目录。

### Issues

- [#756 [共创大赛] telemetry write_flag覆盖损坏的配置文件](https://atomgit.com/atomgit_atomcode/atomcode/issues/756)
- [#762 [共创大赛] OpenAI流式解析tool_call索引无上限导致OOM](https://atomgit.com/atomgit_atomcode/atomcode/issues/762)
- [#765 [共创大赛] skill模板shell注入执行顺序导致命令注入](https://atomgit.com/atomgit_atomcode/atomcode/issues/765)
- [#1368 [bug] marketplace 同步失败警告前缀异常（⚠g/⚠d）+ 失败提示冗长刷屏](https://atomgit.com/atomgit_atomcode/atomcode/issues/1368)
- [#1472 [Rust Review] workspace 代码检视：P0/P1 命中点跟踪（2026-08-20）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1472)
- [#1500 [Bug] /mcp tools <server> 传入不存在的 server 键时提示 "(none — server not configured)"，误导用户以为未配置](https://atomgit.com/atomgit_atomcode/atomcode/issues/1500)
- [#1504 [共创大赛][Bug]Team explorer 无法关闭](https://atomgit.com/atomgit_atomcode/atomcode/issues/1504)
- [#1506 [共创大赛][Bug] 模型响应显示不完整，经常说半句就断了，再继续追问立马再回一段](https://atomgit.com/atomgit_atomcode/atomcode/issues/1506)
- [#1511 [共创大赛][Bug]在鸿蒙电脑中shift+tab无法切换模式](https://atomgit.com/atomgit_atomcode/atomcode/issues/1511)
- [#1513 v509接入mmm3出现3次工具调用失败, 依旧念出来系统提示词, 我觉得可能是这个模型就是这么拉胯](https://atomgit.com/atomgit_atomcode/atomcode/issues/1513)
- [#1514 [共创大赛]-[Feature] 希望添加response协议](https://atomgit.com/atomgit_atomcode/atomcode/issues/1514)
- [#1532 [bug] /loop 模式下 goal 为空时陷入死循环，模型重复输出模板回复 80 次](https://atomgit.com/atomgit_atomcode/atomcode/issues/1532)
- [#1535 [共创大赛][Feature] 状态栏常驻显示缓存命中率（cache hit rate）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1535)
- [#1538 codeintel 图谱工具(file_dependencies/blast_radius等)在大工作目录下挂死:CodeIndex 全量建索引无上限无超时](https://atomgit.com/atomgit_atomcode/atomcode/issues/1538)
- [#1539 [共创大赛]-[Feature] openrouter](https://atomgit.com/atomgit_atomcode/atomcode/issues/1539)
- [#1546 [共创大赛]-[Feature] 给底部展示正在使用的模型增加渠道信息展示](https://atomgit.com/atomgit_atomcode/atomcode/issues/1546)
- [#1549 [共创大赛][Bug] atomcode连接https://mcp.espressif.com/docs这个MCP时提示出错。](https://atomgit.com/atomgit_atomcode/atomcode/issues/1549)
- [#1550 [共创大赛][Bug] AtomCode 代码图谱工具对 Kotlin 语言超级不友好，任务大的时候非常容易遇到折叠墙](https://atomgit.com/atomgit_atomcode/atomcode/issues/1550)
- [#1551 [Bug][Windows] datalog 会话日志 O(n²) 膨胀：每轮全量快照写入 jsonl 且无轮转/保留策略，29 天写满 36GB 磁盘](https://atomgit.com/atomgit_atomcode/atomcode/issues/1551)
- [#1553 [bug] 网络短暂中断直接掐断响应（os error 10054），无网络探测等待与断点续传](https://atomgit.com/atomgit_atomcode/atomcode/issues/1553)
- [#1558 [共创大赛][Bug] 会话日志只记录轮级汇总，缺少每次 LLM 请求的耗时/token 明细，建议增加请求级粒度](https://atomgit.com/atomgit_atomcode/atomcode/issues/1558)
- [#1561 [Bug][VSCode 扩展] 模型限流（rate_limited）时本轮直接结束 + 排队消息被清空：应保留排队并在限流恢复后自动继续](https://atomgit.com/atomgit_atomcode/atomcode/issues/1561)
- [#1562 [共创大赛]-[Feature]linux/鸿蒙终端环境中，完善允许 Bash执行命令。](https://atomgit.com/atomgit_atomcode/atomcode/issues/1562)
- [#1563 [共创大赛]-[Feature] TUI 审批弹窗信息不足：bash 命令被截断，无法判断执行后果](https://atomgit.com/atomgit_atomcode/atomcode/issues/1563)
- [#1566 模型报错［invalid_request_error/invalid_request_error］ Invalid schema for function 'code_review': null is not of type "array"］](https://atomgit.com/atomgit_atomcode/atomcode/issues/1566)
- [#1567 [共创大赛][Bug]建议提高这个超时时间 stream idle timeout — reconnecting (1/5)](https://atomgit.com/atomgit_atomcode/atomcode/issues/1567)

## v5.0.9 (2026-08-27)

### 概览

这一版让 AtomCode 能调度更多力量：Codex、Claude Code 可作为外部子代理，代码审查新增深度模式，推理强度全面配置化，并实现了 ACP v2 协议。

### 更新内容

- **外部子代理接入（Codex / Claude Code）**：可将 Codex、Claude Code 作为外部子代理的后端调用。
- **代码审查深度模式**：新增 /review deep 与 verify，按维度并行做深度审查，每条发现单独验证过滤后再去重合并。
- **推理强度全面配置化，新增 xhigh 档位**：推理强度改由配置决定，支持服务端下发的 reasoning_effort_levels。
- **ACP v2 协议**：实现 ACP v2，支持会话与恢复、elicitation 交互表单、MCP 以及 v2 HTTP MCP 连接。
- **命令行恢复指定会话**：-p --resume 与 resume 子命令可恢复指定会话，退出时显示会话 id。
- **TUI 交互与上下文压缩优化**：工具调用成功时 ● 圆点转绿（串行、并行、resume 一致）；单个超大回合也能被压缩；压缩时至少保留最近一次真实的问答。
- **若干修复**：Provider / 模型管理与弱模型健壮性增强，以及登录、安全等方面的修复。

### Issues

- [#1456 [共创大赛][Bug] Todowrite工具调用更新状态出错](https://atomgit.com/atomgit_atomcode/atomcode/issues/1456)
- [#1473 5.0.8更新后TUI渲染表格好难看](https://atomgit.com/atomgit_atomcode/atomcode/issues/1473)
- [#1482 [共创大赛][Bug] 使用/login登录时，若用户点击“取消”按钮，则会导致Windows控制台会卡住](https://atomgit.com/atomgit_atomcode/atomcode/issues/1482)
- [#1487 动不动就截断......一点token全浪费在处理截断上了](https://atomgit.com/atomgit_atomcode/atomcode/issues/1487)
- [#1488 [共创大赛][Bug] 输出折叠频繁，消耗很多无意义的 token](https://atomgit.com/atomgit_atomcode/atomcode/issues/1488)
- [#1490 希望添加会话恢复功能](https://atomgit.com/atomgit_atomcode/atomcode/issues/1490)
- [#1492 gmi的mmm3接入atomcode跟个**一样, 但是接到cc里就开了智了](https://atomgit.com/atomgit_atomcode/atomcode/issues/1492)
- [#1494 [共创大赛]-[Feature] 打开的web页面聊天框无法@模糊匹配文件名,希望增加一个](https://atomgit.com/atomgit_atomcode/atomcode/issues/1494)
- [#1495 resume 指令处理超时](https://atomgit.com/atomgit_atomcode/atomcode/issues/1495)

## v5.0.8 (2026-08-20)

### 概览

这一版集中改进终端里的选择与复制，新增智谱 Coding Plan 预设和更多 Hooks 事件，并减少弱模型读大文件时的来回。

### 更新内容

- **鼠标选择与复制**：双击选词、三击选行，选中即复制；bash 命令和输出等工具块也能拖拽选中复制；鼠标捕获改为默认关闭，默认使用终端自带的选择。
- **智谱 Coding Plan 预设**：/provider 可直接添加智谱 Coding Plan；协议切换新增 Ollama，地址留空时自动填入本机地址。
- **Hooks 新增事件**：新增 Stop、StopFailure、PostToolUseFailure，插件可以感知回合结束、API 出错和工具调用失败。
- **/code-review 显示进度**：显示当前轮次（round X/N）和累计的发现数。
- **读大文件更省往返**：read_file 支持按大纲精准定位和一次读多段，减少弱模型反复翻页。
- **若干修复**：交互式 /resume 遇到正被占用的会话时自动另开一个分支；缓解回复被长度截断的问题；WebUI 在局域网 HTTP 下无法发送消息、命令通知重复堆叠；edit_file 未匹配改为黄色提示等。

### Issues

- [#1329 [hook] 补齐 PostToolUseFailure 事件，让插件能区分"工具成功"与"工具失败"](https://atomgit.com/atomgit_atomcode/atomcode/issues/1329)
- [#1468 没有ollama版？](https://atomgit.com/atomgit_atomcode/atomcode/issues/1468)
- [#1470 TUI: 带圈数字序号（①②③等）与后续字符重叠/错位](https://atomgit.com/atomgit_atomcode/atomcode/issues/1470)
- [#1471 [共创大赛][Bug] ghostty 新版本复制回答内容文本有问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/1471)

## v5.0.7 (2026-08-19)

### 概览

这一版重点在网页端：可直接管理 Provider、发现模型，并能安装为桌面应用；同时支持按模型配置推理强度，终端交互也大幅增强。

### 更新内容

- **WebUI 管理 Provider 与发现模型**：可直接在网页中管理 Provider、发现并添加模型；新增模型时复用已有账号，不必重复录入。
- **WebUI 可安装为应用并发送通知**：支持安装为桌面应用（PWA），任务完成时发送浏览器通知；新增常驻的 Todo 进度面板，侧边栏重构，可调字号，并展示轮次统计。
- **按模型配置推理强度**：可按模型配置推理强度档位；TUI /provider 支持多选，WebUI 只列出端点实际支持的档位，并在多个客户端之间同步。
- **TUI 交互大幅增强**：支持按语义选择会话内容，鼠标可在输入框和历史中选择文本，会话选择器内可直接预览会话，上下键按折行后的显示行移动；重复粘贴可展开长文本，长任务折行更稳定。
- **安全能力升级**：凭据类 shell 命令的防护策略可配置，审批时给出的授权条件会持久保存。
- **若干修复**：WebUI 输入法组合输入被覆盖、侧边栏闪烁、等待指示、历史轮次时间戳丢失、通知权限弹窗、消息排队与同步；Orca 终端兼容、Windows 表格渲染、代理回环泄漏；VSCode 会话标签标题截断、JetBrains 模型切换下拉高亮；插件 home 目录重复枚举、输入历史按项目隔离、Team 工具输出折叠等。

### Issues

- [#1312 [共创大赛][Bug] vscode的扩展一旦出现会话名字过长，在窗口过小的状态下会出现不显示关闭按钮](https://atomgit.com/atomgit_atomcode/atomcode/issues/1312)
- [#1348 5.0.5 历史会话webui里的时间戳 显示的时间是恢复时的时间吗, 能改成显示这条消息发送时的时间吗](https://atomgit.com/atomgit_atomcode/atomcode/issues/1348)
- [#1349 [bug] 对话界面显示内部系统文本（"我就在任务1上！继续任务2！"、<system-reminder> 块泄漏到会话显示）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1349)
- [#1376 [Feature] 文本输入类似编缉一样删除/修改](https://atomgit.com/atomgit_atomcode/atomcode/issues/1376)
- [#1381 [共创大赛]-[Feature] /init指令需要自定义](https://atomgit.com/atomgit_atomcode/atomcode/issues/1381)
- [#1390 [Enhancement] TUIX 输入历史应按工作目录/会话隔离，避免跨项目上下文污染](https://atomgit.com/atomgit_atomcode/atomcode/issues/1390)
- [#1395 在回复skill提问的时候应该允许多行输入](https://atomgit.com/atomgit_atomcode/atomcode/issues/1395)
- [#1396 [共创大赛][Bug] Tab键误触发切换模式](https://atomgit.com/atomgit_atomcode/atomcode/issues/1396)
- [#1400 fix(plugin): 修复 home 目录下插件 scope 重复枚举（对应 PR #992）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1400)
- [#1409 /paste在回答skill提问时无效](https://atomgit.com/atomgit_atomcode/atomcode/issues/1409)
- [#1410 终端输入 atomcode 后自动填充了 OSC 转义序列残留 ］11;rgb:ffff/ffff/ffff\](https://atomgit.com/atomgit_atomcode/atomcode/issues/1410)
- [#1414 [bug] Windows 窗口缩放到 1/4 大小（Win+←+↑）后对话输入框无法输入](https://atomgit.com/atomgit_atomcode/atomcode/issues/1414)
- [#1415 [bug] load session failed：.lease 文件被锁定 os error 33（Windows ERROR_LOCK_VIOLATION）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1415)
- [#1416 [bug] /resume 恢复会话出现 "loading session" 卡死（加载无超时/锁死锁/无法中断）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1416)
- [#1418 [共创大赛][Bug] 作答输入无法用左右键控制光标](https://atomgit.com/atomgit_atomcode/atomcode/issues/1418)
- [#1419 [共创大赛][Bug] windows上powershell中使用时模型回答部分内容未正常显示](https://atomgit.com/atomgit_atomcode/atomcode/issues/1419)
- [#1422 [共创大赛]-[Feature] 长文本ctrl + v两次，把全文贴到终端里](https://atomgit.com/atomgit_atomcode/atomcode/issues/1422)
- [#1425 [共创大赛]-[Feature] 视觉化移动光标](https://atomgit.com/atomgit_atomcode/atomcode/issues/1425)
- [#1426 [Bug] TUI markdown 列表编号低对比度不可见（选中才可见）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1426)
- [#1427 [共创大赛]-[Feature] 希望webui增加任务完成时的浏览器提醒](https://atomgit.com/atomgit_atomcode/atomcode/issues/1427)
- [#1429 [共创大赛][Bug] 我只开了auto，不知道什么时候改成build了？](https://atomgit.com/atomgit_atomcode/atomcode/issues/1429)
- [#1432 [共创大赛]-[Feature] /provider 添加模型增加默认的思考强度字段](https://atomgit.com/atomgit_atomcode/atomcode/issues/1432)
- [#1433 [bug] request_user_input 自由文本输入长内容无换行，被截断隐藏（truncate 而非 wrap）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1433)
- [#1434 [bug] 切换窗口后输入内容不实时回显，需按删除/空格才触发显示](https://atomgit.com/atomgit_atomcode/atomcode/issues/1434)
- [#1435 [共创大赛][Bug] 23%tok窗口就过早自动压缩](https://atomgit.com/atomgit_atomcode/atomcode/issues/1435)
- [#1437 task 子代理(hard 档)只路由到 capable_model 模型(GL M-5.2)且易卡住](https://atomgit.com/atomgit_atomcode/atomcode/issues/1437)
- [#1438 PR 说明中拦截 token/key 关键词：无文档无开关，建议改为密钥格式识别+分层策略](https://atomgit.com/atomgit_atomcode/atomcode/issues/1438)
- [#1444 [共创大赛][Bug] agent 执行多任务过程中，手动取消，输入新提示词，还是会继续上一次的任务](https://atomgit.com/atomgit_atomcode/atomcode/issues/1444)
- [#1447 申请给auto模式解除限制 - 安全策略已阻止工具调用：凭据不能通过通用 shell 参数、临时文件或环境变量传递](https://atomgit.com/atomgit_atomcode/atomcode/issues/1447)
- [#1450 web UI 历史对话的时间显示不对,时间总是显示的当前系统时间（atomocode v5.0.6）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1450)
- [#1454 [Bug] webui 切换模型推理强度未广播同步到 TUI](https://atomgit.com/atomgit_atomcode/atomcode/issues/1454)
- [#1455 [优化] 模型思考强度配置展示优化：档位符号、官方模型锁定、默认值合并](https://atomgit.com/atomgit_atomcode/atomcode/issues/1455)
- [#1457 [优化] 会话历史：事件时间显示在文字前且带年月日、prompt/回答左右对齐+背景色区分](https://atomgit.com/atomgit_atomcode/atomcode/issues/1457)

## v5.0.6 (2026-08-12)

### 概览

这一版带来多 Agent 并行协作（Agent Team）和按需启用的语义代码智能（LSP），长任务的中断恢复更可靠，并新增交互式配置编辑器。

### 更新内容

- **Agent Team 异步协作**：多个 Agent 并行执行任务，实时展示成员状态、任务详情与 Token 消耗。
- **语义代码智能**：可按需启用 LSP，提供符号读取、引用查找、调用链分析等代码理解工具，分析大型项目更高效。
- **Goal 长任务恢复**：暂停、压缩、切换模型或异常中断后仍保留任务进度与上下文，减少重复执行和任务丢失。
- **交互式 /config 配置编辑器**：直接在 TUI 中管理配置；新增 Xiaomi MiMo、OpenCode Zen 等 Provider 预设，支持按模型配置视觉能力。
- **MCP 优化**：读取并遵循 MCP 服务器提供的使用说明（instructions）；加强 HTTP 响应大小限制、stdio 超时恢复与并发处理，更稳定也更安全。
- **会话与 WebUI 体验**：可配置 /resume 的历史截断、跨工作区继续会话、执行中追加消息；TodoWrite 工具行显示具体任务名；移动端输入与模型切换体验改进。
- **视觉与大文件处理**：模型可通过 supports_vision 声明原生图片能力；read_file 增加分页提示，避免超大文件一次性进入上下文。
- **若干修复**：DeepSeek V4 工具调用兼容、F2 跳过不可用模型、Goal / Todo 面板状态不同步、长任务折行、输入前缀丢失、WebUI 消息排队与同步、会话内部提醒外泄等。

### Issues

- [#737 [共创大赛] bash工具dd命令绕过危险操作检测](https://atomgit.com/atomgit_atomcode/atomcode/issues/737)
- [#738 [共创大赛] bash工具无空格重定向绕过关键文件保护](https://atomgit.com/atomgit_atomcode/atomcode/issues/738)
- [#752 [共创大赛] MCP HTTP响应体无大小限制导致OOM](https://atomgit.com/atomgit_atomcode/atomcode/issues/752)
- [#787 [共创大赛] MCP registry call_tool跨await持锁导致阻塞](https://atomgit.com/atomgit_atomcode/atomcode/issues/787)
- [#880 WeCom WebSocket errcode 846609: aibot websocket not subscribed - response delivery fails silently](https://atomgit.com/atomgit_atomcode/atomcode/issues/880)
- [#1324 w10 v5.0.4 TUI出现标号文字重叠问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/1324)
- [#1335 tui v5.0.5 win10在429以后添加provider, 写完4项以后按回车没反应](https://atomgit.com/atomgit_atomcode/atomcode/issues/1335)
- [#1347 [bug] 新对话自动名称变成 <system-reminder>；/webui 输入中文在终端显示乱码](https://atomgit.com/atomgit_atomcode/atomcode/issues/1347)
- [#1357 [共创大赛]-[Feature] atomcode工作时，若配置的多模态模型，可以让agent自己就能看到png的图，现在多模态模型接入无法查看图片](https://atomgit.com/atomgit_atomcode/atomcode/issues/1357)
- [#1358 askpass 测试使用固定临时目录，可能因目录残留而失败](https://atomgit.com/atomgit_atomcode/atomcode/issues/1358)
- [#1361 [Bug/确认] MCP client 是否注入 server initialize 返回的 instructions 字段（说话纪律等行为约束未生效）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1361)
- [#1362 [bug] /webui 每轮对话结束后终端回显乱码的 <system-reminder> 块（UTF-8 破坏 + system 消息泄漏）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1362)
- [#1363 [Bug] atomcode多模态支持设计路线完全错误，再次催促官方合入多模态BU修复的PR](https://atomgit.com/atomgit_atomcode/atomcode/issues/1363)
- [#1371 任务规划不够详细](https://atomgit.com/atomgit_atomcode/atomcode/issues/1371)
- [#1377 fix(capabilities): 清理 LSP 写入失败后的 pending 请求](https://atomgit.com/atomgit_atomcode/atomcode/issues/1377)
- [#1384 [feature] 安全拦截触发时不应直接终止回合，应给用户提供可选择、可执行的替代方案](https://atomgit.com/atomgit_atomcode/atomcode/issues/1384)
- [#1386 [共创大赛]-[Feature]](https://atomgit.com/atomgit_atomcode/atomcode/issues/1386)
- [#1387 [Bug] WebUI turn 进行中无法发送 prompt（输入被吞/按钮无反应）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1387)
- [#1388 [共创大赛][Bug] 添加自定义 provider，`Base URL` 显示不全](https://atomgit.com/atomgit_atomcode/atomcode/issues/1388)
- [#1389 [Bug] 插件 hooks 配置 {"hooks":{...}} 包装格式不兼容，导致 plugin trust 报 "no hooks"](https://atomgit.com/atomgit_atomcode/atomcode/issues/1389)
- [#1392 【Bug / WebUI 移动端】聊天输入框无法换行：手机软键盘回车键直接发送消息](https://atomgit.com/atomgit_atomcode/atomcode/issues/1392)

## v5.0.5 (2026-08-07)

### 概览

这一版让 Goal 成为可持续推进的目标，支持任务运行中追加引导、回合结束给出下一步建议，并能原样编辑 GBK 编码文件。

### 更新内容

- **Goal 持续目标**：目标达成或用完轮数后仍会保留，下一条消息可以接着推进；按 Esc 暂停，下次提交时恢复；界面上常驻显示目标状态。
- **执行中引导与下一步建议**：任务运行中可以追加引导消息；回合结束后根据结果给出下一句输入建议。
- **原样编辑 GBK / GB18030 文件**：编辑工具保留文件原有的编码。
- **目录选择器改版**：/cd 的目录选择器重新设计；输入的路径前缀不存在时，回车落到高亮的匹配项。
- **服务地址全部可配置**：所有服务端地址都可以在配置中修改，便于私有化部署。
- **若干修复**：.mcp.json 支持注释，MCP 工具名规范化、协议版本协商；首次临时 429 静默重试；路径不存在时提示最近的上级目录；凭据类 shell 命令不再反复重试；浅色、深色终端下的文字对比度；旧版 Windows 控制台调整窗口大小后的重排等。

### Issues

- [#894 使用问题：终端交互界面缩放（窗口大小调整）因未做防抖处理的SIGWINCH信号引发类内存溢出卡死](https://atomgit.com/atomgit_atomcode/atomcode/issues/894)
- [#1252 bug: 同名 skill 不同目录时去重逻辑误删全部而非保留一个 — `/skills dedup-skill` 结果为空](https://atomgit.com/atomgit_atomcode/atomcode/issues/1252)
- [#1287 模型提问时, 模型输出在cli里显示不全, 看起来新增的提问区域把输出遮挡了](https://atomgit.com/atomgit_atomcode/atomcode/issues/1287)
- [#1290 [共创大赛][Bug] 【Bug】历史会话迁移后按"回合"被拆成多个独立会话，旧格式文件未清理，/login 同步后会话列表错乱](https://atomgit.com/atomgit_atomcode/atomcode/issues/1290)
- [#1291 [共创大赛][Bug] TUI 窗口最大化/恢复后界面消失](https://atomgit.com/atomgit_atomcode/atomcode/issues/1291)
- [#1292 todolist很难触发](https://atomgit.com/atomgit_atomcode/atomcode/issues/1292)
- [#1293 [共创大赛][Bug] 首次使用/webui报错，无法正常打开web界面](https://atomgit.com/atomgit_atomcode/atomcode/issues/1293)
- [#1295 5.0.4版报告 os error 5](https://atomgit.com/atomgit_atomcode/atomcode/issues/1295)
- [#1296 [5.0.3][Windows] datalog 追加 .jsonl 路径在沙箱身份下报 os error 5，每轮对话弹 "session transcript was not saved"](https://atomgit.com/atomgit_atomcode/atomcode/issues/1296)
- [#1297 [Bug] request_user_input 在 TUI 端选择后，WebUI 对应弹窗未隐藏](https://atomgit.com/atomgit_atomcode/atomcode/issues/1297)
- [#1298 [共创大赛][Bug]](https://atomgit.com/atomgit_atomcode/atomcode/issues/1298)
- [#1299 [共创大赛]-[Feature] 新增功能](https://atomgit.com/atomgit_atomcode/atomcode/issues/1299)
- [#1302 5.0.4: session transcript .jsonl save fails with EACCES on Windows (os error 5)](https://atomgit.com/atomgit_atomcode/atomcode/issues/1302)
- [#1303 llm在尝试读取secrets时遭到cli阻止导致多次工具调用失败从而出现对话中断](https://atomgit.com/atomgit_atomcode/atomcode/issues/1303)
- [#1304 [共创大赛][Bug] session transcript was not saved](https://atomgit.com/atomgit_atomcode/atomcode/issues/1304)
- [#1305 mcp stdio的不知道为啥用不了了 w10 v5.0.4](https://atomgit.com/atomgit_atomcode/atomcode/issues/1305)
- [#1306 [共创大赛][Bug] 一直出这个警告“⚠ session transcript was not saved; check available disk space: C:\Users\73887\.atomcode\sessions\51b941699e6d8685\bf2f0af7-4c5d-43d4-94cf-404dc148ad8c.jsonl: 拒绝访问。 (os error 5)”](https://atomgit.com/atomgit_atomcode/atomcode/issues/1306)
- [#1308 [共创大赛][Bug] 在webui 中做出选择后cli 的选择框未及时同步关闭](https://atomgit.com/atomgit_atomcode/atomcode/issues/1308)
- [#1313 [Bug] 使用 /webui 后直接退出，会话结束时报 "session transcript was not saved; ... 拒绝访问 (os error 5)"，jsonl 落盘为 0 字节](https://atomgit.com/atomgit_atomcode/atomcode/issues/1313)
- [#1314 [Bug] Windows 平台会话转录文件（.jsonl）持续写入失败 — 5.0.2/5.0.4 均复现，transcript writer regression](https://atomgit.com/atomgit_atomcode/atomcode/issues/1314)
- [#1315 [Bug] 多开 atomcode 进程时，会话结束同样报 "session transcript was not saved; ... 拒绝访问 (os error 5)"，jsonl 为 0 字节](https://atomgit.com/atomgit_atomcode/atomcode/issues/1315)
- [#1316 [Bug] /cd 切换工作目录时可选目录数量过少，少于 /webui 浏览器界面展示的项目分组](https://atomgit.com/atomgit_atomcode/atomcode/issues/1316)
- [#1317 [Bug] Windows 5.0.4 会话结束时 transcript 保存失败: 拒绝访问 (os error 5)](https://atomgit.com/atomgit_atomcode/atomcode/issues/1317)
- [#1320 [Bug] Windows: 活跃会话 transcript jsonl 写入报 os error 5（拒绝访问），datalog 主记录正常](https://atomgit.com/atomgit_atomcode/atomcode/issues/1320)
- [#1321 [Windows] Session transcript save fails with 拒绝访问 (os error 5) since auto-update — all session .jsonl stay 0 bytes](https://atomgit.com/atomgit_atomcode/atomcode/issues/1321)
- [#1323 [BUG] session transcript 拒写 os error 5: append_jsonl_line lock_exclusive 撞同session snapshot写流程排它锁规律触发](https://atomgit.com/atomgit_atomcode/atomcode/issues/1323)

## v5.0.4 (2026-08-04)

### 概览

这一版新增本地定时任务，可注册到系统调度器无人值守运行；同时加入 /rewind 命令和输入历史搜索。

### 更新内容

- **本地定时任务**：新增 atomcode schedule 子命令（add / list / remove / enable / disable），自动注册到系统调度器（launchd、systemd、Windows 任务计划程序），无人值守运行时拒绝高风险命令；在 TUI 中用 /schedule 查看任务列表。
- **/rewind 与历史搜索**：新增 /rewind 打开检查点选择器；Ctrl+R 反向搜索输入历史。
- **登录后自动同步 CodingPlan 模型**：登录 CodingPlan 后自动拉取可用模型。
- **项目记忆目录可自定义**：用 ATOMCODE_PROJECT_MEMORY_DIR 指定项目级记忆存放的位置。
- **若干修复**：网络中断时保留已输出的内容，并提示可能的代理原因（10054）；Windows 粘贴截图；工具块渲染重叠；深色主题下灰色文字看不清；VSCode / JetBrains 插件内置程序的更新替换；启动速度提升等。

### Issues

- [#1255 memory 工具 project scope 存储路径硬编码 .atomcode/，建议支持宿主自定义](https://atomgit.com/atomgit_atomcode/atomcode/issues/1255)
- [#1258 rewind 不能用](https://atomgit.com/atomgit_atomcode/atomcode/issues/1258)
- [#1260 [共创大赛][Bug] 无法粘贴图片提示[错误：剪贴板中没有图片。]](https://atomgit.com/atomgit_atomcode/atomcode/issues/1260)
- [#1268 [共创大赛][Bug] AtomCode 扩展 0.1.2 后台持续写 rewind 快照（git 仓库），导致 cmd/git.exe 窗口频繁闪现、C 盘被持续占满、部分会话历史对话无法显示](https://atomgit.com/atomgit_atomcode/atomcode/issues/1268)
- [#1269 [Bug] 鸿蒙 PC 上 /provider 配置界面无法使用 Ctrl+A 添加快捷键添加模型](https://atomgit.com/atomgit_atomcode/atomcode/issues/1269)
- [#1271 [共创大赛][Bug] request_user_input 在多条驱动路径下被静默 Null 应答，导致前端无弹窗、模型误判"用户取消"](https://atomgit.com/atomgit_atomcode/atomcode/issues/1271)
- [#1275 [共创大赛][Bug] 删除会话失败，请查看 AtomCode 日志了解详情。](https://atomgit.com/atomgit_atomcode/atomcode/issues/1275)
- [#1276 [共创大赛]-[Feature] 能否加速一下会话删除功能，当我删除大量会话的时候要等一会才能删除](https://atomgit.com/atomgit_atomcode/atomcode/issues/1276)
- [#1277 [共创大赛][Bug] webui 会话列表选中后滚动位置丢失——点击深层会话后列表回滚到顶部，且从不滚动到选中项](https://atomgit.com/atomgit_atomcode/atomcode/issues/1277)
- [#1279 [共创大赛][Bug] webui左侧的对话列表里面点击其他会话的时候，如果当前会话没有结束，会触发当前会话中断，并在喇叭里面听到响声。稳定复现。](https://atomgit.com/atomgit_atomcode/atomcode/issues/1279)
- [#1281 [共创大赛][Bug] atomcode工具内部指令优先级超越用户指令，用户输入明确禁止编译和禁止执行脚本，但是任务完成后还是执行了脚本](https://atomgit.com/atomgit_atomcode/atomcode/issues/1281)
- [#1283 TUI 流式长行重复扫描缓冲，导致渲染性能退化](https://atomgit.com/atomgit_atomcode/atomcode/issues/1283)
- [#1288 [共创大赛][Bug] 双击ESC看更新日志改进了Rewind功能，但是我的5.0.3没生效](https://atomgit.com/atomgit_atomcode/atomcode/issues/1288)

## v5.0.3 (2026-07-30)

### 概览

这一版重做了 /provider 管理面板，新增基于检查点的安全回退、工具输出存档和回合上限检查点，长会话更可控。

### 更新内容

- **/provider 管理面板重做**：分账号、模型两个页签，可按厂商预设添加（新增 TaoToken 预设），可编辑账号与模型；/model 按账号分组。
- **安全回退**：基于工作区检查点，把会话和代码改动一起退回到之前的某一步。
- **工具输出存档**：超长的工具输出另存一份，模型按需用 fetch_output 分页读取，长会话不再膨胀。
- **回合上限检查点**：达到 [coding] max_rounds 设定的轮数时，询问继续还是停止，而不是直接中断。
- **TUI 体验**：用户输入带背景色块；子任务进度固定在底栏；新增切换模型的快捷键；/skills 支持一次选多个并模糊过滤；新增 shell 自动补全。
- **/cost 更准确**：按 models.dev 的价格计算，并按 Provider 和模型分别统计。
- **若干修复**：Windows 改用系统 TLS，绕过指纹拦截；AtomGit 401 后自动恢复登录；鸿蒙系统没有 bash 时改用 sh；MCP stdio 进程退出后自动恢复；WebUI 同步模式支持 /compact、上传的图片刷新后不再丢失、回合进行中拒绝切换模型等。

### Issues

- [#1188 [Feature] App 端支持 request_user_input 工具交互（选择弹窗）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1188)
- [#1193 [Bug] webui上传图片bug](https://atomgit.com/atomgit_atomcode/atomcode/issues/1193)
- [#1194 [共创大赛][Bug] 配置了provider不生效，终端还是提示让配置provider，无法正常使用](https://atomgit.com/atomgit_atomcode/atomcode/issues/1194)
- [#1199 Installer may skip PATH update when an existing entry only contains the install directory as a substring](https://atomgit.com/atomgit_atomcode/atomcode/issues/1199)
- [#1201 [共创大赛][Bug] 版本更新后 开启多个AtomCode窗口时，webui界面切换模型问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/1201)
- [#1202 [共创大赛]-[Feature] 自定义指令的参数配置和校验](https://atomgit.com/atomgit_atomcode/atomcode/issues/1202)
- [#1203 [Bug] atomcode.exe webui 直接启动时 MCP 工具在 Chat 中不可用](https://atomgit.com/atomgit_atomcode/atomcode/issues/1203)
- [#1204 atomcode 5.0.2 的 /cost 命令无法区分单个模型各自消耗的词元](https://atomgit.com/atomgit_atomcode/atomcode/issues/1204)
- [#1206 [共创大赛][Bug] VSCode 扩展聊天面板中 vscode://file/ 协议链接不可点击跳转](https://atomgit.com/atomgit_atomcode/atomcode/issues/1206)
- [#1207 [共创大赛][Bug] 无法登录+设置代理失败](https://atomgit.com/atomgit_atomcode/atomcode/issues/1207)
- [#1208 [共创大赛][Bug] TUI偶发出现两行或多行重复行](https://atomgit.com/atomgit_atomcode/atomcode/issues/1208)
- [#1209 429等错误不应直接结束](https://atomgit.com/atomgit_atomcode/atomcode/issues/1209)
- [#1210 BUG: v5.0.2 crash-orphaned .lease / .meta.lock 0-byte files block SessionManager::delete — session stuck as undeletable](https://atomgit.com/atomgit_atomcode/atomcode/issues/1210)
- [#1211 BUG: v5.0.2 legacy .json + native .ui.json dual-form coexistence causes session click to jump to a new session](https://atomgit.com/atomgit_atomcode/atomcode/issues/1211)
- [#1212 BUG: v5.0.2 fork_native_session leaves orphaned fork copies shattering one conversation into many session rows](https://atomgit.com/atomgit_atomcode/atomcode/issues/1212)
- [#1215 [BUG] 会话JSON末尾缺turn_count必填字段导致反序列化失败,会话点开卡死](https://atomgit.com/atomgit_atomcode/atomcode/issues/1215)
- [#1217 [共创大赛][Bug] 在补充回答大模型的问题场景下，无法粘贴信息,只能键盘输入](https://atomgit.com/atomgit_atomcode/atomcode/issues/1217)
- [#1222 [共创大赛][Bug]](https://atomgit.com/atomgit_atomcode/atomcode/issues/1222)
- [#1223 [Bug] /webui 启动后 TUI 未获得 sync 状态，webui 消息不同步到 TUI](https://atomgit.com/atomgit_atomcode/atomcode/issues/1223)
- [#1225 内容重复展示](https://atomgit.com/atomgit_atomcode/atomcode/issues/1225)
- [#1229 [共创大赛][Bug]AtomCode多窗口时，在webui删除会话404](https://atomgit.com/atomgit_atomcode/atomcode/issues/1229)
- [#1230 [共创大赛][Bug]](https://atomgit.com/atomgit_atomcode/atomcode/issues/1230)
- [#1231 cli使用领取的glm5.2时, 莫名其妙使用了不存在也未声明的brainstorming skill, 并且markdown表格底边显示不全, 还出现了滚动页面文字重复和内容显示不全的问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/1231)
- [#1234 [共创大赛][bug] GLM-5.2是支持1m的，还写200k就不对了](https://atomgit.com/atomgit_atomcode/atomcode/issues/1234)
- [#1237 [共创大赛][Bug]](https://atomgit.com/atomgit_atomcode/atomcode/issues/1237)
- [#1238 模型回复太慢的问题请求解决](https://atomgit.com/atomgit_atomcode/atomcode/issues/1238)
- [#1240 [Bug] PluginManager Marketplace 选择后按 Enter 导致 atomcode abort 退出](https://atomgit.com/atomgit_atomcode/atomcode/issues/1240)
- [#1241 插件 marketplace 安装的 skill 无法被 $ 和 /skills 发现](https://atomgit.com/atomgit_atomcode/atomcode/issues/1241)
- [#1242 [Bug] WebUI 刷新后图片和 error 信息不展示](https://atomgit.com/atomgit_atomcode/atomcode/issues/1242)
- [#1243 [bug] vision_preprocessor_provider 未配置/配置不存在的 model 时，图片没有透传，显示 [图片识别失败]](https://atomgit.com/atomgit_atomcode/atomcode/issues/1243)
- [#1244 [bug] webui 贴图发送后，同步的 TUI 只显示文本不显示图片](https://atomgit.com/atomgit_atomcode/atomcode/issues/1244)
- [#1245 [Bug] /skills 贪婪匹配多个 skill 名未生效](https://atomgit.com/atomgit_atomcode/atomcode/issues/1245)
- [#1246 [Bug] WebUI 模型下拉框未展示 CodingPlan 等新 schema 模型](https://atomgit.com/atomgit_atomcode/atomcode/issues/1246)
- [#1247 [Bug] WebUI sync 模式下 /compact 提示"暂不支持"，无法压缩会话](https://atomgit.com/atomgit_atomcode/atomcode/issues/1247)
- [#1248 [Bug] WebUI sync 模式：模型与 TUI 不一致 + 切模型中断 TUI 任务 + "继续"不接中断的现场](https://atomgit.com/atomgit_atomcode/atomcode/issues/1248)
- [#1251 [bug] daemon live hub 广播未按 session_id 过滤，不同会话的模型切换互串](https://atomgit.com/atomgit_atomcode/atomcode/issues/1251)

## v5.0.2 (2026-07-24)

### 概览

这是一个稳定性更新：自定义指令参数生效，配置出错不再导致无法启动，网络兼容性进一步改善。

### 更新内容

- **自定义指令参数生效**：自定义指令的 args 字段真正生效，参数会填进指令内容。
- **配置容错**：无效的 provider 配置段会被隔离，不再导致启动失败，/model 等写入照常进行。
- **网络兼容**：AtomGit 连接失败时改用 TLS 1.2 重试；单个损坏的系统根证书不再导致无法联网。
- **若干修复**：WebUI 切换项目时同步 TUI 状态、模型选择器轮询优化；安装脚本按完整路径项判断 PATH。

### Issues

- [#518 [共创大赛]-[Feature] git commit message 有时会是英文](https://atomgit.com/atomgit_atomcode/atomcode/issues/518)
- [#1015 需求：建议VSCode插件支持实时展示模型规划的任务清单](https://atomgit.com/atomgit_atomcode/atomcode/issues/1015)
- [#1022 [共创大赛][Bug] idea atomcode插件还在对话中，报login failed](https://atomgit.com/atomgit_atomcode/atomcode/issues/1022)
- [#1175 [Bug] VSCode/JetBrains 插件运行模式与 TUI 不一致（缺 AcceptEdits，命名不统一）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1175)
- [#1195 无有效移除项目功能，且“设为默认目录（重启后仍生效）。”并不会生效。](https://atomgit.com/atomgit_atomcode/atomcode/issues/1195)
- [#1200 [Bug] WebUI 切换项目目录后未同步到其他端，sync 状态丢失](https://atomgit.com/atomgit_atomcode/atomcode/issues/1200)

## v5.0.1 (2026-07-24)

### 概览

这一版让模型能在界面里直接向你提问，加强了项目 MCP 与插件 hooks 的信任管理，并改善了企业网络环境下的连接。

### 更新内容

- **选择题直接在界面中回答**：模型需要你做决定时，会在 TUI 或 WebUI 中弹出单选、多选或自由填写的问题，多个问题可以一次答完；默认开启。
- **项目 MCP 信任**：项目 .mcp.json 里的 MCP 服务器需要先 /mcp trust 才会连接，可用 /mcp untrust 撤回；.mcp.json 支持注释；插件 hooks 信任后才运行，插件内容变化后会重新询问。
- **企业网络兼容**：跟随系统代理（Windows / macOS）；信任系统根证书并支持 SSL_CERT_FILE；登录连接失败时给出排查引导。
- **TUI 体验**：等待时显示实时 token 计数，长任务不再像卡住；/resume 选择器改版并标出删除、重命名的按键；/status、/diff、/rename 等可在任务进行中执行；/todo add 直接添加任务。
- **技能更容易触发**：技能目录写进系统提示，匹配的技能会优先使用；子代理 task 工具默认开启。
- **若干修复**：Bash 超时或取消时结束整个进程树（Windows / Unix）；编辑工具更能容忍空白差异；宽表格渲染、Windows 终端二维码、TERM=dumb 下方向键失效；外部模型 429 不再误报 CodingPlan 超额；接近上下文上限时为输出预留空间等。

### Issues

- [#514 [共创大赛][Bug] ❌ 不使用 Windows 证书库 ❌ 不读取 SSL_CERT_FILE ❌ 不信任系统 CA store ❌ OCSP/CRL 在企业代理下失败 ❌ 直接 connection failed](https://atomgit.com/atomgit_atomcode/atomcode/issues/514)
- [#693 [共创大赛]-[Feature] 添加 AskUserQuestion 交互式问答工具和 TUI overlay](https://atomgit.com/atomgit_atomcode/atomcode/issues/693)
- [#817 提问，调用mcp，选了总是，但还是需要反复确认](https://atomgit.com/atomgit_atomcode/atomcode/issues/817)
- [#956 [P0] provider/retry.rs std::thread::sleep 阻塞 tokio 运行时风险](https://atomgit.com/atomgit_atomcode/atomcode/issues/956)
- [#960 [P1] pre_exec 闭包缺少 async-signal-safe 约束注释](https://atomgit.com/atomgit_atomcode/atomcode/issues/960)
- [#964 [P2] 部分 ? 传播链路缺少 .context() 错误注解](https://atomgit.com/atomgit_atomcode/atomcode/issues/964)
- [#977 [共创大赛]-[Feature] 当.claude文件夹被添加到.gitignore忽略时, atomcode cli无法艾特其下的skills, 也不会自动使用其下的skills](https://atomgit.com/atomgit_atomcode/atomcode/issues/977)
- [#1004 [Bug] TUI 中执行斜杠命令的输出未同步到 webui](https://atomgit.com/atomgit_atomcode/atomcode/issues/1004)
- [#1006 [Bug] TUI 中 /cd 切换目录和 /resume 切换会话未同步到 webui](https://atomgit.com/atomgit_atomcode/atomcode/issues/1006)
- [#1043 bug：任务完成后AtomCode自己给自己发了一句毫无关联的对话并自己澄清了（疑似与他人对话串一起了）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1043)
- [#1045 [共创大赛][Bug] webui输入prompt发送之后不见了，但是同步状态下的tui会显示](https://atomgit.com/atomgit_atomcode/atomcode/issues/1045)
- [#1074 FAQ文档中的几处错误：auth.json文件名与cargo install路径](https://atomgit.com/atomgit_atomcode/atomcode/issues/1074)
- [#1076 bug：正常对话过程中突发的AtomCode无响应(AtomCode daemon started but not responding)](https://atomgit.com/atomgit_atomcode/atomcode/issues/1076)
- [#1097 bug：上下文对话的搜索关键词功能失效](https://atomgit.com/atomgit_atomcode/atomcode/issues/1097)
- [#1102 Plugin loader 应兼容 Claude Code 默认目录约定（plugin.json 未显式声明时下钻扫描）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1102)
- [#1112 [Plugin] 插件描述截断无省略提示，用户无法感知内容被截断](https://atomgit.com/atomgit_atomcode/atomcode/issues/1112)
- [#1113 [Doc] bundled docs 快照（docs-zh.json）未包含 /loop 相关内容](https://atomgit.com/atomgit_atomcode/atomcode/issues/1113)
- [#1114 [共创大赛][Bug] 429处理问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/1114)
- [#1117 新的usage没有以前cost好用](https://atomgit.com/atomgit_atomcode/atomcode/issues/1117)
- [#1118 [共创大赛][Bug]](https://atomgit.com/atomgit_atomcode/atomcode/issues/1118)
- [#1121 增加粘贴图片alt+v快捷键](https://atomgit.com/atomgit_atomcode/atomcode/issues/1121)
- [#1123 check_destructive_command 漏判单文件删除与 mv 移出目录，删除审批可被等价命令绕过](https://atomgit.com/atomgit_atomcode/atomcode/issues/1123)
- [#1124 [共创大赛]-[Feature] 新增 ask 内置工具，支持 LLM 在对话中向用户发起单选/多选/文本输入](https://atomgit.com/atomgit_atomcode/atomcode/issues/1124)
- [#1125 [共创大赛][Bug] 终端TUI模式下markdown表格没有渲染显示，直接显示的markdown源码，网页模式正常](https://atomgit.com/atomgit_atomcode/atomcode/issues/1125)
- [#1126 [共创大赛]-[Feature] 建议安装脚本支持MSYS2](https://atomgit.com/atomgit_atomcode/atomcode/issues/1126)
- [#1127 [共创大赛]-[Feature] 聊天窗口字体问题 + 文件路径截断问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/1127)
- [#1131 [共创大赛][Bug] TUI 模式下 `use_skill` 工具无法识别已安装 plugin 的 skill](https://atomgit.com/atomgit_atomcode/atomcode/issues/1131)
- [#1138 [共创大赛][Bug] webui 搜索功能不合理改造优化](https://atomgit.com/atomgit_atomcode/atomcode/issues/1138)
- [#1141 atomcode在windows 10系统中，无法用上下箭头选择菜单项](https://atomgit.com/atomgit_atomcode/atomcode/issues/1141)
- [#1145 atomcode的/cost命令在windows 10系统中后续命令失效](https://atomgit.com/atomgit_atomcode/atomcode/issues/1145)
- [#1146 接入商汤的免费ds4f无法调用工具, 测试过cc没问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/1146)
- [#1150 TUI模式下，复制窗口文本，不能保留原有格式](https://atomgit.com/atomgit_atomcode/atomcode/issues/1150)
- [#1151 [共创大赛]-[Feature] 需要支持从环境变量获取apikey](https://atomgit.com/atomgit_atomcode/atomcode/issues/1151)
- [#1152 /desktop 下载的 桌面端跟功能介绍不一致](https://atomgit.com/atomgit_atomcode/atomcode/issues/1152)
- [#1153 relay-client: 3条daemon连接导致响应重复推送到手机端](https://atomgit.com/atomgit_atomcode/atomcode/issues/1153)
- [#1154 【bug】加载会话失败：corrupt presentation anchor — legacy turn boundaries 非严格递增](https://atomgit.com/atomgit_atomcode/atomcode/issues/1154)
- [#1156 [共创大赛][Bug] mac终端，首次启动 二维码有点问题，整个布局也是乱的](https://atomgit.com/atomgit_atomcode/atomcode/issues/1156)
- [#1157 [共创大赛][Bug] 删除.atomcode 目录，执行cargo run 直接输出一点内容然后闪退。](https://atomgit.com/atomgit_atomcode/atomcode/issues/1157)
- [#1158 [共创大赛][Bug] 在登录窗口，调整窗口大小，历史格式内容乱了，而且感觉很卡，resize的时候。](https://atomgit.com/atomgit_atomcode/atomcode/issues/1158)
- [#1160 [共创大赛][Bug] idea 终端，/login 命令不弹二维码](https://atomgit.com/atomgit_atomcode/atomcode/issues/1160)
- [#1161 [共创大赛][Bug] tools的调用后输出正文内容上面要一个空行，不然和tools的内容贴到一起了，没有层级。](https://atomgit.com/atomgit_atomcode/atomcode/issues/1161)
- [#1167 新建会话首个 prompt 报 provider 不可用（AwaitingProvider 无自恢复机制）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1167)
- [#1169 bug：疑似死循环了，一直在Bash同一个任务](https://atomgit.com/atomgit_atomcode/atomcode/issues/1169)
- [#1170 [Bug] TUI 模式切换未同步到 webui（Plan/Build/Auto/AcceptEdits）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1170)
- [#1171 [Bug] TUI 模式切换未同步到 App（Plan/Build/Auto/AcceptEdits）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1171)
- [#1173 [Bug] TUI/webui/App 三端同步缺陷汇总：目录、会话、模式切换未正确同步](https://atomgit.com/atomgit_atomcode/atomcode/issues/1173)
- [#1174 [Bug] webui 发送 prompt 后 TUI 未显示用户消息，只显示回答](https://atomgit.com/atomgit_atomcode/atomcode/issues/1174)
- [#1176 [共创大赛]-[Feature] 会话进行中，支持部分斜杠command](https://atomgit.com/atomgit_atomcode/atomcode/issues/1176)
- [#1177 [共创大赛][Bug] 我不清楚为什么它写着谢谢会跟 sed 干起来](https://atomgit.com/atomgit_atomcode/atomcode/issues/1177)
- [#1178 [Bug] 第三方模型欠费/超限时无错误提示，直接显示"已中断"](https://atomgit.com/atomgit_atomcode/atomcode/issues/1178)
- [#1179 [Bug] 同一目录不同终端窗口无法独立使用不同模型，一个终端切模型影响所有终端](https://atomgit.com/atomgit_atomcode/atomcode/issues/1179)
- [#1181 request_user_input 工具触发率低，需增强 persona 引导文案使模型主动调用](https://atomgit.com/atomgit_atomcode/atomcode/issues/1181)
- [#1182 [共创大赛][Bug] windows 首次启动，二维码无法展示](https://atomgit.com/atomgit_atomcode/atomcode/issues/1182)
- [#1184 [共创大赛][Bug] 耗时任务会出现死循环耗尽系统资源atomcode进程强制终止也不会释放](https://atomgit.com/atomgit_atomcode/atomcode/issues/1184)
- [#1185 [Bug] /usage 等斜杠命令的 ANSI 输出混入对话历史，不可读](https://atomgit.com/atomgit_atomcode/atomcode/issues/1185)
- [#1186 [共创大赛] Bug: /cd 在同一路径下创建新会话目录分组，导致 /resume 看不到其他分组会话](https://atomgit.com/atomgit_atomcode/atomcode/issues/1186)
- [#1187 [Feature] /cd 命令提示信息和site里面的文档描述要调整，目前/cd到对应目录是 切换工作目录并开启新建对话](https://atomgit.com/atomgit_atomcode/atomcode/issues/1187)
- [#1189 [Perf] /cd 和 /resume 重配置慢 — MCP eager connect 导致全量重建阻塞数秒](https://atomgit.com/atomgit_atomcode/atomcode/issues/1189)
- [#1190 [Bug] /resume 会话选择器中 Ctrl+D 删除无效果](https://atomgit.com/atomgit_atomcode/atomcode/issues/1190)

## v5.0.0 (2026-07-17)

### 概览

AtomCode 5.0 的第一个版本：统一四种执行模式与审批，只读工具并行执行，新增 /usage 用量面板，Todo 任务面板、插件市场和 /init 全面优化。

### 更新内容

- **Todo 任务面板优化**：按项增量更新任务，每轮核对进行中的任务防止漂移，还有未完成项时自动续跑一次；/todo clear 一键清空。
- **统一执行模式与审批增强**：按 Tab 在四种模式间切换：plan、build、auto（免审批）与 accept edits。
- **只读工具并发执行**：读文件、grep、只读 bash 自动并行，多工具回合更快。
- **/usage 用量面板（替代 /cost）**：当前 5 小时滚动窗口与 CodingPlan 套餐、60 天使用热力图、分模型折线图与用量表格。
- **/plugin 页面优化**：新增插件市场浏览与搜索框，显示安装范围，改为双行列表。
- **/init 优化**：由 Agent 分析仓库，自动生成 AGENTS.md；发现并共享 ~/.agents/skills 与 .agents/skills，技能可在不同 Agent 之间复用。
- **上下文、稳定性与终端兼容**：收紧工具输出上限，避免长会话膨胀；WebUI 断线自动重连，崩溃时增量保存。
- **若干修复**：切换模型后上下文窗口未刷新、/clear 真正新开会话、审批与回合展示优化等。

### Issues

- [#628 memory使用问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/628)
- [#755 [共创大赛][Bug] cd 切换了路径，问他所在路径还是没有变化](https://atomgit.com/atomgit_atomcode/atomcode/issues/755)
- [#794 [共创大赛] 对话压缩摘要prompt注入防护不足](https://atomgit.com/atomgit_atomcode/atomcode/issues/794)
- [#944 [共创大赛][Bug] 在浏览器中点了 “本会话总是允许”后，还是会第二次出来要确认 （Deepseek模型）](https://atomgit.com/atomgit_atomcode/atomcode/issues/944)
- [#955 [P0] MCP 注册表 std::sync::RwLock::write().unwrap() 锁中毒风险](https://atomgit.com/atomgit_atomcode/atomcode/issues/955)
- [#1018 bug：任务的执行耗时显示错误，显示0.0s，并且持续卡住（Read_file、Bash等）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1018)
- [#1036 [Enhancement] VS Code 插件会话列表未按工作目录区分展示](https://atomgit.com/atomgit_atomcode/atomcode/issues/1036)
- [#1037 [TUI][Bug] /model 切换模型输入关键字过滤时，输入内容未在输入框显示](https://atomgit.com/atomgit_atomcode/atomcode/issues/1037)
- [#1040 [Enhancement] loop 功能未在功能文档中增加相关说明](https://atomgit.com/atomgit_atomcode/atomcode/issues/1040)
- [#1041 [Enhancement] /todo 功能未在功能文档中增加相关说明](https://atomgit.com/atomgit_atomcode/atomcode/issues/1041)
- [#1042 [共创大赛][Bug] 执行到一半webui刷新内容全丢失](https://atomgit.com/atomgit_atomcode/atomcode/issues/1042)
- [#1047 [Bug] App 中出现重复消息显示，过一会儿又恢复正常](https://atomgit.com/atomgit_atomcode/atomcode/issues/1047)
- [#1053 [Bug] Windows 下刚打开时插件更新信息逃逸到了输入框](https://atomgit.com/atomgit_atomcode/atomcode/issues/1053)
- [#1055 [Bug] Windows 和鸿蒙下 /app 指令生成二维码失败（relay-client 未找到）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1055)
- [#1059 [Bug] iOS GitCode App 扫码未连接上 atomcode 时黑屏](https://atomgit.com/atomgit_atomcode/atomcode/issues/1059)
- [#1085 [Bug] App 中连续输入 2 个 prompt，排队中的 prompt 被中断不执行](https://atomgit.com/atomgit_atomcode/atomcode/issues/1085)
- [#1094 SseDecoder:finish_reason 为 stop 等终止原因时不终止,退化的流式响应导致解码循环不结束](https://atomgit.com/atomgit_atomcode/atomcode/issues/1094)
- [#1095 [共创大赛][Bug] 怎么禁止atomcode 执行git命令](https://atomgit.com/atomgit_atomcode/atomcode/issues/1095)
- [#1096 [共创大赛]-添加Skills支持](https://atomgit.com/atomgit_atomcode/atomcode/issues/1096)
- [#1098 /app 使用报错](https://atomgit.com/atomgit_atomcode/atomcode/issues/1098)
- [#1099 [共创大赛][Bug] /cd 回车无法再输入路径，导致用户只能选择](https://atomgit.com/atomgit_atomcode/atomcode/issues/1099)
- [#1100 [共创大赛][Bug] Ctrl+o 输出的详细内容 和 思考内容，没有根据窗口大小来适配](https://atomgit.com/atomgit_atomcode/atomcode/issues/1100)
- [#1101 [共创大赛][Bug] 不能自动触发skill了](https://atomgit.com/atomgit_atomcode/atomcode/issues/1101)
- [#1104 [Bug][Win] TUI 输入框光标无法定位到行尾最后一个字符后面](https://atomgit.com/atomgit_atomcode/atomcode/issues/1104)
- [#1105 [Bug] 取消回合后输入新 prompt，旧任务列表仍然在底部展示](https://atomgit.com/atomgit_atomcode/atomcode/issues/1105)
- [#1106 [Bug] Plan mode 下 MCP 查询工具仍弹审批，未细化到工具粒度](https://atomgit.com/atomgit_atomcode/atomcode/issues/1106)
- [#1107 ［Bug］ /usage 页面在 Mac 下文字不可见（\x1b［90m 配色导致）](https://atomgit.com/atomgit_atomcode/atomcode/issues/1107)
- [#1108 [Bug] 插件市场界面 3 个问题：未选中 tab 文字不可见、缺搜索框、描述截断无省略提示](https://atomgit.com/atomgit_atomcode/atomcode/issues/1108)
- [#1109 [Bug] 删除插件市场时需手工先卸载插件，应自动卸载并弹确认框](https://atomgit.com/atomgit_atomcode/atomcode/issues/1109)
- [#1110 插件市场顶部 tab "Browse Marketplaces" 展示的是 plugin 而非 marketplace，建议调整名称](https://atomgit.com/atomgit_atomcode/atomcode/issues/1110)
- [#1111 [Bug] 插件详情页未展示 Marketplace 和 Version 字段](https://atomgit.com/atomgit_atomcode/atomcode/issues/1111)

## v4.26.0 (2026-07-09)

### 概览

这一版带来强弱模型协作的子代理委派、/loop 持续循环和 Todo 任务管理，WebUI 大幅增强，上下文压缩也更聪明。

### 更新内容

- **子代理强弱模型委派**：主模型负责编排，按服务商能力自动分档路由（强模型规划、快模型执行），多个子任务可以并行；默认关闭，设置环境变量 ATOMCODE_SUBAGENT=1 开启。
- **/loop 持续循环**：支持自定步调和固定间隔两种模式，Agent 按节奏自动重复推进任务，停止、轮数上限与取消都有完整处理。
- **/app 连接 GitCode APP**：GitCode APP 与 AtomCode 配对后，可以在 GitCode 上连接使用 AtomCode。
- **Todo 任务**：todowrite 工具管理任务清单，每轮把当前清单告诉模型，底栏显示当前任务和进度（N/M），/todo 命令查看清单。
- **WebUI 大幅增强**：斜杠命令全面接入（/undo、/compact、/context、/status、/config、/diff、/cost、/memory、/skills 等）；三档审批模式选择（Build / Plan / Bypass）；跨项目会话侧栏；会话内消息搜索与定位；消息时间标记；只在底部时自动跟随输出，并提供回到底部按钮。
- **上下文与压缩优化**：上下文溢出时自动多级压缩（机械压缩加 LLM 总结），压缩时保留近期工作上下文并限制摘要输入规模，历史压缩对缓存更友好；切换模型后按当前模型的窗口重新计算压缩压力。
- **导出与复制**：/save 把当前对话导出为 Markdown，/copy msg 复制完整回复；/provider 可设置 context_window，并提示 128k / 256k / 512k / 1m 等窗口档位。
- **若干修复**：修复 grep 把大文件整个读进内存、导致低配机器卡死的问题，以及其他体验问题。

### Issues

- [#844 [共创大赛][Bug] atomcode经常出现堆栈缓冲区溢出的问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/844)
- [#917 bug：VSCode插件中，一旦输出"示例（Pandoc）"，输出结果就会断层，无后续内容返回](https://atomgit.com/atomgit_atomcode/atomcode/issues/917)
- [#936 bug： 报错Failed to buffer the request body: length limit exceeded](https://atomgit.com/atomgit_atomcode/atomcode/issues/936)
- [#950 [共创大赛][Bug] vscode 终端运行atomcode，调整终端大小时atomcode界面会异常](https://atomgit.com/atomgit_atomcode/atomcode/issues/950)
- [#970 [共创大赛][Bug] harmonyos ctrl+v无法粘贴图片](https://atomgit.com/atomgit_atomcode/atomcode/issues/970)
- [#978 [Bug] 终端 UI：任务执行完成后转圈动画未清除](https://atomgit.com/atomgit_atomcode/atomcode/issues/978)
- [#979 【功能请求】新增内置 Todo List 待办清单功能](https://atomgit.com/atomgit_atomcode/atomcode/issues/979)
- [#980 [TUI][Bug] 从历史记录翻页回访 prompt 时 $ 前缀丢失，skill 无法调用](https://atomgit.com/atomgit_atomcode/atomcode/issues/980)
- [#983 [共创大赛][Bug] 当前会话的Token上下文剩余量无法正常查看](https://atomgit.com/atomgit_atomcode/atomcode/issues/983)
- [#985 鸿蒙pc无法黏贴图片](https://atomgit.com/atomgit_atomcode/atomcode/issues/985)
- [#987 [共创大赛]webui界面，模型输出时自动滚动增加开关，允许用户自由浏览历史记录](https://atomgit.com/atomgit_atomcode/atomcode/issues/987)
- [#993 [Bug] Windows 路径大小写漂移导致重启后历史对话列表为空（写入归一化但过滤未归一化）](https://atomgit.com/atomgit_atomcode/atomcode/issues/993)
- [#997 [Bug] 在历史对话中续聊必报 HTTP 400 "glm-5.2 is not a multimodal model"，新对话正常](https://atomgit.com/atomgit_atomcode/atomcode/issues/997)
- [#998 bug：输入的提示词和输出的结果被截断 （</script>截断）](https://atomgit.com/atomgit_atomcode/atomcode/issues/998)
- [#1001 [共创大赛][Bug] JetBrains插件输入报错400](https://atomgit.com/atomgit_atomcode/atomcode/issues/1001)
- [#1002 bug：VSCode插件中，排队中的提示词一直没被触发(整个对话结束了都没触发)](https://atomgit.com/atomgit_atomcode/atomcode/issues/1002)
- [#1003 [Bug] /app 扫码连接文案错误："AtomCode App" 应为 "GitCode App"](https://atomgit.com/atomgit_atomcode/atomcode/issues/1003)
- [#1005 [Bug] 手机 App 缺少 "Always Allow" 审批选项，且审批决策后卡片状态未同步](https://atomgit.com/atomgit_atomcode/atomcode/issues/1005)
- [#1010 [共创大赛]-[Feature] tui输入框在触发 bash 模式后，给用户交互提示](https://atomgit.com/atomgit_atomcode/atomcode/issues/1010)
- [#1012 bug：从「deepseek-v4-flash」模型切换到「GLM-5.2」模型后报错](https://atomgit.com/atomgit_atomcode/atomcode/issues/1012)
- [#1013 自动升级最新版本后不能使用了，网络有代理？](https://atomgit.com/atomgit_atomcode/atomcode/issues/1013)
- [#1016 [Bug] Android App 上弹了两个相同的审批卡片](https://atomgit.com/atomgit_atomcode/atomcode/issues/1016)
- [#1017 优化：AtomGit Bash的OUT的结果无法输出中文路径，显示问号](https://atomgit.com/atomgit_atomcode/atomcode/issues/1017)
- [#1019 [Bug] GitCode App 扫码连接后会话名称未展示](https://atomgit.com/atomgit_atomcode/atomcode/issues/1019)
- [#1020 [共创大赛][Bug] SKILL脚本调用失效](https://atomgit.com/atomgit_atomcode/atomcode/issues/1020)
- [#1034 [Bug] TUI/WebUI 中发送 prompt，App 显示 2 条相同消息，执行完后变为 1 条](https://atomgit.com/atomgit_atomcode/atomcode/issues/1034)

## v4.25.9 (2026-07-04)

### 概览

这一版让会话自动起名，终端标签页显示会话名和运行状态，一眼就能分清多个窗口。

### 更新内容

- **会话自动命名**：第一轮结束后由 AI 给会话起名，不会覆盖你用 /rename 设的名字，WebUI 同步显示；可在 [ui] ai_session_naming 关闭。
- **标签页状态灯**：终端标签标题显示会话名，并以 🟢 空闲、🟡 运行中、🔴 等待审批标示状态；可在 [ui] terminal_status_glyph 关闭。
- **/compact 不再卡住**：生成压缩摘要最多等 120 秒，超时即结束。
- **若干修复**：Windows 下 /cd 的路径不再带 \\?\ 前缀，/cd 目录列表去重；Git Bash 下的 > nul 自动改写为 /dev/null；Windows 下 WebUI 能正确打开浏览器；WebUI 的输出不再搅乱 TUI 画面；Ctrl+O 展开提示在工具开始时就显示。

### Issues

- [#945 [共创大赛][Bug] 版本显示不统一，如图](https://atomgit.com/atomgit_atomcode/atomcode/issues/945)
- [#966 [共创大赛]-[Feature] cli程序在标题栏添加一个工作状态标记](https://atomgit.com/atomgit_atomcode/atomcode/issues/966)
- [#967 [共创大赛][Bug] win下概率生成nul这个异常文件](https://atomgit.com/atomgit_atomcode/atomcode/issues/967)
- [#969 [共创大赛][Bug]](https://atomgit.com/atomgit_atomcode/atomcode/issues/969)
- [#975 [共创大赛][Bug] 疑似 ACP 协议不兼容](https://atomgit.com/atomgit_atomcode/atomcode/issues/975)
- [#976 Windows下WebUI处理prompt时TUI切换目录触发同步快照回放及daemon日志泄漏](https://atomgit.com/atomgit_atomcode/atomcode/issues/976)

## v4.25.8 (2026-07-03)

### 概览

这一版完善了 WebUI 的复制与导出，/status 能看清登录状态，@ 提及支持逐级进入目录，并改进了 Windows 下的 bash 识别。

### 更新内容

- **WebUI 复制与导出**：消息新增复制按钮，可复制整轮内容（含工具调用）；会话可导出为 Markdown 文件；历史图片正常显示，回合进行中被强行关闭也不再丢掉整轮。
- **/status 显示登录状态**：首行显示是否登录及昵称（用户名），登录过期时明确提示运行 /login；去掉 Token 行。
- **@ 提及进入目录**：@ 补全可以逐级进入子目录，选中项自动滚动到可见处；/ 与 @ 菜单的选中颜色统一。
- **Windows bash 识别**：能找到装在非 C 盘的 Git Bash，不再把 WSL 的应用别名当成 bash，并如实告诉模型当前用的是 Git Bash 还是 cmd.exe。
- **VS Code 插件**：支持 @ 引用文件、上下键翻输入历史，界面支持中英文。
- **若干修复**：自动复制代码块默认关闭，开启时也只在回复恰好一个代码块时复制；web_fetch 按页面编码解码，中文网页不再乱码、非 ASCII 页面不再崩溃；模型给出的路径支持 ~；MCP 工具参数显示上限放宽到 450 字；添加插件市场后提示可安装的插件和命令；resume 历史会话的工具名显示正常；回复过程中输入框光标正常显示；Windows 下 WebUI 打开浏览器不再失败。

### Issues

- [#63 [Feature] 支持acp](https://atomgit.com/atomgit_atomcode/atomcode/issues/63)
- [#447 [共创大赛]-[Feature] 支持acp和a2a以及 尝试支持claude code 的plugins市场](https://atomgit.com/atomgit_atomcode/atomcode/issues/447)
- [#811 [共创大赛][feat] 修改TUI 根据系统来制定不同的渲染方案，并设计降级策略，确保在不同的终端更好的交互。](https://atomgit.com/atomgit_atomcode/atomcode/issues/811)
- [#858 【Bug】VSCode 插件未处理 artifact 事件，可能导致代码块内容不展示](https://atomgit.com/atomgit_atomcode/atomcode/issues/858)
- [#868 [共创大赛]-[Feature] ACP 支持](https://atomgit.com/atomgit_atomcode/atomcode/issues/868)
- [#889 [共创大赛]-[Feature] 希望能够增加ACP支持](https://atomgit.com/atomgit_atomcode/atomcode/issues/889)
- [#893 [共创大赛]-[Feature] 优化微信插件消息](https://atomgit.com/atomgit_atomcode/atomcode/issues/893)
- [#902 [共创大赛][Bug]](https://atomgit.com/atomgit_atomcode/atomcode/issues/902)
- [#903 [共创大赛][Bug] BYPASS模式，使用 /skill 调用技能，还是会出现审批](https://atomgit.com/atomgit_atomcode/atomcode/issues/903)
- [#905 [共创大赛][Bug] Windows版本，如果用户没有启动浏览器，输入/webui命令，不会自动打开默认浏览器](https://atomgit.com/atomgit_atomcode/atomcode/issues/905)
- [#907 4.25.7 版本无法执行命令，windows环境](https://atomgit.com/atomgit_atomcode/atomcode/issues/907)
- [#908 [共创大赛][Bug] Windows版：会话中断重启后记录丢失（应有2条仅剩1条）](https://atomgit.com/atomgit_atomcode/atomcode/issues/908)
- [#909 [共创大赛]-[Feature] AtomCode for VS Code 增加 “输入历史记录导航（方向键 ↑/↓）” 功能](https://atomgit.com/atomgit_atomcode/atomcode/issues/909)
- [#910 [Bug] /resume 恢复会话后工具名称显示为原始 snake_case 而非 PascalCase](https://atomgit.com/atomgit_atomcode/atomcode/issues/910)
- [#912 长代码块内容被截断/横向滚动失效](https://atomgit.com/atomgit_atomcode/atomcode/issues/912)
- [#915 [共创大赛][Bug]](https://atomgit.com/atomgit_atomcode/atomcode/issues/915)
- [#916 在上一个问题回复过程中，输入框中光标不显示](https://atomgit.com/atomgit_atomcode/atomcode/issues/916)
- [#920 [共创大赛][Bug] ATOMCODE_PLAIN=1 模式下 OSC 11 终端探测响应泄漏为 ANSI 乱码](https://atomgit.com/atomgit_atomcode/atomcode/issues/920)
- [#921 [共创大赛][Bug] ATOMCODE_PLAIN=1 模式下自更新 eprintln 抢占启动输出](https://atomgit.com/atomgit_atomcode/atomcode/issues/921)
- [#928 [共创大赛]-[Feature] vscode插件通过@可引用文件夹/文件](https://atomgit.com/atomgit_atomcode/atomcode/issues/928)
- [#931 [共创大赛]-[Feature] 导出atomcode对话为markdown文件](https://atomgit.com/atomgit_atomcode/atomcode/issues/931)
- [#932 [共创大赛]-[Feature] 不要自动复制代码到剪切板](https://atomgit.com/atomgit_atomcode/atomcode/issues/932)
- [#933 bug：服务商返回空响应Provider returned an empty response](https://atomgit.com/atomgit_atomcode/atomcode/issues/933)
- [#934 bug：阅读文件时报错中断任务(no reasoning detected)](https://atomgit.com/atomgit_atomcode/atomcode/issues/934)
- [#935 [共创大赛][feat] webui 对话标题点击可以下来，修改对话标题，导出、删除功能](https://atomgit.com/atomgit_atomcode/atomcode/issues/935)
- [#937 [共创大赛][feat] webui用户发送的消息下面增加复制功能，还有大模型返回内容下面增加复制功能。](https://atomgit.com/atomgit_atomcode/atomcode/issues/937)
- [#940 [Feature] TUI 的 CodeReview 调用时未显示参数信息](https://atomgit.com/atomgit_atomcode/atomcode/issues/940)
- [#946 fix(acp): 内核死亡时 cmd_tx.send 错误被 .ok() 吞掉，客户端收到假 EndTurn](https://atomgit.com/atomgit_atomcode/atomcode/issues/946)
- [#947 refactor: 两个 crate 重复定义 strip_reasoning_filler 函数，应共享实现](https://atomgit.com/atomgit_atomcode/atomcode/issues/947)
- [#948 执行命令为啥一定要强制通过wsl？](https://atomgit.com/atomgit_atomcode/atomcode/issues/948)
- [#949 VS Code插件/命令列表无法滚动查看且高亮状态残留](https://atomgit.com/atomgit_atomcode/atomcode/issues/949)
- [#952 [BadCase] ReadFile 对 ~ 开头路径未展开家目录，被当作相对路径处理](https://atomgit.com/atomgit_atomcode/atomcode/issues/952)
- [#965 /app 命令的说明缺少中文 i18n 翻译](https://atomgit.com/atomgit_atomcode/atomcode/issues/965)

## v4.25.7 (2026-06-30)

### 概览

这一版重点改善了限流和长任务体验：遇到 429 会按套餐窗口自动等待或暂停，sudo 密码可以在界面里输入，/goal 更稳，压缩提示也更统一。

### 更新内容

- **限流自动等待**：遇到 429 时按真实的重置时间决定自动等待后继续，还是暂停并显示倒计时；TUI 和 WebUI 都会显示重置时间，不再当成报错。
- **sudo 密码输入框**：命令需要 sudo 或 ssh 密码时，在 TUI 里弹出隐藏输入框，可用 Ctrl+C 取消，回合结束自动关闭。
- **/goal 更稳**：遇到可恢复的中断会继续推进，新增轮数和时长上限，连续没有进展时停止，不再出现目标结束后又复活。
- **压缩提示统一**：自动和手动压缩用同一种进度提示和分隔标记；底栏始终显示上下文占用百分比。
- **双击 Esc 撤销**：输入框为空时连按两次 Esc 撤销上一轮，第一次会提示再按一次。
- **视觉模型能看图片**：用视觉模型时，read_file 读到的图片会直接交给模型。
- **长输出不再截断**：回复被输出上限截断时自动接着写，大文件改为分段写入。
- **若干修复**：模型把答案放进推理内容时也能正常显示；Windows 下 bash 检测、引号与中文 Python 输出问题修复；粘贴的文本在历史回溯和提交后正确展开；终端被信号结束后恢复正常；todo 更新显示任务标题；web_search 显示结果来源域名；波浪号不再被当成删除线；/login 显示真实套餐名。

### Issues

- [#540 [共创大赛][Bug] 拖拽程序窗口大小，会导致程序闪退](https://atomgit.com/atomgit_atomcode/atomcode/issues/540)
- [#736 [共创大赛][Bug] windows 上面 cmd和powershell 在对话中，用户不能线上滚动，一滚动又被强制回到了底部](https://atomgit.com/atomgit_atomcode/atomcode/issues/736)
- [#819 JetBrains插件需要增加插件主页，方便用户了解使用，支持中英文](https://atomgit.com/atomgit_atomcode/atomcode/issues/819)
- [#825 [WebUI][Bug] 同步模式下路径前的 ~ 指示符丢失](https://atomgit.com/atomgit_atomcode/atomcode/issues/825)
- [#827 [WebUI][Bug] sync 同步异常时 TUI prompt 未同步到 WebUI，且无法终止](https://atomgit.com/atomgit_atomcode/atomcode/issues/827)
- [#843 [共创大赛][Bug] 历史对话复制的内容无法使用](https://atomgit.com/atomgit_atomcode/atomcode/issues/843)
- [#845 [共创大赛][Bug] webui开启同步，切换session对话，TUI应该也要同步切换对话](https://atomgit.com/atomgit_atomcode/atomcode/issues/845)
- [#847 【Bug】JetBrains 插件 代码块（code block）无法正常显示](https://atomgit.com/atomgit_atomcode/atomcode/issues/847)
- [#848 JetBrains插件市场安装AtomCode下载失败且概览未加载](https://atomgit.com/atomgit_atomcode/atomcode/issues/848)
- [#850 [共创大赛][Bug] webui未开启同步的情况，新建对话TUI也会新建，应该不需要](https://atomgit.com/atomgit_atomcode/atomcode/issues/850)
- [#851 [共创大赛][feat] webui字体优化](https://atomgit.com/atomgit_atomcode/atomcode/issues/851)
- [#857 上下文使用进度条 / Context Usage Progress Bar](https://atomgit.com/atomgit_atomcode/atomcode/issues/857)
- [#861 能不能把那个gitbash集成到atomcode里, 现在这种终端兼容性太差了, 空格都识别不了, 而且经常出现cmd/pwsh/PowerShell混用的问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/861)
- [#872 [共创大赛] 上下文超200K后系统失效：5层防御缺口导致不可恢复](https://atomgit.com/atomgit_atomcode/atomcode/issues/872)
- [#884 [共创大赛]-[Feature] Ctrl+C 中断对话后保留已生成的上下文记忆](https://atomgit.com/atomgit_atomcode/atomcode/issues/884)
- [#885 [共创大赛][Bug] Codingplan Pro 中的 Qwen/Qwen3-VL-8B-Instruct 无法正常使用](https://atomgit.com/atomgit_atomcode/atomcode/issues/885)
- [#891 TUI中/skills技能执行内容未在WebUI同步展示](https://atomgit.com/atomgit_atomcode/atomcode/issues/891)
- [#892 [共创大赛][Bug] todo工具并行调用，未展示任务名称](https://atomgit.com/atomgit_atomcode/atomcode/issues/892)

## v4.25.6 (2026-06-25)

### 概览

这一版集中修了 Windows 下的大量问题，改进了自动压缩和 /cd，并新增统一的网络代理设置；WebUI 与 JetBrains 插件也有较多改进。

### 更新内容

- **统一代理设置**：新增 [network.proxy] 配置，可选跟随系统、固定代理或不走代理，覆盖所有对外连接；TUI 里用 /proxy 切换，/status 查看。
- **Windows 体验**：bash 工具不再每次闪出控制台窗口；命令输出按控制台编码正确解码；CRLF 换行和中文 GBK 文件不再让读取、编辑失败；/quit 不再卡住退不出。
- **自动压缩更可靠**：上下文接近上限时像 /compact 一样做摘要，不再只折叠工具结果；超长粘贴不再反复空跑摘要；压缩时显示进度。
- **/cd 更好用**：选择器里打字即过滤最近目录，也可以直接输入路径；切换目录后 /resume 列出的是新项目的会话。
- **编辑容错**：tab 与空格不一致时也能匹配上，不再逼模型改用脚本修改文件。
- **WebUI 改进**：新增欢迎页、技能与 MCP 菜单、会话搜索和按日期分组；新会话立刻出现在侧栏；同步开关刷新后保持；多开实例不再互相顶掉登录。
- **JetBrains 插件**：新增欢迎页和启动引导，支持粘贴或拖拽文件、粘贴图片作为附件，跟随 IDE 主题色。
- **若干修复**：MCP 的 trust/autoApprove 配置生效，「总是允许」对 MCP 持久；/setup 安装的 MCP 写到能被读取的 .mcp.json；流式中途连接重置自动重连并给出可读提示；/login 后不再报认证过期；窗口缩放后不再出现重复输出；会话改名后不再被覆盖；parallel_edit 显示逐文件进度。

### Issues

- [#173 [共创大赛][Bug] permission channel 关闭时静默返回 Deny，导致工具被误拒绝且缺少可观测性](https://atomgit.com/atomgit_atomcode/atomcode/issues/173)
- [#175 [共创大赛][Bug] OpenAI 流式 tool call：`ToolCallStart` 事件可能发送空 name，UI 显示空白工具名](https://atomgit.com/atomgit_atomcode/atomcode/issues/175)
- [#177 [共创大赛][Bug] 文件编辑跟踪依赖工具输出文本格式（“Edited …”），输出措辞变化/本地化会导致跟踪失效](https://atomgit.com/atomgit_atomcode/atomcode/issues/177)
- [#703 Windows下窗口全屏/缩放后会话历史重复，内容错乱表格未对齐](https://atomgit.com/atomgit_atomcode/atomcode/issues/703)
- [#709 [共创大赛][Bug] windows 11、10 PowerShell 放大缩小后，历史记录会多出2条](https://atomgit.com/atomgit_atomcode/atomcode/issues/709)
- [#713 [共创大赛]-[Feature] 非 JSON SSE 数据不应该显示空响应，应区分有数据但非法 和 没数据](https://atomgit.com/atomgit_atomcode/atomcode/issues/713)
- [#714 [共创大赛][Bug] 使用 atomcode -y 偶尔还是需要执行确认](https://atomgit.com/atomgit_atomcode/atomcode/issues/714)
- [#715 JetBrains插件输入栏按钮布局与快捷键说明UI优化建议](https://atomgit.com/atomgit_atomcode/atomcode/issues/715)
- [#716 JetBrains插件背景色需随编辑器主题联动，增加与编辑器的分隔线](https://atomgit.com/atomgit_atomcode/atomcode/issues/716)
- [#718 JetBrains插件未展示会话列表，不支持新建会话](https://atomgit.com/atomgit_atomcode/atomcode/issues/718)
- [#719 JetBrains插件没有打开的会话时设置按钮点击无效](https://atomgit.com/atomgit_atomcode/atomcode/issues/719)
- [#724 JetBrains插件建议支持文件拖拽、@选择文件和/命令](https://atomgit.com/atomgit_atomcode/atomcode/issues/724)
- [#725 [共创大赛][Bug] [错误：stream read error: error decoding response body: error reading a body from connection: unexpected EOF during chunk size line]](https://atomgit.com/atomgit_atomcode/atomcode/issues/725)
- [#727 [共创大赛][Bug] mcp服务权限问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/727)
- [#735 额度耗尽错误被误判为可重试:识别英文/结构化 quota 文案,避免 ~45s 无谓重试](https://atomgit.com/atomgit_atomcode/atomcode/issues/735)
- [#759 JetBrains插件建议支持粘贴截图](https://atomgit.com/atomgit_atomcode/atomcode/issues/759)
- [#760 JetBrains插件快捷键文本提示建议用placeholder展示在输入框内](https://atomgit.com/atomgit_atomcode/atomcode/issues/760)
- [#799 [共创大赛][feat] webui 改造](https://atomgit.com/atomgit_atomcode/atomcode/issues/799)
- [#813 [共创大赛][Bug] 频繁的读不到内容](https://atomgit.com/atomgit_atomcode/atomcode/issues/813)
- [#816 [共创大赛][Bug] webui 同步按钮开启和取消浏览器的sync参数要跟着变化，不然用户刷新一下又开启了](https://atomgit.com/atomgit_atomcode/atomcode/issues/816)
- [#818 # Bug: 多项目目录下先后执行 `/webui`，前一个 Web UI 页面 token 失效](https://atomgit.com/atomgit_atomcode/atomcode/issues/818)
- [#820 [共创大赛][Bug] webui 发送对话，左侧sessions列表不刷新、或者标题不止那是问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/820)
- [#824 [WebUI][Bug] 重命名会话后发送消息，会话名称被重置为默认名称](https://atomgit.com/atomgit_atomcode/atomcode/issues/824)
- [#826 [Bug] TUI 中切换目录未同步到 WebUI（/sync 会话）](https://atomgit.com/atomgit_atomcode/atomcode/issues/826)
- [#828 [共创大赛][Bug] 微信clawbot 调用OpenClaw 调用 atomcode去干活，cmd窗口一直在闪](https://atomgit.com/atomgit_atomcode/atomcode/issues/828)
- [#829 [Bug] /setup 安装 MCP 到 .atomcode/mcp.json 但加载器不识别该路径](https://atomgit.com/atomgit_atomcode/atomcode/issues/829)
- [#859 Windows下TUI执行任务时无法向上翻页查看历史消息，固定在底部且翻动闪烁](https://atomgit.com/atomgit_atomcode/atomcode/issues/859)

## v4.25.5 (2026-06-23)

### 概览

这是一个稳定性修复：坏掉的插件 hook 不再卡住每次提问，模型偶发空回复时自动重试。

### 更新内容

- **坏 hook 不再卡死**：插件 hook 启动失败或没给出原因就退出时，不再被当成拦截，提问和工具调用照常进行。
- **空回复自动重试**：服务偶尔返回完全空的回复时，自动重试几次并给出提示，不再让回合悄悄中断。
- **若干修复**：以管理员权限运行时的提示改为建议以普通权限运行。

### Issues

- [#635 [共创大赛][Bug] Provider returned an empty response (no text, no tool calls). no reason 无回应](https://atomgit.com/atomgit_atomcode/atomcode/issues/635)

## v4.25.4 (2026-06-23)

### 概览

这一版新增会话内代码审查 /review 和持续目标 /goal，推出 JetBrains IDE 插件，并优化了上下文压缩。

### 更新内容

- **/review 代码审查**：在会话中直接审查代码改动，内置各语言的审查规则，标注 diff 行号，按变更文件逐一覆盖。
- **/goal 持续目标**：设定一个目标后，Agent 会跨轮次自动朝它推进，并在每轮评估进展。
- **JetBrains IDE 插件**：提供多标签页聊天、会话状态管理和 diff 查看，与 IDE 界面集成。
- **上下文与压缩优化**：上下文溢出时自动多级压缩（机械压缩加 LLM 总结），历史压缩对缓存更友好。
- **若干修复**：切换模型后上下文窗口未刷新；/clear 真正新开会话；审批与回合展示优化等。

### Issues

- [#619 [共创大赛][Bug] ParallelEditFiles时仅能看到第一个文件名称](https://atomgit.com/atomgit_atomcode/atomcode/issues/619)
- [#633 [Bug] 插件市场 Skill 详情移动端页面留白太多，需优化](https://atomgit.com/atomgit_atomcode/atomcode/issues/633)
- [#686 /skills skill name 过滤应使用子串匹配而非前缀匹配](https://atomgit.com/atomgit_atomcode/atomcode/issues/686)
- [#694 [共创大赛][Bug] 使用GLM-5.2模型时，他无法使用复制的到CLI中的内容。](https://atomgit.com/atomgit_atomcode/atomcode/issues/694)
- [#697 [共创大赛]-[Feature] todo工具优化](https://atomgit.com/atomgit_atomcode/atomcode/issues/697)
- [#698 [共创大赛][Bug] 最近一直Web UI一直提示[错误: [warning] conversation compacted]文字变成红色，但任务继续进行。](https://atomgit.com/atomgit_atomcode/atomcode/issues/698)
- [#699 [共创大赛][Bug]](https://atomgit.com/atomgit_atomcode/atomcode/issues/699)
- [#700 长文本粘贴被截断成多段，显示异常](https://atomgit.com/atomgit_atomcode/atomcode/issues/700)
- [#701 bash命令执行后转义序列未过滤，输入框出现乱码panic](https://atomgit.com/atomgit_atomcode/atomcode/issues/701)
- [#702 [共创大赛][Bug] skill hub 安装命令直接运行会报错](https://atomgit.com/atomgit_atomcode/atomcode/issues/702)
- [#704 邀请页登录授权后重新进入需再次登录，登录态未持久化](https://atomgit.com/atomgit_atomcode/atomcode/issues/704)
- [#705 收款信息手机号输入未做校验，允许输入无效非数字字符](https://atomgit.com/atomgit_atomcode/atomcode/issues/705)
- [#706 强制杀进程后终端残留ANSI乱码](https://atomgit.com/atomgit_atomcode/atomcode/issues/706)
- [#707 WebUI停止按钮无法终止运行中的任务](https://atomgit.com/atomgit_atomcode/atomcode/issues/707)
- [#708 Markdown表格渲染时CJK字符导致表格边框|未对齐](https://atomgit.com/atomgit_atomcode/atomcode/issues/708)

## v4.25.3 (2026-06-20)

### 概览

这是一个小更新：文件写入审批恢复按路径区分，Windows 终端的鼠标操作恢复正常。

### 更新内容

- **写入审批按路径区分**：工作区内的普通文件改动自动放行；工作区外的文件按路径单独记住「总是允许」；敏感文件（如 .env、SSH 密钥）每次都询问。
- **Windows 鼠标恢复原生**：不再改动控制台模式，conhost 与 Windows Terminal 上的滚轮、拖选、复制、右键粘贴都恢复正常。
- **若干修复**：JetBrains 内置终端不再因鼠标移动出现乱码输入。

### Issues

- [#687 README 缺失 /wechat、/review、/goal 等斜杠命令的文档说明](https://atomgit.com/atomgit_atomcode/atomcode/issues/687)
- [#688 [共创大赛][Bug] 《总是》授权无法正确使用，目前《总是》授权后，还需要多次授权](https://atomgit.com/atomgit_atomcode/atomcode/issues/688)
- [#689 # AtomCode 聊天窗口鼠标失灵 Bug 报告](https://atomgit.com/atomgit_atomcode/atomcode/issues/689)
- [#690 鼠标移动后出现乱码](https://atomgit.com/atomgit_atomcode/atomcode/issues/690)
- [#691 [共创大赛][Bug] 更新后power shell 窗口不能上下滚动](https://atomgit.com/atomgit_atomcode/atomcode/issues/691)

## v4.25.2 (2026-06-19)

### 概览

这一版换上了新的 agent 引擎，取消、审批、用量显示更可靠，并修了大量 WebUI 同步方面的问题。

### 更新内容

- **取消即撤回**：按 Esc 取消的回合不再留在上下文里，模型之后不会再提起；卡在连接阶段的回合也能立即取消。
- **/compact 可指定重点**：/compact 后面可以加上重点，摘要会围绕它来写。
- **底栏用量更真实**：每轮显示实际计费的 token 和缓存命中比例，不再把每轮重发的上下文重复累加；token 数以 K/M 显示。
- **读取敏感文件需确认**：读取 SSH 密钥、.env、云凭证等文件前会先请求审批。
- **若干修复**：交互式审批一直等你回应，不再超时自动拒绝；新增 /exit；响应慢时提示「响应较慢」；私有插件市场按登录状态拉取，git 操作不再卡住界面；长时间连接失效后自动恢复；流式超时放宽到 300 秒；web_fetch 支持 Markdown 输出；嵌套目录里的技能可被发现；深色主题下表格边框可见；输入框最高 6 行；/model 切换后旧审批不再重复弹出；WebUI 新建对话后侧栏及时刷新。

### Issues

- [#213 [共创大赛][Bug] 读文件经常出BLOCKED，这究竟是什么意思？](https://atomgit.com/atomgit_atomcode/atomcode/issues/213)
- [#220 [共创大赛][Bug] 总是试图在未授权路径查询代码](https://atomgit.com/atomgit_atomcode/atomcode/issues/220)
- [#495 [共创大赛][Bug] websearch功能不能使用](https://atomgit.com/atomgit_atomcode/atomcode/issues/495)
- [#605 no reasoning removed)](https://atomgit.com/atomgit_atomcode/atomcode/issues/605)
- [#606 bug：长文本复制粘贴不全](https://atomgit.com/atomgit_atomcode/atomcode/issues/606)
- [#610 [共创大赛][Bug] skills只能识别到相对于skills/目录下一级目录下的SKILL.md, 无法识别二级目录下的SKILL.md](https://atomgit.com/atomgit_atomcode/atomcode/issues/610)
- [#614 [共创大赛][Bug] atomcode --help i18n 问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/614)
- [#615 [共创大赛][Bug] cancel之后会覆盖输入框中已有内容](https://atomgit.com/atomgit_atomcode/atomcode/issues/615)
- [#629 [Bug] Skill 翻译描述只支持中文，翻译 README 抛 504 Gateway Time-out](https://atomgit.com/atomgit_atomcode/atomcode/issues/629)
- [#637 [共创大赛][feat] skills hub 发布skill 需要支持 atomgit.com](https://atomgit.com/atomgit_atomcode/atomcode/issues/637)
- [#640 [共创大赛][feat] skill hub 默认排序调整，根据star](https://atomgit.com/atomgit_atomcode/atomcode/issues/640)
- [#641 TUI resume切换会话时界面闪动](https://atomgit.com/atomgit_atomcode/atomcode/issues/641)
- [#642 turn运行中时ESC和Ctrl+C无法取消或终止运行](https://atomgit.com/atomgit_atomcode/atomcode/issues/642)
- [#643 [共创大赛][Bug] WriteFile工具显示异常](https://atomgit.com/atomgit_atomcode/atomcode/issues/643)
- [#644 [共创大赛][Bug] v2引擎缺少工具并行调用时的显示](https://atomgit.com/atomgit_atomcode/atomcode/issues/644)
- [#645 [共创大赛][Bug] v2引擎不支持OpenFile工具](https://atomgit.com/atomgit_atomcode/atomcode/issues/645)
- [#646 [共创大赛][Bug] BYPASS模式下，WriteFile依旧弹出了审批](https://atomgit.com/atomgit_atomcode/atomcode/issues/646)
- [#647 [共创大赛][Bug] 审批确认信息中，SearchReplace工具和mcp调用工具的参数显示错误](https://atomgit.com/atomgit_atomcode/atomcode/issues/647)
- [#648 [共创大赛][Bug] OpenFile工具显示异常](https://atomgit.com/atomgit_atomcode/atomcode/issues/648)
- [#650 [共创大赛][feat] skill_hub 发布支持 test.gitcode.net](https://atomgit.com/atomgit_atomcode/atomcode/issues/650)
- [#651 [共创大赛][Bug] skill hub 定时更新，仓库的 star、download数据](https://atomgit.com/atomgit_atomcode/atomcode/issues/651)
- [#652 [共创大赛][feat] skill hub 控制台skill管理，翻译频率限制，避免用户一直点翻译](https://atomgit.com/atomgit_atomcode/atomcode/issues/652)
- [#653 [共创大赛][Bug] skill hub发布skill没有 home page 数据](https://atomgit.com/atomgit_atomcode/atomcode/issues/653)
- [#654 [共创大赛][Bug] 每次对话结束后下方的token显示异常](https://atomgit.com/atomgit_atomcode/atomcode/issues/654)
- [#655 [共创大赛][Bug] v2引擎无法识别图片，缺少VL预处理](https://atomgit.com/atomgit_atomcode/atomcode/issues/655)
- [#656 [共创大赛][Bug] 使用/model切换模型后，不应该出现黄色warning信息](https://atomgit.com/atomgit_atomcode/atomcode/issues/656)
- [#657 [共创大赛][Bug] /compact功能不可用](https://atomgit.com/atomgit_atomcode/atomcode/issues/657)
- [#658 [共创大赛][Bug] v2 下重试机制/UI 渲染变了](https://atomgit.com/atomgit_atomcode/atomcode/issues/658)
- [#659 [共创大赛][Bug] 切换模型后，询问agent当前模型，回答的仍是上一个的模型](https://atomgit.com/atomgit_atomcode/atomcode/issues/659)
- [#660 WebUI打开后TUI执行过程未同步展示，仅展示结果](https://atomgit.com/atomgit_atomcode/atomcode/issues/660)
- [#661 插件市场简介和README翻译后，英文语言下仍展示中文内容](https://atomgit.com/atomgit_atomcode/atomcode/issues/661)
- [#663 [共创大赛][Bug] EditFile工具参数显示异常，而且diff也不显示了](https://atomgit.com/atomgit_atomcode/atomcode/issues/663)
- [#664 engine v2 provider提示在每次turn完成后重复显示，应仅在provider实际变化时打印](https://atomgit.com/atomgit_atomcode/atomcode/issues/664)
- [#665 bug: /setup 后 bridge respawn 失败导致 atomcode 直接退出](https://atomgit.com/atomgit_atomcode/atomcode/issues/665)
- [#666 [共创大赛][Bug] 终端添加市场源，填写git配置失败，导致终端卡死](https://atomgit.com/atomgit_atomcode/atomcode/issues/666)
- [#667 [v2 engine] 缺少 VL 图片预处理逻辑，图片被直接发送给纯文本模型导致 400 错误](https://atomgit.com/atomgit_atomcode/atomcode/issues/667)
- [#668 Skill Hub 同一仓库地址可创建多个重复 Skill，应做后台限制](https://atomgit.com/atomgit_atomcode/atomcode/issues/668)
- [#670 [共创大赛][feat] skill hub 新增运维工具，支持手动同步，skill的 star和download数](https://atomgit.com/atomgit_atomcode/atomcode/issues/670)
- [#671 [共创大赛][feat] skillhub homepage 数据替换脚本](https://atomgit.com/atomgit_atomcode/atomcode/issues/671)
- [#672 [共创大赛][feat] skill hub 控制台菜单切换支持刷新恢复](https://atomgit.com/atomgit_atomcode/atomcode/issues/672)
- [#673 [共创大赛][Bug] TraceChain工具缺少了参数的显示](https://atomgit.com/atomgit_atomcode/atomcode/issues/673)
- [#674 Token数量展示不友好，大数值建议以K/M单位显示](https://atomgit.com/atomgit_atomcode/atomcode/issues/674)
- [#675 [共创大赛][Bug] v2引擎会导致开源构建直接崩溃，TUI启动后几秒自动退出](https://atomgit.com/atomgit_atomcode/atomcode/issues/675)
- [#677 [共创大赛][Bug] webui 开始使用一个无法使用的模型对话，然后再切换到一个可以用的模型对话，结果无法切换还是使用的上一个模型始终报错](https://atomgit.com/atomgit_atomcode/atomcode/issues/677)
- [#679 /goal help 命令的 Notes 以及 help 文案需要中文 i18n 支持](https://atomgit.com/atomgit_atomcode/atomcode/issues/679)
- [#680 图文一起发送后，文本未立即显示，跑完后才出现](https://atomgit.com/atomgit_atomcode/atomcode/issues/680)
- [#681 [共创大赛][Bug] webui 输出内容回答中，点击新建对话，此时TUI中内容还在输出上一个对话的内容。](https://atomgit.com/atomgit_atomcode/atomcode/issues/681)
- [#682 goal执行过程中Esc/Ctrl+C/goal clear均无法中断](https://atomgit.com/atomgit_atomcode/atomcode/issues/682)
- [#683 取消prompt A后切换模型，旧的pending工具调用重新触发执行](https://atomgit.com/atomgit_atomcode/atomcode/issues/683)
- [#684 [共创大赛][Bug] webui 首次对话完成后，第二次对话，TUI就没有任何输出](https://atomgit.com/atomgit_atomcode/atomcode/issues/684)
- [#685 goal执行时在状态栏常驻显示goal状态，/goal status信息被刷走看不到](https://atomgit.com/atomgit_atomcode/atomcode/issues/685)

## v4.25.1 (2026-06-12)

### 概览

这一版新增 /view 查看文件和微信渠道接入，默认网页搜索改用 Exa，并改进了大目录下的性能与终端渲染。

### 更新内容

- **/view 查看文件**：用 /view <文件路径> 在终端界面里直接查看代码文件。
- **微信渠道接入**：可以用个人微信 ClawBot 与 AtomCode 对话，插件用法见 AtomCode-Channel 仓库。
- **网页搜索默认接入 Exa**：WebSearch 默认使用 Exa 搜索，并保留 web_access 技能兜底。
- **性能与渲染优化**：超大目录下 @ 文件索引更快；修复 DevEco 终端中文整行渲染问题；加固 WebFetch 的 SSRF 防护。
- **若干修复**：以及其他问题修复。

### Issues

- [#243 [共创大赛][Bug] 经常读写某个文件报告没有权限，但是又用BASH命令读到了](https://atomgit.com/atomgit_atomcode/atomcode/issues/243)
- [#542 [共创大赛]-[Feature] 请求给win11增加任务完成后的系统通知](https://atomgit.com/atomgit_atomcode/atomcode/issues/542)
- [#570 [共创大赛][Bug] 为什么在VS Code里一直压缩上下文？在终端里没发现这种情况](https://atomgit.com/atomgit_atomcode/atomcode/issues/570)
- [#571 [共创大赛][Bug] webui tools的调用没有根据宽度来展示，后面会有很大一个空白](https://atomgit.com/atomgit_atomcode/atomcode/issues/571)
- [#595 @符号 新创建的文件@不出来](https://atomgit.com/atomgit_atomcode/atomcode/issues/595)
- [#598 [共创大赛] web_fetch SSRF防护存在TOCTOU竞态窗口和IPv4映射绕过](https://atomgit.com/atomgit_atomcode/atomcode/issues/598)
- [#604 [共创大赛]-[Feature] 弹出来的让人工选择项A,Y,N 能否给个默认值？然后让按Enter可以继续](https://atomgit.com/atomgit_atomcode/atomcode/issues/604)
- [#608 deepseek-v4-flash 模型频繁出现 (no reasoning recorded) 占位符，导致对话卡顿/中断](https://atomgit.com/atomgit_atomcode/atomcode/issues/608)
- [#609 [共创大赛][Bug] 经常提示(no reasoning detected)后中断执行](https://atomgit.com/atomgit_atomcode/atomcode/issues/609)
- [#611 [Bug] 用户昵称展示长度未限制，登录与修改资料后展示不一致](https://atomgit.com/atomgit_atomcode/atomcode/issues/611)
- [#616 [共创大赛][Bug] PLAN模型，直接去写代码了](https://atomgit.com/atomgit_atomcode/atomcode/issues/616)
- [#617 [共创大赛][Bug] 使用 MCP cpu占用率100%，任务完成后也持续占用](https://atomgit.com/atomgit_atomcode/atomcode/issues/617)
- [#618 [共创大赛][Bug] Always提权无效](https://atomgit.com/atomgit_atomcode/atomcode/issues/618)
- [#621 [Bug] 发布 skill 到 marketplace 时 source.path 指向了 SKILL.md 文件而非目录](https://atomgit.com/atomgit_atomcode/atomcode/issues/621)
- [#622 [Windows] webui切换工作目录带 `\\?\` 前缀导致会话分组 hash 不同步，WebUI 与 TUI 之间无法同步会话记录](https://atomgit.com/atomgit_atomcode/atomcode/issues/622)
- [#624 [Bug] 安装 wechat channel 插件后 slash 命令 /wechat 未加载](https://atomgit.com/atomgit_atomcode/atomcode/issues/624)
- [#625 [共创大赛][Bug] 大小窗口问题，小窗口渲染丢失了内容，大窗口展示的内容完整](https://atomgit.com/atomgit_atomcode/atomcode/issues/625)
- [#626 [Bug] WebUI 提交 prompt 后无任何返回，但 TUI 有正常返回](https://atomgit.com/atomgit_atomcode/atomcode/issues/626)
- [#627 [Feature] [共创大赛] TUI样式优化](https://atomgit.com/atomgit_atomcode/atomcode/issues/627)
- [#630 [Bug] /api/admin/skills 查询过滤结果不准确，包含不相关条目](https://atomgit.com/atomgit_atomcode/atomcode/issues/630)
- [#631 [Optimization] 用户昵称展示长度过长，UI 显示不协调](https://atomgit.com/atomgit_atomcode/atomcode/issues/631)

## v4.25.0 (2026-06-06)

### 概览

这一版新增 ! 直接执行命令和 DeepSeek V4 推理强度控制，简化了审批逻辑，并提升了 WebFetch 与 WebUI 的体验。

### 更新内容

- **! 直接执行命令**：输入 ! 加命令即可直接执行，比如 !git status，输出会进入模型的上下文。
- **DeepSeek V4 推理强度**：用 /effort 在 high 和 max 两档之间选择推理强度。
- **审批逻辑简化**：审批规则更简单，说明见官网的审批文档。
- **WebFetch 更快**：去掉 max_chars 限制，取回 HTML 后转换成 Markdown。
- **WebUI 增强**：在网页中切换工作目录、模型与推理强度更顺畅。
- **若干修复**：以及其他问题修复。

### Issues

- [#152 [共创大赛][Bug] 以Administrator运行缺少安全提示](https://atomgit.com/atomgit_atomcode/atomcode/issues/152)
- [#222 [共创大赛][Bug] 往符号连接写好像有问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/222)
- [#325 [共创大赛][Bug] window11下 powershell 终端中打开 atomcode 输入中文有乱码](https://atomgit.com/atomgit_atomcode/atomcode/issues/325)
- [#364 [共创大赛][Bug] AtomCode在WSL2 Ubuntu环境下不可用](https://atomgit.com/atomgit_atomcode/atomcode/issues/364)
- [#489 【Feature】MCP 工具调用审批弹窗需展示完整入参信息](https://atomgit.com/atomgit_atomcode/atomcode/issues/489)
- [#530 [共创大赛][Bug] 在执行bash等命令时，下面的输入框在不停地闪烁](https://atomgit.com/atomgit_atomcode/atomcode/issues/530)
- [#561 [Bug] Sync 模式下 TUI 和 WebUI 不在同一会话，三端历史互相独立、刷新后丢失 sync 对话](https://atomgit.com/atomgit_atomcode/atomcode/issues/561)
- [#572 [共创大赛][Bug] webui 信息流顺序和 TUI 不一致，不正确。](https://atomgit.com/atomgit_atomcode/atomcode/issues/572)
- [#573 [共创大赛][Bug] webui 模型选择下拉框，会溢出屏幕](https://atomgit.com/atomgit_atomcode/atomcode/issues/573)
- [#574 [共创大赛][Bug] 卸载插件后，终端会卡死无法输入](https://atomgit.com/atomgit_atomcode/atomcode/issues/574)
- [#575 [共创大赛][Bug] upgrade命令异常](https://atomgit.com/atomgit_atomcode/atomcode/issues/575)
- [#576 [共创大赛][Bug] /skills命令显示后，删掉下面会有文字残留](https://atomgit.com/atomgit_atomcode/atomcode/issues/576)
- [#581 [Feature] MCP 工具调用展示样式优化 + 审批提示样式优化](https://atomgit.com/atomgit_atomcode/atomcode/issues/581)
- [#583 [Bug] /webui --host 后终端输出的"访问地址"缺少 sync=1，且浏览器打开的 URL 丢失 token](https://atomgit.com/atomgit_atomcode/atomcode/issues/583)
- [#585 [Enhancement] WebUI 工具调用展示格式对齐 TUI：点号分隔 + key:value 参数](https://atomgit.com/atomgit_atomcode/atomcode/issues/585)
- [#588 atomecode打开窗口在会话中不能调整大小，只要改变大小，界面布局就会乱](https://atomgit.com/atomgit_atomcode/atomcode/issues/588)

## v4.24.2 (2026-06-03)

### 概览

这一版新增 /undo 回退对话，WebUI 与 TUI 同步更紧密，并修复了几处导致缓存命中率周期性下降的问题。

### 更新内容

- **/undo 回退对话**：把对话记忆退回到之前的某条提问，之后的内容不再进入上下文。
- **WebUI 与 TUI 同步**：两边切换模型实时互通；/webui 直接打开 TUI 当前会话；同步模式下 TUI 审批生效，同一回合的第二个工具不再被秒拒。
- **恢复会话更完整**：恢复对话时还原每轮之间的分隔线和 token、工具统计。
- **缓存更稳定**：会话级系统提示固定不变，Plan 模式提示和压缩摘要移出系统提示，旧的读文件结果不再变动，前缀缓存不再周期性失效。
- **图片路径识别**：手敲或在 Windows 上粘贴的本地图片路径，发送时会作为图片附件。
- **若干修复**：限流重试按网关给出的冷却时间等待；WebUI 实时输出不再串到其他会话、完成后不再残留审批卡片、默认端口改为 13457 避免与 VSCode 冲突；Windows 系统目录受敏感路径保护，Windows 路径与多行内容不再被误转义；显式指定 .atomcode、.claude 等目录时可以搜到文件；CodingPlan 不再误报模型列表漂移。

### Issues

- [#534 安装 skill/plugin 时未提示安装到用户全局位置还是项目目录位置](https://atomgit.com/atomgit_atomcode/atomcode/issues/534)
- [#563 [共创大赛][Bug] 恢复对话之后，模型最终输出和用户输入之间没有间隔，token统计和tool调用统计间隔丢失](https://atomgit.com/atomgit_atomcode/atomcode/issues/563)
- [#564 关于/undo的使用问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/564)
- [#565 修复 referral install_completed 客户端上报可靠性问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/565)
- [#568 [Bug] WebUI 请求 TUI 审批时，第二次审批自动 deny（response_rx 未按 call_id 区分）](https://atomgit.com/atomgit_atomcode/atomcode/issues/568)

## v4.24.1 (2026-06-03)

### 概览

这一版推出全新的 WebUI 本地界面，与终端共享同一会话实时互通；同时新增 $ 技能菜单，并简化了 /provider 添加流程。

### 更新内容

- **全新 WebUI 本地界面**：/webui 在进程内启动 Web 服务并自动打开浏览器，支持流式聊天、工具执行展示、权限审批、会话侧栏和切换工作目录；终端与网页使用同一个会话、实时互通，也支持远程访问。
- **$ 技能菜单**：在行首输入 $ 列出并过滤技能，Tab 补全，输入 $名称 加参数直接调用。
- **/provider 添加更简单**：自动识别粘贴的 curl、JSON、TOML 内容，优先填写 Base URL，类型自动预填。
- **Bash 增强**：支持在后台运行长任务（run_in_background），执行破坏性命令前给出拦截提示。
- **若干修复**：以及其他问题修复。

### Issues

- [#513 [共创大赛]-[Feature] 建议curl命令下载时不体现版本号](https://atomgit.com/atomgit_atomcode/atomcode/issues/513)
- [#523 【Feature】webui页面支持](https://atomgit.com/atomgit_atomcode/atomcode/issues/523)
- [#541 [共创大赛][Bug] 现在atomcode如何 设置不审批？回复没有 atomcode --dangerously-skip-permissions 或者 atomcode -y](https://atomgit.com/atomgit_atomcode/atomcode/issues/541)
- [#544 [共创大赛][Bug] skills hub 缺少 atomcode provider](https://atomgit.com/atomgit_atomcode/atomcode/issues/544)
- [#547 [共创大赛][Bug] [错误：auto-update: auto-update of marketplace atomcode-plugins-official failed: spawn git pull]](https://atomgit.com/atomgit_atomcode/atomcode/issues/547)
- [#548 [共创大赛][Bug] 鸿蒙系统启动报错 错误: auto-install: auto-install of marketplace https://atomgit.com/atomgit_atomcode/atomcode-plugins-official.git failed: clone https://atomgit.com/atomgit_atomcode/atomcode-plugin](https://atomgit.com/atomgit_atomcode/atomcode/issues/548)
- [#549 [Bug] webui 执行过程中输入框被禁用，用户无法键入](https://atomgit.com/atomgit_atomcode/atomcode/issues/549)
- [#551 [Enhancement] 编辑模型对话框增加 context_window 上下文大小配置](https://atomgit.com/atomgit_atomcode/atomcode/issues/551)
- [#552 [Bug] 编辑模型时，模型名称字段无法修改](https://atomgit.com/atomgit_atomcode/atomcode/issues/552)
- [#553 [Bug] 新建模型时名称重复直接覆盖原有配置，无提示](https://atomgit.com/atomgit_atomcode/atomcode/issues/553)
- [#555 [Bug] 移动端页面设置菜单部分被遮挡](https://atomgit.com/atomgit_atomcode/atomcode/issues/555)
- [#557 [Doc] 文档和 Ask 中增加虚拟组网（手机端访问 WebUI）相关指引](https://atomgit.com/atomgit_atomcode/atomcode/issues/557)
- [#558 [Bug] WebUI 上传文件只能选择当前目录层级下的文件](https://atomgit.com/atomgit_atomcode/atomcode/issues/558)
- [#559 [Bug] 虚拟组网后手机端 WebUI 会话消息需手动刷新才能同步（无实时推送）](https://atomgit.com/atomgit_atomcode/atomcode/issues/559)
- [#562 [共创大赛][Bug] bash调用playwright会阻塞主会话，直到用户主动按ESC取消](https://atomgit.com/atomgit_atomcode/atomcode/issues/562)

## v4.24.0 (2026-06-01)

### 概览

这一版重点是插件：新增交互式 /plugin 管理器和官方插件市场，同时加入跳过权限确认的 -y 选项，并大幅提高提示词缓存命中率。

### 更新内容

- **交互式插件管理器**：直接输入 /plugin 打开管理界面，浏览插件市场、一键安装或卸载，不必再记 name@marketplace；也可以只用插件名安装、卸载。
- **官方插件市场**：官方市场成为默认来源，启动时自动同步市场并安装新增插件；支持 git 子目录形式的插件来源。
- **跳过权限确认**：新增 --dangerously-skip-permissions（简写 -y），开启后底栏显示红色 BYPASS 标记。
- **缓存命中率提升**：系统提示词在不同次启动间保持一致，压缩不再无谓触发，缓存命中率从约 79% 提到约 96%。
- **支持 AGENTS.md**：项目里的 AGENTS.md 会被当作项目说明读取；新增 /guide 查询使用方法；新增可配置的 hooks（含 webhook）。
- **若干修复**：出错的轮次显示「已中断」而非成功横幅；流式输出时输入框不再闪动；粘贴含 Tab 缩进的文本光标不再错位；粘贴时去掉 shell 提示符；Windows 上不再引导模型写 bash 语法；鸿蒙系统自动升级取对应的安装包；CodingPlan 月度额度耗尽时提示正确；UserPromptSubmit hook 失败不再阻断对话。

### Issues

- [#26 [Feature] 面向编程小白的入门引导建议](https://atomgit.com/atomgit_atomcode/atomcode/issues/26)
- [#109 [Feature] 希望支持hook机制](https://atomgit.com/atomgit_atomcode/atomcode/issues/109)
- [#208 [共创大赛]-[Feature] 支持 AGENTS.md 标准](https://atomgit.com/atomgit_atomcode/atomcode/issues/208)
- [#286 插件包安装的skill在建议 superpower:brainstrorming 冒号前后都支持自动筛选并tab补全，方便快速定位到对应的skill](https://atomgit.com/atomgit_atomcode/atomcode/issues/286)
- [#326 [共创大赛]-[Feature] 从 Claude Code 已安装的插件中加载 skills/commands，避免重复安装](https://atomgit.com/atomgit_atomcode/atomcode/issues/326)
- [#440 [共创大赛][Bug] 已经取消的prompt，再下个prompt对话时仍然执行并返回了结果](https://atomgit.com/atomgit_atomcode/atomcode/issues/440)
- [#456 [共创大赛][Bug] 工具调用详细描述和resume提示线颜色与正文一样产生视觉混淆](https://atomgit.com/atomgit_atomcode/atomcode/issues/456)
- [#467 [共创大赛][Bug] skills name包含大写字母时抛错误提示](https://atomgit.com/atomgit_atomcode/atomcode/issues/467)
- [#468 [共创大赛][Bug] 已经安装的 plugin 提示存在问题，应该告诉用户已经安装了，如果需要重新安装使用什么命令先卸载，在重新执行什么命令安装](https://atomgit.com/atomgit_atomcode/atomcode/issues/468)
- [#471 [共创大赛][Bug] ParallelEditFiles 审批的时候，没有展示细节，不知道 ParallelEditFiles 需要干啥](https://atomgit.com/atomgit_atomcode/atomcode/issues/471)
- [#472 [共创大赛][Bug] ParallelEditFiles 是 AutoApprove，而 edit_file 是 RequireApprovalAlways](https://atomgit.com/atomgit_atomcode/atomcode/issues/472)
- [#478 [共创大赛]-[Feature] vscode插件能否增加选中文字添加到对话中这个功能点？](https://atomgit.com/atomgit_atomcode/atomcode/issues/478)
- [#501 [共创大赛][Bug] 鸿蒙版本每次自动升级后，都无法运行](https://atomgit.com/atomgit_atomcode/atomcode/issues/501)
- [#504 [共创大赛][Bug] 在windows上模型生成多行命令的情况都会调用失败，经历几次尝试最终都会变成使用临时文件传递参数或调用命令](https://atomgit.com/atomgit_atomcode/atomcode/issues/504)
- [#507 [共创大赛][Bug] 当◐ Running Grep… · 320.4s 时 esc 无法取消](https://atomgit.com/atomgit_atomcode/atomcode/issues/507)
- [#509 [共创大赛]-[Feature] 请求添加类似cc的--dangerously-skip-permissions, gemini cli的 --yolo 启动参数来让cli全自动默认允许通过审批任何权限相关工具/命令](https://atomgit.com/atomgit_atomcode/atomcode/issues/509)
- [#520 怎么总是会操作一些不属于当前项目，也不属于他自己的目录？](https://atomgit.com/atomgit_atomcode/atomcode/issues/520)
- [#521 [共创大赛]-[Feature] 增加atomcode官方插件市场和优化plugin命令](https://atomgit.com/atomgit_atomcode/atomcode/issues/521)
- [#522 [共创大赛][bug] marketplace 始终没有更新，启动时自动 git pull 同步 marketplace](https://atomgit.com/atomgit_atomcode/atomcode/issues/522)
- [#524 [共创大赛][Bug] /plugin 在 marketplace 安装成功，实际/skills 中没有](https://atomgit.com/atomgit_atomcode/atomcode/issues/524)
- [#525 [共创大赛][Bug] /plugin 已安装里面 -> 卸载插件 提示卸载成功了，但是 /skills 里面还能看到](https://atomgit.com/atomgit_atomcode/atomcode/issues/525)
- [#527 [共创大赛][Bug] /plugin 在 “浏览并安装” 里面选择插件回车安装，会卡住，此时没有任何交互，卡了很久安装成功了才有提示，优化一下交互，中间可能会去clone 仓库下来，会比较慢](https://atomgit.com/atomgit_atomcode/atomcode/issues/527)
- [#528 [共创大赛]-[Feature] 优化粘贴代码后光标的位置](https://atomgit.com/atomgit_atomcode/atomcode/issues/528)
- [#532 [共创大赛][Bug] 优化一个 /plugin install 安装问题， 目前按照 /plugin install control-ui-e2e@atomcode，后面还需要@对应 marketplace 这样很不方便，直接根据名字去安装即可。](https://atomgit.com/atomgit_atomcode/atomcode/issues/532)
- [#533 [共创大赛][Bug] /plugin uninstall control-ui-e2e 卸载一个不存在的 plugin 时，提示不够友好。](https://atomgit.com/atomgit_atomcode/atomcode/issues/533)
- [#535 [Enhancement] --dangerously-skip-permissions 提示文案优化：badge 红色+BYpass、banner 参考 CC 详细警告](https://atomgit.com/atomgit_atomcode/atomcode/issues/535)
- [#536 [共创大赛][feat] atomcode 官网新增 插件菜单和链接](https://atomgit.com/atomgit_atomcode/atomcode/issues/536)
- [#537 [共创大赛][feat] 文档更新 --dangerously-skip-permissions 和 -y 说明](https://atomgit.com/atomgit_atomcode/atomcode/issues/537)
- [#538 [Bug] atomcode 启动没有触发更新市场和插件](https://atomgit.com/atomgit_atomcode/atomcode/issues/538)

## v4.23.3 (2026-05-28)

### 概览

这是一个修复更新，重点改善 Windows 终端的显示与中文输出，以及思考型模型在长回合中的上下文占用。

### 更新内容

- **Windows 终端改进**：修复 PowerShell 7 等环境下中文字符重复和输入框闪烁；bash 输出的中文不再乱码。
- **长回合不再撑爆上下文**：压缩时按 token 量保留最近内容，思考内容很多的模型不会因此超出上下文窗口。
- **CodingPlan 用量提示**：本月额度用完时只显示一行"本月用量已耗尽，等 N 天后再使用"，/status 与 /login 的时长显示一致。
- **若干修复**：斜杠命令进入输入历史，可用上键找回；按 Ctrl+C 取消时网络搜索能立即停止；路径末尾多出的 {} 会自动去掉；1M 上下文窗口显示为 1m；Markdown 表格边框和宽度计算更准确；uninstall 时不再先触发自动更新。

### Issues

- [#476 [共创大赛][Bug] autocode 使用微信扫描登录后运行/codingplan 失败](https://atomgit.com/atomgit_atomcode/atomcode/issues/476)
- [#493 atomcode uninstall卸载触发更新](https://atomgit.com/atomgit_atomcode/atomcode/issues/493)
- [#500 【Feature】支持npm和brew安装](https://atomgit.com/atomgit_atomcode/atomcode/issues/500)

## v4.23.2 (2026-05-28)

### 概览

这一版主要改善终端界面：历史内容进入终端原生的回滚区，鼠标滚轮、选中和复制交还给终端；同时修复了大量 Windows 显示问题，破坏性命令始终需要审批。

### 更新内容

- **终端原生滚动与复制**：不再占用鼠标，可以直接用滚轮翻看历史、用终端自己的方式选中和复制。
- **Windows 显示修复**：修复中文环境下字符重复、错位和菜单导航卡顿等问题，宽字符和表情的宽度计算更准确。
- **破坏性操作始终审批**：rm -rf、强制推送等危险命令和修改 .env、密钥等敏感文件，不会因为本会话已允许而跳过审批。
- **npm 安装**：新增通过 npm 安装 AtomCode 的方式，支持鸿蒙系统。
- **启动更快**：插件与技能市场的初始化放到后台，输入框更早可用，装好的技能无需重启即可出现。
- **兼容更多模型**：识别 Qwen3 等模型以文本形式输出的工具调用，不再当作普通文字显示。
- **CodingPlan 用量更清楚**：/codingplan status 按时间窗口显示用量，本月额度用完时明确提示。
- **若干修复**：压缩后 /resume 的会话仍从原始问题开始；读取大文件更省内存；取消或超时时一并结束 bash 的子进程；Windows 路径中的 \t 等不再被误读成转义字符；/setup 完成后自动刷新技能，/plugin 新增 reload；表格中的代码和 \| 正确拆分；两个令牌都过期时不再启动崩溃。

### Issues

- [#424 为什么不能上划,进行到哪一步,就只能看当前这一屏幕的内容,上面的就看不到了](https://atomgit.com/atomgit_atomcode/atomcode/issues/424)
- [#451 [共创大赛][Bug] 使用setup生成的skill，无法正常修改](https://atomgit.com/atomgit_atomcode/atomcode/issues/451)
- [#460 会话历史只能查看到终端高度的内容，无法滚动查看到之前的内容](https://atomgit.com/atomgit_atomcode/atomcode/issues/460)
- [#461 工具执行结果中 `└` 树形连接符出现在折行文本行首，导致渲染异常](https://atomgit.com/atomgit_atomcode/atomcode/issues/461)
- [#462 [共创大赛][Bug] 使用 /setup 安装的skills，没有重新加载skills，导致安装的skills无法使用，也没有提示](https://atomgit.com/atomgit_atomcode/atomcode/issues/462)
- [#464 [共创大赛][Bug] /plugin 命令后面没有 reload 提示，和i18n处理](https://atomgit.com/atomgit_atomcode/atomcode/issues/464)
- [#466 [共创大赛][Bug] windows下 Waiting for approval 展示不正常，输入Y后上一个的 Waiting for approval 还存在](https://atomgit.com/atomgit_atomcode/atomcode/issues/466)
- [#474 【Feature】TUI改版](https://atomgit.com/atomgit_atomcode/atomcode/issues/474)
- [#480 【冒烟测试】Step 7 search_replace 步骤标题重复显示，审批后变为一个](https://atomgit.com/atomgit_atomcode/atomcode/issues/480)
- [#481 【冒烟测试】小窗口下 WriteFile 文字显示错位断裂](https://atomgit.com/atomgit_atomcode/atomcode/issues/481)
- [#485 【Bug】小窗口下执行 /help 指令，文案重叠](https://atomgit.com/atomgit_atomcode/atomcode/issues/485)
- [#486 Ctrl+O 详细模式展示存在重复行及 ANSI 序列泄漏问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/486)
- [#488 【Bug】并行 bash 调用计数与显示数量不一致：「Running N calls in parallel」N 值与实际列出的调用数不匹配](https://atomgit.com/atomgit_atomcode/atomcode/issues/488)

## v4.23.1 (2026-05-24)

### 概览

这是一个修复更新：审批提示更清楚可靠，读取工作区外文件需要确认，并修复 bash 偶发长时间卡住的问题。

### 更新内容

- **审批提示改进**：提示里显示替换详情，窄窗口下自动换行确保 Y/A/N 可见；确认后输入框不再消失；等待审批时 spinner 显示"等待审批"而不是一直计时。
- **更严的权限**：始终需要审批的操作不再被本会话的授权绕过；在工作区外搜索、诊断、列符号也需要确认。
- **修复 bash 卡死**：执行前后的工作区快照加了超时，不会再拖住命令数分钟。
- **ATOMCODE_HOME 语义统一**：设置 ATOMCODE_HOME 后直接用作配置目录，不再多加一层 .atomcode。
- **若干修复**：Tab 在输入框有文字时也能切换模式；令牌过期时自动刷新重试一次，静默刷新不再往输入框写字；扫码步骤按回车可在浏览器打开链接；无界面模式下思考内容不再逐字换行；~ 开头的路径正确解析；新增 Linux ARM64 自动升级。

### Issues

- [#351 [共创大赛][Bug] 从Jebtbrain IDEA中进入以后 随机乱码](https://atomgit.com/atomgit_atomcode/atomcode/issues/351)
- [#400 [共创大赛][Bug]atomcode的会话内容和iterm中git log日志内容混在一起](https://atomgit.com/atomgit_atomcode/atomcode/issues/400)
- [#446 [共创大赛][Bug] 等待审核 命令超长没有换行，导致审核指令看不到了](https://atomgit.com/atomgit_atomcode/atomcode/issues/446)
- [#448 【Bug】查询终端背景色超时污染了输入缓冲区](https://atomgit.com/atomgit_atomcode/atomcode/issues/448)
- [#450 [共创大赛][Bug] 工具调用的权限决定阶段出现语义丢失问题，RequireApproval覆盖了RequireApprovalAlways](https://atomgit.com/atomgit_atomcode/atomcode/issues/450)
- [#453 [共创大赛][Bug] 审批后输入框消失，resize 后恢复](https://atomgit.com/atomgit_atomcode/atomcode/issues/453)
- [#454 [共创大赛][Bug] 待审批一行就够，换行了](https://atomgit.com/atomgit_atomcode/atomcode/issues/454)
- [#455 [共创大赛][Bug] 待审批输入 Y 后，输入框没了](https://atomgit.com/atomgit_atomcode/atomcode/issues/455)
- [#457 【Bug】审批过后，之前等待审批的行占位还在](https://atomgit.com/atomgit_atomcode/atomcode/issues/457)
- [#458 【Bug】SearchReplace等待审批时，无法知道替换内容，审批过后才知道](https://atomgit.com/atomgit_atomcode/atomcode/issues/458)

## v4.23.0 (2026-05-21)

### 概览

这一版重做了首次启动：微信扫码登录、自动领取 CodingPlan，一页完成；同时新增 /setup 项目配置推荐、浅色/深色主题与代码块语法高亮。

### 更新内容

- **扫码快速上手**：首次启动直接显示二维码，手机微信扫码后自动检测完成、保存登录并领取 CodingPlan，随后进入界面。
- **/setup 推荐配置**：分析当前项目，推荐合适的技能、MCP、hooks 等并一键安装；也可在命令行运行 atomcode setup。
- **浅色/深色主题**：自动识别终端背景色切换配色，浅色下对比度更高；代码块按语言做语法高亮。
- **默认技能市场**：首次启动自动安装官方技能市场，升级后自动更新已安装的市场。
- **打开文件预览**：新增 open_file 工具，按系统用默认程序打开生成的网页、PDF、图片；在 SSH 或无界面环境下会明确说明打不开。
- **/keys 快捷键说明**：列出所有键盘快捷键，并标注哪些换行组合在你的终端里可能不可用；多行输入时上下键先在行间移动，再翻历史。
- **更安全的 git 命令**：强制推送、改写历史、交互式 rebase、强制切换或删除分支、跳过 hooks 等操作需要审批。
- **若干修复**：并行调用同名文件时审批提示能区分；已在本会话自动允许的工具不再重复弹审批；上游报错显示更清楚（429 简化为一行）；请求总超时放宽到 30 分钟；Ghostty 支持完成通知；VS Code 扩展支持多标签页会话、批量删除会话和工作区文件选择。

### Issues

- [#405 [共创大赛]-[Feature] 是否可以添加色彩支持，提升视觉交互体验](https://atomgit.com/atomgit_atomcode/atomcode/issues/405)
- [#423 【Feature】错误上报细分](https://atomgit.com/atomgit_atomcode/atomcode/issues/423)
- [#425 [共创大赛][Bug] 快速安装中的命令未按平台分别提供复制命令按钮](https://atomgit.com/atomgit_atomcode/atomcode/issues/425)
- [#426 [共创大赛]-[Feature] vs code插件中的会话列表，建议支持多选删除](https://atomgit.com/atomgit_atomcode/atomcode/issues/426)
- [#429 [共创大赛][Bug] ATOMCODE_HOME 设置后 Skills 运行异常 现象](https://atomgit.com/atomgit_atomcode/atomcode/issues/429)
- [#432 [共创大赛][Bug] API error (429 Too Many Requests)](https://atomgit.com/atomgit_atomcode/atomcode/issues/432)
- [#437 [共创大赛][Bug] 优化Tools并行调用展示问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/437)
- [#439 [共创大赛][Bug] tools并行调用，用户审批不知道是审批哪一项](https://atomgit.com/atomgit_atomcode/atomcode/issues/439)
- [#441 [共创大赛][Bug] 修复 /setup 安装后，没有刷新 skills](https://atomgit.com/atomgit_atomcode/atomcode/issues/441)
- [#443 [共创大赛][Bug] /setup 支持国际化i18n](https://atomgit.com/atomgit_atomcode/atomcode/issues/443)
- [#444 [共创大赛][feat] /setup 交互优化不再需要 /recommend 来触发](https://atomgit.com/atomgit_atomcode/atomcode/issues/444)
- [#445 [共创大赛][bug] 修复/setup 设置 ATOMCODE_HOME 无法运行问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/445)

## v4.22.3 (2026-05-18)

### 概览

这一版新增后台会话命令，官网全新升级，并修复了几处影响使用的问题。

### 更新内容

- **后台会话**：新增 /bg 系列命令，可以把会话放到后台运行、列出后台会话并恢复。
- **官网全新升级**：官网 atomcode.atomgit.com 全新改版。
- **若干修复**：修复 /codingplan 401 错误；修复 Windows 下 Ctrl+C 无法复制选中内容；修复 WebFetch 解析错误；修复 cd 自动切换目录的问题。

### Issues

- [#267 用户能成功要求atomcode删除系统skill目录下的文件，没做权限控制？](https://atomgit.com/atomgit_atomcode/atomcode/issues/267)
- [#376 [共创大赛][Bug] Linux下登录链接](https://atomgit.com/atomgit_atomcode/atomcode/issues/376)
- [#394 [共创大赛][Bug] 在全局路径:~/.atomcode/skills/下的SKILL在TUI下无法发现](https://atomgit.com/atomgit_atomcode/atomcode/issues/394)
- [#396 [共创大赛][Bug] 通过/plugin安装karpathy-skills无法使用](https://atomgit.com/atomgit_atomcode/atomcode/issues/396)
- [#403 [共创大赛][feat] 优化 /login 二维码太大，连接太长问题](https://atomgit.com/atomgit_atomcode/atomcode/issues/403)
- [#414 [共创大赛][Bug] /bg 命令，中途需要确认的会话，切后台后，重新恢复，没展示之前的prompt和输出内容](https://atomgit.com/atomgit_atomcode/atomcode/issues/414)
- [#415 [共创大赛][Bug] /bg bash命令申请情况，/bg 回来，内容为空](https://atomgit.com/atomgit_atomcode/atomcode/issues/415)
- [#417 [共创大赛]-[Feature] bg命令的说明内容需要支持中文语言下的中文内容](https://atomgit.com/atomgit_atomcode/atomcode/issues/417)
- [#421 【Feature】site站点风格和样式更新](https://atomgit.com/atomgit_atomcode/atomcode/issues/421)
