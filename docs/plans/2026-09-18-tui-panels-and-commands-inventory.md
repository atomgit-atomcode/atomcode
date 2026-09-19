# tui 面板与命令补齐：清单（待裁）

状态: 待审(2026-09-18)。承接 `docs/tui-replaces-tuix-plan.md` 的 M6.1——那一格原本只写
「自用到不想切回 tuix」,没有清单;这份文档就是那张清单,裁定后作为 **M5.6** 插在 M6.1 前面。

## 清单怎么来的

两半,顺序不能倒:

1. **能力面打底(不读 tuix)。** 规则:「产品能做的每件事都要能从屏幕上做到」。可数的来源:
   宿主控制契约 13 个命令变体、runtime 句柄 43 个公开方法、设置目录 14 个键、会话日志
   26 种事实。逐个问「屏幕上有没有入口 / 有没有画出来」。
2. **tuix 对照查漏。** ADR 0012 定的是「不读 tuix 定需求」;经本人明确解禁,**只用于查漏**
   ——找能力面推不出来的*交互形态*。解禁只限这一次清点,不作标尺:每条仍要独立回答
   「人为什么需要它」,答不上来的不做。

## 现状

**面板行 12**:welcome、transcript、status、live、tip、input、todo、steering、mascot、team、
raster、ask。
**命令 23(内建)**:quit/exit、clear、keys、mouse、transcript、showinject、tools、help、new、
resume、compact、context、undo、rewind、effort、reasoning、model、login、logout、mcp、
withdraw、reload、cancel-all。**加目录投影 5**:goal、loop、queue、policy、stop。

---

## P0 会卡住人的(核实后提上来的)

| # | 做什么 | 证据 |
|---|---|---|
| P0-1 | **接上 askpass**:工具里 `sudo` / `ssh` 要密码时,屏幕给一个不进输入历史的密码框 | 能力在 `capabilities/askpass/`(unix socket + 一次性 token + `Zeroizing`),bash 工具已给子进程设 `SUDO_ASKPASS`/`SSH_ASKPASS`;但 `askpass::server::start` **今天只有 tuix 调**(`tuix/lib.rs:784`),tui 一处引用都没有 ⇒ 现在会干等到超时 |
| P0-2 | **首启登录**:没有可用 provider 时,屏幕给 OAuth 二维码 + 跳过 | tuix 首启只弹这一屏;tui 没有任何引导 ⇒ 新机器上开箱不可用。是 M6.1 自用的前提 |

## A 能力面缺口("产品能做、屏幕做不到")

| # | 缺什么 | 依据 | 归哪一行 |
|---|---|---|---|
| A1 | 模式切换(plan / accept-edits / 默认) | 句柄 `set_mode`;设置键 `ui.mode_switch_key` 已存在却没入口 | 新命令行 + 状态栏徽标 |
| A2 | `/cd` 切工作目录 | 句柄 `change_directory` | 命令 + 目录选择 |
| A3 | `/config` 设置读写(**不是**把 `/patch` 请回来,见下方「边界」) | 设置目录 14 个键 | 命令 + 半屏面板 |
| A4 | 会话改名 | 事实 `Titled` | 命令;状态栏显示会话名 |
| A5 | 压缩痕迹上屏 | 事实 `Compacted`、`MessagesRewritten` 不折叠(只有一句 tip) | transcript 生产者 |
| A6 | 限速暂停上屏 | 事实 `RateLimitPaused` | transcript / live |
| A7 | 策略干预面板 | 事实 `PolicyIntervention`;现在只能靠 `/policy` 主动问 | ask 面板扩展 |
| A8 | 成员停止上屏 | 事实 `Stopped` | transcript(team 面板已有状态) |
| A9 | 工具结果被打桩 | 事实 `ToolResultsStubbed` | transcript |
| A10 | 轮次上限的继续/停止 | `round_cap_checkpoint` 已开,屏幕没有问法 | ask 面板扩展 |
| A11 | `/mcp tools <server>` | 句柄 `mcp_tools`;`/mcp` 只给状态 | 命令 |
| A12 | **列出可选模型**(契约缺口) | `ModelsSvc::list`(`harness/model_source.rs:92`)在 agent 树里,契约没有对应命令,屏幕读不到 ⇒ `/model` 只能打全名 | 先补契约,再给 `/model` 选择器 |
| A13 | **用量与额度**(契约缺口) | `RateLimitWindowSource`(`on_harness.rs:1312`)在 coding 树里,契约既无命令也不在 `Described`;`SessionEvent::Usage` 只有本会话 token | ✅ 2026-09-18 晚:`HostCommand::Usage` + `UsageWindow` + 运行时 `usage()`(3 秒上限、取不到答空表) + tui `/usage`。**面板另算** |

