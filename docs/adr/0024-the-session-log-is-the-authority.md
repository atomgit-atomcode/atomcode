# 会话的权威是 harness 日志

状态: 已决定(2026-09-17)。**推翻** 2026-09-16 收双引擎时定的「原生快照是主、harness JSONL
是从」(`AGENTS.md` 第 39、42 行;`docs/handoff-collapse-dual-engine-2026-09-16.md`「三条不要
重议的决定」第 1 条)。承接 [`0014`](./0014-an-agent-owns-its-session-and-world.md) 与
[`0023`](./0023-a-session-is-a-realm.md)。

## 背景

**09-16 的决定与今天的实现。** 原生 `SessionManager` 是唯一的会话持久化模型;resume、撤销、
rewind、恢复、会话目录、租约、落盘失败即停只认它。harness 日志由 `session-journal` 行写到
`<home>/sessions/harness/<bucket>/`,`resume = false`,两边不读对方的文件当权威。

**为什么变。**

- 0023 定了「一个会话一个 realm」、subAgent 就是 Agent、成员日志要保留。原生存储没有子会话
  的概念,成员今天 `persist(false)`。
- 原生为主就意味着 resume 永远有损:每次从原生快照起一棵 agent,日志都由
  `seed_from_snapshot` 重建(`coding/host_rows.rs:210`),只剩 6 种事件、序号重起(0022)。
- harness 的持久化缝存的是事实,不是投影。`harness/seams.rs:323-326` 原文:「persistence that
  stored the projection instead of the facts could not reproduce a UI replay or a different
  compaction after the fact」。

**原生存储今天承担的东西。** `capabilities/src/session/` 共 14,756 行,全仓 441 处引用
(capabilities 149、daemon 167、coding 47、cli 28、tuix 26、clix 4、kernel 1):

| 能力 | 今天在哪 | harness 侧现状 |
|---|---|---|
| 会话内容与 resume | `<id>.snapshot`,`SnapshotHook`(`snapshot.rs`) | 有:事件 JSONL + header + resume(`harness/agent.rs:752-760`) |
| 会话目录:名字、列表、最近一个、改名、删除 | `manager.rs` 的 `scan_catalog` / `list_visible` / `latest` / `rename` / `delete` + `<id>.meta` | 部分:`SessionPersistence::list` / `describe`(标题取 `Titled` 事件);无改名、删除 |
| fork | `fork_native_session` | 有:`header.parent` + `inherited` |
| 租约(同一会话不被两个进程同时打开) | `acquire_lease` | 无 |
| 撤销、恢复快照的落盘事务 | `commit_native_runtime_mutation` + undo sidecar | 无;0022 / 0023 已定做成日志事件 |
| 逐回合统计 | `<id>.meta` 的 turn stats | 原料在 `Usage` 等事件里 |
| 跨会话 recall、`list_sessions`、`/worklog` | `transcript.rs` 的 `<id>.jsonl` + `recall.rs` + `session_list.rs` + `worklog.rs` | 无;原料就是事件日志 |
| Claude Code hooks 的 `transcript_path` | `transcript.rs` | 无 |
| todo sidecar、artifacts 目录 | `manager.rs` | todo 可从日志折出(tui 已用 `reduce_todos`) |
| UI 回放数据 | `presentation.rs`(tuix、daemon 用) | 会话事实流本身(0022) |
| 会话上下文块与 resume 时沿用的系统提示 | `context.rs`;`HostContext.stored`(`coding/on_harness.rs:1275`) | 无;需要一条日志事实承载 |
| 历史 core JSON 单向导入 | `daemon/legacy_convert.rs` | 无 |
| rewind 的代码检查点 | `rewind.rs`,独立 git 目录 | 不依赖快照 |

## 决策

1. **每个会话的唯一权威是它的 harness 事件日志**,经 `session-persistence` 缝落盘。lead、
   team 成员、`task` 子 agent 一视同仁:都是 Agent,都落盘;成员是带 `parent` 的会话(0023)。
2. **resume 就是重放日志。** 不再从原生快照重建;`seed_from_snapshot` 不再在 resume 路径上。
3. **原生存储承担的能力逐项搬到日志之上**(上表):形状是日志事件 + 行 / 缝。撤销、恢复做成
   日志事件(0022、0023)。
