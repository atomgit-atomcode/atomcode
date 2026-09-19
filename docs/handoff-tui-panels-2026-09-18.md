# 交接：tui 面板这一块（2026-09-18）

面板与命令的深度补齐从这里交给别人。这份文档是给**接手的人**看的：现状、
该从哪开始、哪些坑是我踩出来的、哪些线不能碰。

非 UI 的部分（契约、分层、6.3 迁移）不在这次交接范围内，见文末「留给我的」。

## 一、现状

- 分支 `feat/tui-replaces-tuix`，worktree 在 `.worktrees/tui-replaces-tuix`
- M1–M5 完成；M5.6 的 a / b / c / e 完成，d（B2）逐条有结论；**M6 全部没开始**
- 门：`gates/tui.sh`、`gates/layers.sh`、`gates/tui-test-count.baseline`（判据数棘轮）、
  `gates/harness-layer.baseline`（分层债棘轮，卡在 6，只降不升）

## 二、**先读这一段**：清单里前两遍的对照是错的

`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` 里有三轮对照，
**前两轮不可信**：它们问的是「这个能力有没有入口」，有就勾 ✅。

真实的例子：`/config` 按那把尺子算「做完了」，而 tuix 那边是
`crates/atomcode-tuix/src/modals/config_panel.rs` **565 行的可搜索半屏编辑器**——
恢复默认、文本项预填当前值让你改、写完不关可以连改、按模型的 retry 项，我这边
一样都没有。同一把错尺子量了一整轮命令表。

**第三轮（「按深度再对一遍」那节）才是能用的**：按对面的行数和那些行在干什么
逐条列，缺就写「无」。接手后如果要再对，照第三轮的方法。

## 三、按优先级，接手的人该做什么

### P0 —— 挡住「开箱可用」

| | 做什么 | 依据 |
|---|---|---|
| P0-2 | **首启登录引导**（`onboarding_wizard` 2336 行，tui 侧**零**） | 没有可用 provider 时新机器上开箱不可用，是 M6.1 自用的前提 |

### P1 —— 已经有入口但深度不够

按缺口大小排，每条的具体差异写在清单第三轮那张表里：

1. **`/config`**（对面 565 行）——四样具体缺口逐条写在清单的
   「`/config` 差的四样」小节，含每一样该动哪里。其中「恢复默认」**要动契约**
   （`host-api` 加 `ResetSetting`、`HostConfig` 加 `reset_setting`、cli 侧调
   `SettingSpec::reset`），「预填当前值」**要给 `Action` 加 `Compose(String)`**
   （今天只有 `Insert(char)`），这两样不是纯屏幕改动。
2. **`/provider`**（对面 3234 行）——列表+切换已有，**那 3234 行我没逐行核过**，
   接手第一件事是读完它再判断还差什么。注意下面第五节的红线。
3. **`/resume`**（对面 `session_picker` 2185 行）——没搜索、没删除、没预览
4. **`/view`**（对面 `file_viewer` 869 行，我这边约 60 行）——没搜索、没语法色
5. **`/model`**（对面 735 行）——没分组、没能力标注
6. **`/cd`**（对面 `dir_picker` 981 行）——没搜索、没书签

### P2 —— 前置已经打通（2026-09-18 晚补）

这两条原本写的是「被前置卡住」，现在**缝已经开好了**，面板可以直接动手：

- **`/usage`**（对面 1782 行）：契约有了 `HostCommand::Usage` / `HostReply::Usage`
  与 `UsageWindow`（名字、用完没有、什么时候回来、上限），宿主侧接到
  `RateLimitWindowSource`，tui 侧已有一条 `/usage` 命令把它说成话。**面板要做的
  是把这份数据画成 1782 行那种图**，数据不用再找。注意契约是 best-effort 的：
  取不到和不计额度都答空表，别把空表画成错误。
- **常驻 goal / loop 状态行**：`HostEvent::Autonomy` 是推送的，宿主每轮announce，
  `Moment::autonomy` 已经在存了（`plugin::took_autonomy`，带判据）。**面板要做的
  只剩画那一行**，`Moment` 里读得到，不用订阅任何流。它和 `/autonomy` 共用
  `Running` 这一个类型，所以行和命令不会说出两套话。