## B 对照查漏后要做的

### B1 由**能力行自己登记目录命令**(tui 侧零改动)

`review`、`memory` 已是 agent 树里的行(`on_harness.rs:242`、`:245`),但都没登记命令——
全仓今天只有两处 `commands::register`。正确做法是行自己登记(0021 §10),斜杠菜单投影目录
之后自动就有。

| # | 命令 | 拥有它的能力 |
|---|---|---|
| B1-1 | `/review` | `tool-code-review` 行 |
| B1-2 | `/memory`、`/remember`、`/forget` | `memory` 行 |
| B1-3 | `/skills` | skills 行 |
| B1-4 | `/init` | 生成 `AGENTS.md`(skill) |
| B1-5 | `/worklog` | 扫会话日志算日报,天然跨项目 |
| B1-6 | `/setup`、`/guide` | 都是展开一个 skill |

### B2 屏幕自己的活

进度标在第一列:✅ = 已做并有判据。

| # | 做什么 | 为什么 |
|---|---|---|
| B2-1 | `/diff` 两级浏览器(文件列表 → 详情) | 看 agent 改了什么,编码会话里最高频  ✅ d8fb3429(工作区快照出 numstat 与单文件 diff;`HostCommand::Changes` 一条命令两个深度) |
| B2-2 | 多会话:人自己开一条、切回来 | 按 0022 §6 + 0023 realm 做,**不照搬** tuix 的槽位号。**依赖**:计划页「后续」的「换会话不重建 App」——今天换会话仍重建 App(0022 §2 允许),所以这条先做「能开、能切回」,realm 化留给那条后续,别当成顺带做掉了 |
| B2-3 | `/model` 选择器(依赖 A12) | 现在要打全名 |
| B2-4 | `/provider` 管理面板 | 增改删 provider;现在只能改配置文件  ✅ 列表+切换已做(`/provider` → 挑一个就是 `/model <id>`,一个开关)。**增改删留给配置文件**:provider 条目带 `api_key`,让屏幕编辑那张表就是让屏幕碰凭据 |
| B2-5 | `/plugin` 市场面板 | 装/卸插件(CLI 已有,面板是体验)  ❌ **判归 CLI**:coding 没开 capabilities 的 `plugin` feature,为它开等于把市场/git 那套拉进 agent 进程,只为重复 CLI 已有的动作 —— 与 `/upgrade`、`/webui` 同一把尺子 |
| B2-6 | `/copy`、`/save`、`/view` | 复制代码块、存 markdown、看文件浮层  —— `/copy` `/save` ✅(c7f87718,自成一行 `tui-commands-take-away`);`/view` 未做  `/view` 亦 ✅ 722fb501 |
| B2-7 | `/language` | 设置键的一种(可并进 A3)  ✅ 下一个提交(是 `/config language` 的具名入口,同一段实现) |
| B2-8 | `/whoami`、`/worktree` | 当前登录用户;worktree 隔离  —— `/whoami` ✅(c7f87718,宿主契约 `WhoAmI` + `HostConfig::identity`);`/worktree` 未做 |
| B2-9 | `/sync` | 把当前终端会话共享给 webui/App——**本质依赖屏幕在场**。**依赖**:碰 daemon 的 live hub,与 M6.3(daemon / ACP / clix 迁到两份契约)同一片区域,排在 6.3 之后或同批做,否则适配要写两遍 |
| B2-10 | `/think on\|off` | 与 `/effort` 是两个旋钮:要不要思考 vs 思考多狠  ✅ c7f87718 |
| B2-11 | `/paste [路径]` | 兜 Windows 下 Ctrl+V 被按键层拦截、ohos 读不到剪贴板  ✅ 下一个提交 |
| B2-12 | 输入框上沿 rule | 会话名、历史位置、反向搜索指示  ✅ 722fb501 |
| B2-13 | goal / loop 状态行 | 自主循环在跑时的轮次与耗时  ⏳ 通道已通,只剩画:`/autonomy` 问得到(`HostCommand::Autonomy`),`HostEvent::Autonomy` 每轮推,`Moment::autonomy` 存着(`plugin::took_autonomy`,两条判据:同一轮不重画、别人的会话不上自己的行)。**那一行本身归面板那边** |
| B2-14 | @文件 / $skill 补全菜单 | tui 只有斜杠菜单  @文件 ✅ 下一个提交;**$skill 不做**——本前端每个可被人调用的 skill 已经是一条 `/` 命令(B1),再开一套 `$` 语法是第二个入口 |
| B2-15 | ghost 提示 | 空输入框里的下一步建议,右方向键接受  ✅ ghost 做完:来源是**本会话历史**(fish/zsh 那种),右方向键接受。不问模型、不发明建议 |
| B2-16 | 终端标题 | 会话名进窗口标题  ✅ 下一个提交(顺带:`SessionEvent::Titled` 之前 tui 里没人消费) |