4. **原生快照不再是任何东西的权威,也不留作「从」。**
5. **不设从:`SessionManager` 改存事件日志。** 保留它那套机制——项目分桶、目录、租约、改名
   删除、fork、meta 索引、归属检查——只把会话内容从 `<id>.snapshot` 换成事件日志。一个会话
   一个存储、一种格式,不再有 `session-journal` 的独立目录。
   - 追加直接复用 `SessionManager::append_jsonl_line`(`capabilities/session/manager.rs:2962`):
     归属检查、排他文件锁、单行与总量上限、瞬时错误重试,错误上抛。今天只有 transcript 用它
     (`transcript.rs:214`);对比 `session-journal` 的追加是裸 `std::fs`、不加锁、失败只打印
     (`coding/session_journal.rs:167-190`、`:283-288`)。
   - 返回 `SessionSnapshot`(kernel 类型)的读接口保留,改成从事件投影
     (`derive_messages_with_meta`,`harness/session.rs:638-647`,本就是为「持久化消息并要
     统计的消费方」写的)。daemon / ACP 的读路径可以先不动。
   - recall、worklog 今天流式读 `<id>.jsonl`(`recall.rs:385`、`worklog.rs:103`),改读事件
     记录。
6. **事件词汇与投影挪到 kernel。** 依赖方向决定:harness 依赖 capabilities
   (`harness/Cargo.toml:19`),capabilities 只依赖 kernel,`SessionManager` 看不到 harness 的
   `SessionEvent`。挪的是 `SessionEvent` / `LoggedEvent` / `SessionHeader` 与
   `derive_messages*`。这同时回答 0022 未决的「事实词汇放哪」。
7. **流式片段(`AssistantChunk`)不落盘。** 持久化层跳过它;内存里的日志与实时事实流照旧带着。
   依据:喂模型的投影只读 `AssistantMessage`(`harness/session.rs` 的 `project`);完整回复收完
   后另有一条 `AssistantMessage`(`agent_loop.rs:617`);tui 没有片段时直接用
   `AssistantMessage` 的文本画(`tui/modules/transcript.rs:189-193`)。本机实测片段占 journal
   行数的 97%。磁盘上的序号因此有空洞:屏幕按 `seq > high` 去重、`restore` 取最大序号,都不要求
   连续。
8. **人取消时,半截输出合并成一条事实提交。** 流式到一半被人取消,这一步已经收到、还没有
   `AssistantMessage` 的文本与推理,合并成一条事实,在 `Interrupted` 之前提交
   (`agent_loop.rs:724-731` 的同一条件:`Cancelled` 且是人取消的)。它进内存日志,也落盘。
   没收到任何内容就不提交。harness 自己停回合(关闭进程)、进程崩溃时来不及写,接受丢失。
9. **半截输出进模型上下文。** 这一回合保留时(`keep_interrupted_context = true`,默认值,
   即 `Interrupted { undone: false }`),投影把它变成一条助手消息,放在已有的中断标记
   `Message::user_interruption()` 之前——模型看到「我刚才说到这里,被人打断了」。
   - 只带文本。半截推理不回传(没有签名的思考块有的 provider 不收);没收完的工具调用不回传
     (会变成没有结果的调用)。两者仍在日志里,屏幕照画。
   - 这一回合被撤回时(`undone: true`),半截输出与这一回合的其他工作一起离开投影。
   - 因为投影不读片段,resume 前后同一次请求的消息完全一致,前缀缓存不受片段不落盘影响。

10. **已发布版本的原生会话:读时转换。** 打开一个只有 `<id>.snapshot`、没有事件日志的会话时,
    用 `seed_from_snapshot` 转一次写成事件日志,之后按新格式读写。只对这些历史会话有损
    (只剩 6 种事件)。
11. **resume 带回团队。** resume lead 的会话时,`header.parent` 指向它、且没有被 `stop` 的
    team 成员一并重建:同样的 session id(`<lead>/<name>`)、各自的日志做种子、状态空闲、
    可以继续对话。已 `stop` 的成员和跑完的 `task` 子 agent 不重建,只能切过去看日志。

12. **租约保留,每个落盘的 Agent 一份。** lead 与每个团队成员各是一个会话文件,打开会话时
    `acquire_lease`(`manager.rs:1405-1425`,OS 排他锁、不等待),持有到它的 realm 被移除;
    每次追加前校验租约。追加时的文件锁只保证字节不穿插,挡不住两个进程各自追加出两段冲突的
    历史,所以租约不能省。撞上时沿用现在的行为:从最后提交的状态 fork 一个独立会话
    (`SessionBusyForked`)。