- **`/plugin` 市场面板**：判归 CLI，理由在清单 B2-5。判定可辩，想推翻就推翻，
  但要连着 `/upgrade` `/webui` 一起重判，别只翻这一条。

## 四、怎么验（0019 的规矩，不能绕）

每条能力要有**一行判据**，而且**新判据必须先摘掉被测代码证伪一次**才算数。
门只跑受影响的 crate，不要 `cargo test`、不要 `--workspace`：

```sh
cargo nextest run -p atomcode-tui        # cli 的包名是 `atomcode`，不是 atomcode-cli
cargo fmt --all -- --check
bash gates/tui.sh && bash gates/layers.sh
```

加判据后把 `gates/tui-test-count.baseline` 抬上去——注意它数的是 **tui 的**判据，
放在 coding 里的那条不算（我在这上面算错过一次）。

## 五、两条不能碰的线

1. **provider 条目带 `api_key`。** `/provider` 只做列出和切换；增删改**故意**留在
   配置文件里。`ProviderChoice.about` 是**逐字段拼**的，不是把结构体 `{:?}` 出来——
   改成序列化就等于把密钥打到屏幕和会话日志里。`/whoami` 同理只读名字和邮箱，
   **不读 token**。密码走 `secret.rs`，不进输入历史、不进 transcript。
2. **`atomcode-codingplan-crypto` 是闭源 crate，绝不能提交或推送**，`Cargo.lock`
   也不能带上它的依赖。真模型冒烟需要它时是临时拷贝，跑完还原。

## 六、我踩出来的坑，别再踩

- **按锚点做范围替换会连坐。** 我用锚点切 `/worktree` 的 match arm 时，把隔壁
  `/provider` 的整个分支一起切掉了，症状是 `/provider` 变成 `Quiet`；另一次把
  `RewindCatalog` 和它的 `#[derive]` 切散了。改 `commands.rs` 这种一个大 match 的
  文件，用唯一字符串替换，别用行号范围。
- **删一个入口会挪动别的判据的假设。** `/copy` 把 `/help` 挤出十行菜单窗口，
  菜单那条判据就红了。判据里凡是依赖「第几行」「窗口里有什么」的，改命令表时会中招。
- **python 脚本往 Rust 里写 `\uXXXX` 会写成字面量**，不是字符。
- 会话标题跨 `/new` 泄漏过一次（`switch_session` 没清 `moment.title`），是**已有的**
  判据抓到的——这类状态残留优先考虑写判据而不是肉眼查。

## 七、留给我的（非 UI，不在这次交接里）

1. **M6.3 迁移**：ACP / daemon / clix 迁到两份契约。改动面已量过（12 + 3 + 1 + 7 处），
   卡点只有 `cli/src/acp/commands.rs:21` 一行。**动手前要先拍板**
   `TurnCompletion::SnapshotUnavailable` 在契约里的去处——三条路写在
   `docs/tui-replaces-tuix-plan.md` 的 6.3 测绘里，tui 今天已经在丢这个信息。
2. **分层债三笔**：`atomcode-harness` 应当零 atomcode 依赖（现在 5 个，18 个文件
   引用 `capabilities`）；`atomcode-daemon` 该是 cli 的一个 `[[bin]]`；
   ~~`atomcode-auth` 该拆成两半~~ —— **2026-09-19 判定不做**：拆过一遍又退了
   （`6ecd9047`）。五个调用方拆完之后**全都仍然同时依赖两半**，一个都没变轻，
   每个反而多一条 manifest 边。等真出现一个只要存储那半的调用方再说；理由与
   证据写在 `docs/plans/2026-09-19-remaining-gaps.md` 的「A5 为什么退回去」。
3. ~~**两条契约缺口**~~ **已做**（2026-09-18 晚）：用量/额度、goal/loop 推送通道。
   见上面 P2。
4. **M6.5** 删不再挂载的代码：runtime 驱动协议、coding `team/`、
   capabilities `tools/task.rs` 的委派部分。先做可达性测绘再删。