## 按**深度**再对一遍（2026-09-18 晚）—— 前两遍的方法错了

前两遍对照问的都是「这个能力有没有入口」，有就标 ✅。这把尺子是错的：
`/config` 按它算「做完了」，而 tuix 那边是 `modals/config_panel.rs` **565 行的可搜索
半屏编辑器**，我这边只是「列出来」。用户当面指出了。

tuix 的富交互全在 `modals/`，**16,824 行**。逐个对：

| tuix modal | 行 | tui 这边 | 差在哪 |
|---|---|---|---|
| `provider_panel` | 3234 | `/provider` 列表+切换 | 增改删判了「归配置文件」（带 `api_key`），但 3234 行里不止 CRUD，**没逐行核过** |
| `onboarding_wizard` | 2336 | **无** | P0-2 首启登录引导，一直挂着 |
| `session_picker` | 2185 | `/resume` Picker（已补时间与目录） | 没搜索、没删除、没预览 |
| `plugin_manager` | 2102 | 判「归 CLI」 | 判定可辩，但确实没有 |
| `usage` + `usage_render` | 1782 | `/usage` 一条命令 | 缝已开（`UsageWindow`），**图没画** |
| `dir_picker` | 981 | `/cd` 已改成可浏览 | 没搜索、没书签 |
| `file_viewer` | 869 | `/view` 的 `Reading`，约 60 行 | 没搜索、没语法色 |
| `model_picker` | 735 | `/model` Picker | 没分组、没能力标注 |
| `config_panel` | 565 | `/config` 三级（项 → 值 → 写） | **仍差四样**，逐条见下 |
| `diff_viewer` | 539 | `/diff` 两级 | 接近 |
| `rewind` | 404 | `/rewind` Picker | 接近 |
| `password` | 294 | `secret.rs` | ✅ |
| `qr` | 249 | 无 | 属 `/app`，判归 CLI |
| `language_picker` | 155 | `/language` 已改成走 `/config` 的值选择器 | ✅ |
| `proxy_picker` | 103 | 无 | `/proxy`，写配置 |

### `/config` 差的四样（2026-09-18 逐行读完 565 行后列的）

「三级选择器」不等于「编辑器」。把对面的 key 处理与渲染读完，缺的是：

1. **恢复默认**——对面是 Delete 两次确认（`pending_reset`），调 `SettingSpec::reset`。
   我这边没有任何入口，契约里也没有 `ResetSetting`，宿主侧 `HostConfig` 也没有
   `reset_setting`。**这条要动契约，不只是屏幕。**
2. **文本/数字项预填当前值**——对面 Enter 进入行内编辑，把当前值填进去、首次按键
   整体替换（`replace_edit_value_on_input`）。我这边只说了一句「要 1–200」，人得
   从头敲 `/config coding.max_rounds 40`。tui 的对应物应该是**把命令连当前值填进
   composer**，即 `Action` 需要一个 `Compose(String)`（今天只有 `Insert(char)`）。
3. **写完不关**——对面写完面板还在，可以连改几项，只渲染一行 `✓ id = value`。
   我这边写完就关，改三项进三次。
4. **按模型的 retry 项**——`model.retry_max_attempts` 是随当前 provider 变的动态
   设置（`selection_retry_max_attempts` / `patch_selection_retry_max_attempts`），
   不在静态 `SETTINGS` 里。`cli/src/lib.rs` 的 `settings()` 只映射了静态目录，所以
   这一项在 tui 上根本不存在。

另外两样我这边**已经有**，不用重做：搜索（`Picker` 的过滤同时匹配 label 与 about，
所以「主题」「retry」都能命中）、生效时机（`applies` 已经显示在每行 about 上）。

**记下这个教训**：“有入口”不等于“接上了”。以后对照要看对面花了多少行、
那些行在干什么，而不是名字对上就勾。

## 命令表逐名对过一遍（2026-09-18 晚）

前面那轮对照是按「交互形态」看的，这一遍是**把两边的命令名字面 diff**。
tuix 60 个名字，tui 自己 37 个 + 能力行目录 11 个（`goal` `loop` `queue` `policy`
`review` `memory` `remember` `forget` `skills` `init` `worktree` …）。差集 22 个，逐个归属：