13. **成员的创建参数写进成员自己的会话头;stop 是成员日志里的一条事实。**
    - 今天 team 行只在内存里存 `role`、`agent`、`worktree`、`told`、`_driving`
      (`harness/plugins/team.rs:377-389`),模型、推理档、权限是 delegate 时按角色算的,没有
      留下;`stop` 直接移除 agent(`:882`),日志里没有痕迹。
    - 会话头(`SessionHeader`,`harness/session.rs:412-428`)加 `member` 字段:名字、角色 id、
      任务描述、实际选中的模型与推理档、worktree 目录与分支。会话头在会话开始时写一次,与
      「创建时定下、之后不变」相符;新字段 `serde(default)`,旧文件照读。
    - **权限与工具集 resume 时按当前的角色定义重算**,不照抄——与 lead resume 时用当前配置
      一致。
    - `stop` 先在成员自己的日志末尾提交一条「已停止」事实,再移除。resume 时扫出 `parent` 为
      lead 的会话,末尾是「已停止」的不重建。
14. **事件日志取代 transcript `<id>.jsonl`。**
    - 今天 transcript 每回合一条带回合 id 与时间戳的记录(`transcript.rs:40-63`),被
      recall、worklog、daemon 网页历史(`load_transcript_timestamps`)读,Claude Code hooks 的
      `transcript_path` 也指向它(`coding/parts.rs:986-987`)。
    - **每条落盘记录带提交时的墙钟时间**,加在记录外层(`LoggedEvent` 旁),不进事件本身;
      时钟注入。今天 `LoggedEvent` 只有 `seq` 与 `event`(`session.rs:444-447`)。差分 golden
      录的是 `AgentEvent` 的种类,不受影响。
    - **文件名 `<id>.events`**(不以 `.jsonl` 结尾,理由见第 16 条):已发布版本的目录里
      `<id>.jsonl` 是旧 transcript,格式不同,读时转换要分得清。转换时读旧 transcript 的时间戳给转出来的事件补上时间。
    - recall、worklog、网页历史改读事件日志;Claude Code hooks 的 `transcript_path` 指向事件
      日志(解析这个文件的外部 hook 脚本会看到新格式)。
15. **旧 `sessions/harness/` 里的 journal 丢弃,不导入。** `origin/main`、`release/v5.0.9`、
    `release/v5.1.0` 与最新发布 tag 都没有 `atomcode-harness` 这个 crate,`session_journal.rs`
    09-16 才加、只在 `feat/plexus-plugin-architecture` 上——没有真实用户的数据。它也不可信:
    journal 只追加,而日志事件里没有「撤销」,撤销过的会话导入会让被撤销的回合复活。迁移只有
    「原生快照读时转换」一条路。

16. **格式版本与回滚。**
    - **`SESSION_FORMAT_VERSION`(今天是 4,`harness/session.rs:392`)表示「读这个文件至少要几版」。**
      只加 `serde(default)` 字段(`member` 头字段、记录外层时间)不升;加新的事件种类或改变
      已有事件的含义要升——撤销事件、半截输出、「已停止」、「人对成员说」都属于这一类。
      事件按 `kind` 反序列化,不认识的种类今天会让整个文件读不出来。
    - **读到比自己新的版本**:目录里照样列出、标「需要更新版本」,拒绝 resume。今天是整个读取
      失败(`harness/plugins/session.rs:239`、`coding/session_journal.rs:121`)。
    - **回滚到旧版(updater 的 `.bak`,`updater/lib.rs:15-22`)不丢数据、不分叉,代价是旧版看不到
      新格式与已转换的会话:**
      - 新格式会话落在旧版扫描忽略的文件名上。旧版只认 `.snapshot`、`.jsonl`、
        `.rewind.txn.json`、`.rewind.json`、`.meta`、`.json`(`manager.rs:3240-3244`、
        `:3279-3297`),所以事件日志是 `<id>.events`、索引是 `<id>.index`,不用 `.meta`。
      - 读时转换成功后,这个会话旧版认得的文件(`.snapshot`、`.meta`、`.jsonl`、`.ui.json`、
        rewind 账本)一律加 `.migrated` 后缀挪开,不删。`.ui.json` 必须一起挪:同名的
        `.meta` / `.snapshot` / `.jsonl` 都不在时,旧版会把它当旧 core 会话(`:3281-3293`)。
      - 不挪的后果:回滚后旧版接着用那份过时的快照继续写,再升级回来新版只认事件日志,旧版期间
        的对话丢失。

