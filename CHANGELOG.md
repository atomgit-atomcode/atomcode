# AtomCode 更新日志

<!--
给编辑这份文件的人看的，这一段不会展示给用户。

- 每个版本一节，以 `## vX.Y.Z` 开头，后面可以跟日期：`## v5.2.2 (2026-10-08)`。
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

## v5.2.2

### 概览

这一版的核心是**架构与终端界面的重构**：运行时重写为分层结构，所有前端共用一个运行时，会话改为可完整恢复的事件记录；全新的全屏终端界面成为默认，设置、模型、MCP、插件、后台会话都收进了面板。

### 更新内容

- **全新架构**：运行时按「智能体循环 / 能力装配 / 编码运行时」分层重写，旧引擎整体退役；终端、网页、VS Code、ACP 共用同一个运行时，各端行为一致。会话改为逐条事件记录，撤销、回退、压缩都作为新记录追加，不改写历史，中断或崩溃后可以完整恢复。
- **全新终端界面成为默认**：直接运行 atomcode 即打开新版全屏界面，旧界面可用 --classic 或配置 [ui] screen 切回；支持鼠标点击、双击选词、三击选行、右键菜单，跨屏拖拽选中并自动滚动，链接和文件路径点击即可打开；终端标题和标签页显示会话名，并标出哪个会话在等你。
- **设置与管理面板化**：/config 打开设置面板（设置 / Config / Status / Usage / Stats），可查看额度、每日用量和各模型用量；/provider 管理账号与模型，保存后自动检测连通性；/mcp 面板逐个启用、停用 MCP 服务器；/toolbox 在会话中开关单个工具；/plugin 管理插件；/status 显示登录账号与订阅到期时间。
- **后台会话与团队协作**：/background（/bg）把当前会话放到后台继续跑，可分组查看、切换、取回；/review 默认在后台评审，不占用当前对话；后台会话的提问会出现在前台，可就地作答；团队面板列出主会话、成员和子代理的状态、耗时与上下文用量，成员会话会保存，重启后随主会话一起恢复。
- **会话更可靠**：/resume 可预览会话最后聊了什么，按两次 Ctrl+D 删除会话；项目文件夹改名后会话不再丢失；双击 Esc 打开回退面板。
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

## v5.1.0 (2026-09-18)

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

## v5.0.4 (2026-08-03)

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