| 判定 | 名字 | 为什么 |
|---|---|---|
| **这一批做了** | `plan` `build` `auto` | 模式的三个叫法，转 `/mode`（一个实现） |
| | `status` | 一次说完：会话、模型、思考强度、在哪、在不在自己干 |
| | `cost` | 转 `/context`（同一个实现） |
| | `todo` `team` | 两个面板的折叠，转已有的 `ToggleFold` |
| **归 CLI**（与 `/upgrade` 同一把尺子） | `upgrade` `app` `desktop` `webui` `plugin` `schedule` | CLI 已有，或零屏幕依赖 |
| **排在 6.3 之后** | `sync` | 碰 daemon 的 live hub |
| **已经有了，只是名字不同** | `session` → `/clear`；`welcome` → 欢迎块是流里的一个块，往上滚就在；`usage` → A13（缺限速缝） | |
| **真缺，但要先开东西** | `bg` `background` | 后台会话槽位；要多会话同时跑，而今天换会话仍重建 App |
| | `proxy` `openrouter` | 都是写配置文件；`/config` 已经能改扁平设置项，provider 表则故意不给屏幕改（带 `api_key`） |

也就是说：**没有一条是忘了接**，剩下的要么已判归 CLI，要么卡在一个先决条件上。

## 核实后判定「不做 / 归 CLI / 低优先」

| 东西 | 核实到的事实 | 判定 |
|---|---|---|
| `/upgrade` | 自下载替换二进制;与 CLI `atomcode upgrade` / `rollback` 完全重复,唯一增量是进度条 | 不做 |
| `/app` | 除「终端里画二维码」外全是外部进程编排(下载 relay-client、spawn 子进程) | 归 CLI |
| `/desktop` | 纯路径探测 + 启动外部 app,零屏幕依赖 | 归 CLI |
| `/webui` 起服务那半 | CLI 已有 `atomcode webui`;斜杠版的 attach 半边并进 B2-9 | 归 CLI |
| `/think budget N` | `thinking_budget` 是遗留字段,v2 adaptive thinking 下**被丢弃、不影响请求** | 不做(做了是假的) |
| `/schedule` | 斜杠版只读展示;增删走 CLI;执行靠 OS 调度器(launchd / systemd / schtasks)回调 `atomcode schedule run <id>` | 低优先 |
| `/welcome` 重跑引导 | 首启那条已在 P0-2 | 低优先 |
| 会话选择器 | `/resume`、`/rewind` 无参**已经**是带说明的选择器(`overlay::Picker`) | 已有,划掉 |

## 边界:这份清单没有推翻过任何已定的事

对过计划页与 ADR,只有一处是真冲突(已改),其余三处是「名字像、事不同」,写在这里免得后人
照着往回删:

- **ADR 0022 §4「tui 功能分期」**——原文把撤销 / rewind / 恢复 / 重载 / 登出登录列为「不开放」,
  而这五项在 M5 已经落地并发出去了。**已在该 ADR 补「解禁」段**(2026-09-18),记下解禁条件
  何时满足;那一节从此只剩历史价值。
- **A3 `/config` ≠ M2.6 删掉的配置树命令。** 删掉的 `/rows`、`/patch`、`/audit`、`/tools-list`
  经 `ControlSvc` **直接改运行中的行**(能关掉审批、绕过 runtime 记账,0022 §7 说的就是这个)。
  A3 改的是**配置文件里的设置键**,走重载路径生效。不是把 `/patch` 请回来。
- **今天的 `/tools` 不是被删的那个。** 它是「展开或折叠工具调用结果」的显示开关;被删的是列
  配置树工具的 `/tools-list`。
- **B2-12〜B2-16(上沿 rule、状态行、补全菜单、ghost、终端标题)≠ 0022 §8 去掉的可调布局。**
  §8 去掉的是**让人和模型改布局**(`/layout`、`adjust_layout` 工具、ctrl-f / ctrl-z、布局操作
  日志)。这几条加的是**固定的行**,不带任何运行时改布局的入口。

另外两处是顺序依赖,不是冲突:B2-2 依赖「换会话不重建 App」那条后续,B2-9 依赖 M6.3。
还有一条:**M6.4「删 tuix」必须排在这份清单落地之后**——B2 有多条是照 tuix 的交互形态补的,
tuix 一删就没参照。M5.6 插在 M6.1 前面即自然满足。

## 落地规矩

- 每条一行(面板)或一条命令登记,遵 0019「贡献声明在行上」;不往 Host 里塞。
- 每条先写判据、摘掉被测代码证伪一次,再算完成。
- 分批提交,一批一个主题;`gates/tui-test-count.baseline` 随判据增长上抬。
- 建议顺序:**P0 → A → B1 → B2**。B1 最便宜(tui 零改动),可以和 A 并行。