17. **撤销、rewind、恢复快照:一种事实 `Rewound { to, scope }`。**
    - 今天撤销按「第 N 个 prompt」截断快照并把 prompt 放回输入框(`coding/runtime.rs:1642-1672`);
      rewind 按回合号、分对话 / 代码 / 两者,代码靠独立 git 目录的检查点;投影今天以最后一个
      `Compacted { through }` 为界(`harness/session.rs:653-704`),撤回的 `Interrupted` 回合整回合排除、
      记忆与压缩摘要这类会话级注入保留;todo 清单是对 todo 调用的折叠(`capabilities/tools/todo.rs:308`)。
    - **事实**:`Rewound { to, scope }` 追加到日志末尾,序号照常递增;`to` 是目标回合 `TurnStart`
      的序号,意思是「从这里往后撤掉」。
    - **投影**:遇到序号为 r 的 `Rewound { to }`,序号在 [to, r) 的事件全部排除;多次撤销按先后叠加。
    - **与压缩无需特判**:压缩事件自己的序号总大于它的 `through`。它落在被撤区间里就一起失效,投影
      退回上一个压缩边界、被压掉的原始事件重新露出,上下文变大由正常的自动压缩接手;它在区间之前
      就只覆盖保留下来的事件,照常生效。`ToolResultsStubbed` 同理。
    - **被撤区间里的会话级事实保留**:记忆注入、标题、`Usage`(token 已经花出去,统计与计费照算)。
    - **todo 清单跟投影走**,撤销后回到撤销点的状态。
    - **目标用回合号**,加 0021 第 9 条的 `based_on`;回合进行中回 `Busy`;不支持重做。
    - **代码范围**:先用 git 检查点还原工作区,再提交 `Rewound`;做检查点时提交
      `Checkpointed { turn, id }` 记下对应。
    - **屏幕**:流不可逆,被撤的块不删,只把呈现改成已撤销(变暗并折叠,0004 允许改呈现),加一个
      「已撤销到回合 N」的标记块;被撤的 prompt 放回输入框。
    - **前缀缓存**:撤销后下一次请求是撤销点之前那段的前缀,能命中之前的缓存。

## 迁移要改的写路径

- `SnapshotHook` 每回合整份写快照(`snapshot.rs`):删。
- 撤销的 `commit_native_runtime_mutation` 与 undo sidecar:改成 `Rewound` 事实(第 17 条)。
- daemon 的导入与恢复写入(`legacy_convert.rs` 等):改成写事件。
- presentation:从事件投影,或删。
- rewind 的代码检查点(`rewind.rs`,独立 git 目录)存的是工作区文件,不是会话内容:保留,
  日志里记检查点与回合的对应。
- `AGENTS.md` 第 39、42 行在迁移落地之前继续描述现状,旁边加注指向本 ADR。

## 对前面几份 ADR 的影响

- **0022**:背景里「重建 App 后日志从原生快照重建、有损」是今天的事实,本决定落地后不再
  成立;未决「resume 画面有损」随之取消。
- **0023**:未决「保留范围」定为**落盘**——成员会话持久化、带 `parent`,重启后仍可看。
  resume lead 时带回没被 stop 的团队成员(第 11 条)。
- **收双引擎的三条决定**:第 1 条被本 ADR 推翻;第 2 条(翻默认与删链分两个 commit)、第 3 条
  (差分 golden 不可重录)不受影响。

## 未决

- ~~**已有的原生会话**~~:定为第 10 条,读时转换。
- ~~**过渡方式**~~:定为第 5 条,不设从。
- ~~**流式片段落不落盘**~~:定为第 7、8、9 条。
- ~~**租约**~~:定为第 12 条。
- ~~**resume lead 时是否带回团队成员**~~:定为第 11 条,带回。
- ~~**团队成员的创建参数怎么留在日志里**~~:定为第 13 条。
- ~~**事件文件与现有 transcript `<id>.jsonl` 的关系**~~:定为第 14 条,取代。
- ~~**旧的 `sessions/harness/` 目录里已写的 journal**~~:定为第 15 条,丢弃。
- ~~**`SESSION_FORMAT_VERSION` 怎么演进**~~:定为第 16 条。

## 闸门

- **只有一个权威**:resume 一个会话只读它的 harness 日志——删掉原生快照文件,resume 结果
  不变;删掉日志,resume 失败,而不是回退到快照。
- **resume 无损**:一个含提问与回答、Notice、压缩、标题的会话,resume 后的日志事件**除流式片段外**
  逐条相同(种类、序号)。今天是红的(`seed_from_snapshot` 只产 6 种)。
- **片段不落盘**:流式回复落盘后,磁盘上没有 `AssistantChunk`,有完整的 `AssistantMessage`。
- **半截输出进上下文**:流式到一半人取消 → 日志里在 `Interrupted` 前有一条半截输出的事实;下一次
  请求里有这段文本的助手消息,后面跟中断标记,不含半截推理与没收完的工具调用;resume 之后同一次
  请求的消息与 resume 前逐条相同。`keep_interrupted_context = false` 时这段文本不在请求里。
- **旧会话读时转换**:一个只有 `.snapshot` 的会话能 resume;resume 之后磁盘上有了事件日志,
  再次 resume 不再读 `.snapshot`(删掉它结果不变)。
- **resume 带回团队**:lead 起了两个 team 成员、`stop` 掉其中一个,进程重启后 resume lead →
  注册表里有 lead 和没被 stop 的那个成员,session id 与重启前相同,给它发消息能起回合;被 stop
  的那个不在注册表里,但按 session id 能读出日志。
- **一个会话只有一个写者**:进程 A 打开会话(含团队成员的会话)后,进程 B 打开同一个会话拿到
  `SessionInUse`,`--continue` 落到 fork;没持有租约的追加被拒。A 退出(含被杀)后 B 能打开。
- **成员参数可恢复**:resume 后带回的成员,模型、推理档、worktree 与重启前一致;权限按当前角色
  定义。被 stop 的成员日志末尾有「已停止」。
- **记录带时间**:每条落盘记录有提交时间;worklog 能按天汇总新格式的会话;读时转换出的旧会话
  事件带上了旧 transcript 的时间。
- **旧 journal 不被读**:`sessions/harness/` 下有文件与没有文件,resume 与目录结果相同。
- **新版本会话被拒而不拖垮目录**:头版本高于本版本的会话在目录里列出并标「需要更新版本」,
  resume 被拒;同目录的其他会话照常列出与 resume。
- **回滚不分叉**:在含旧格式会话的目录上用新版 resume 一次、再新建一个会话之后,按旧版扫描的
  后缀规则匹配不到这两个会话的任何文件;挪开的旧文件都还在(`.migrated`)。
- **撤销是投影的事**:撤销到回合 N 之后,下一次请求的消息与「日志里只有回合 N 之前的事件」时逐条
  相同;日志文件里被撤的事件还在。撤销点之后曾经发生过压缩时同样成立。**今天红。**
- **撤销后 todo 回退**:回合 N 之后改过的 todo 清单,撤销到 N 后回到 N 之前的状态。
- **成员落盘**:team 成员与 `task` 子 agent 的会话,进程重启后按 session id 读得回,
  `header.parent` 指向 lead。今天是红的(`persist(false)`)。
- 每条落地时摘掉被测代码证伪一次。

## 落地时的补充(2026-09-17,M3 执行中)

- **追加不直接复用 `append_jsonl_line`**(改第 5 条的做法,不改保证):事件会话用
  `SessionManager::append_events`,保留同样的保证——追加前校验租约、排他文件锁一次写入、单行与总量上限、
  瞬时错误重试、错误上抛——另外跳过流式片段。`append_jsonl_line` 是 transcript 钩子的入口,钩子每回合
  仍在跑,对事件会话它什么都不写:否则回合记录会混进日志。
- **会话头加 `context`**:会话开始时系统提示里的环境块(工作目录、项目指令、git 快照)。resume 用它,
  请求前缀与中断前一致,不从已经变了的仓库重新渲染。`serde(default)`,按第 16 条不升版本。读时转换的
  旧会话取快照开头的非合成系统消息。
- **`Rewound` 提前到 M3**(原在计划 5.3):日志成为权威之后,runtime 的撤销 / rewind / 恢复如果不落成
  事实,只改快照就等于没做——现有前端(tuix)的撤销会直接失效。runtime 算出的目标对话若是日志某回合
  之前的样子,追加一条 `Rewound { to }`;若是日志从未有过的对话(`restore_snapshot` 可以给任意快照),
  整段撤回(`to` = 第一个回合)再把目标对话按 `events_from_snapshot` 提交,回合号接在已用过的之后。
  第 17 条的会话级事实保留规则照旧适用。屏幕呈现与 `Checkpointed` 仍在 M5。
- **只追加的唯一例外:撤销自己刚追加、没人读过的那段。** runtime 追加了撤销事实、随后重建 agent 失败
  时,按追加前记下的长度把日志截回(`truncate_events`,持租约)。此时旧 agent 已停、新 agent 没建起来,
  没有任何读者见过那几行;这是那次撤销自己的事务回滚,不是改写历史。
- **还没发布的新会话**:runtime 先在内存里备好会话,整棵树装配成功后才落盘(已有行为)。这段时间里建出的
  agent 不写任何东西(`begin` 什么都不做),头与索引在发布时一次写下。

## 失效条件

- 某项原生能力(租约、撤销的落盘事务)在追加式日志上做不出同等保证:单独立 ADR 补日志模型,
  不回退到「快照为主」。
