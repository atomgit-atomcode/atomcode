# 整体方案:统一装配、宿主收口、贡献可声明

状态: 方案(2026-09-14)。三份 ADR 的**执行计划**:
[`0017`](adr/0017-config-is-assembled-at-fold-time.md)(配置折树统一)、
[`0018`](adr/0018-host-contract.md)(Host 契约)、
[`0019`](adr/0019-contributions-are-declared-on-the-row.md)(贡献可声明)。

三条线**互相独立**,可以分别交付、分别回滚。下面每步都给**可执行的判据**。

## 一、目标(一句话)

配置在**一处**装配成终态;入口与主体分离,Host 薄且只有它认识全部;
UI 只认中立协议;凡是往共享集合里放东西的,**都要能被声明显影**。

## 二、现状(全部实测,非估计)

| | 数字 |
|---|---|
| `atomcode-harness` | 17,505 行 / 23 个测试文件 |
| `atomcode-tui` | 24,747 行 / 1 个测试文件(424 条判据) |
| `atomcode-cli` | 17,283 行,**完全不依赖 harness/tui** |
| Host 侧代码**住在 harness 里** | 3,562 行(`launch` 532、`bundle` 822、`ui` 498、`ui_web` 439、`ui_jsonrpc` 440、`model_source` 347、`profile` 204、`seam_map` 280) |
| `tui → harness` 的耦合 | 29 条 `use`、52 处引用、**15 个文件** |
| `ConfigTree::from_layers` 调用点 | 24 处 / 21 文件(生产 2、**测试 18**、tui 测试 1) |
| 贡献写入点 | 2 个 chokepoint + **4 个绕过** + 2 个 projections |
| `$ATOMCODE_HOME` 被读 | **3 次**(其中 2 次在 L1 `capabilities`) |
| `expand_env_vars` 实现 | **2 份**(`config/provider.rs:332`、`capabilities/mcp/config.rs:524`) |
| 入口 | **5 个**:`atomcode`、`atomcode-clix`、`atomcode-daemon`、`atui`、`harness` |
| 闸门 | `gates/tui*.sh`(5 个)、`differential.baseline` |
| CI | `build.yml`(macos/linux/windows/distro-pm)、`check.yml`(gate + lint) |

## 三、W3 贡献可声明(ADR 0019)——**先做,最小**

**为什么先做**:它验证「显式声明」这条路走得通,而且改动面最小、回滚最容易。

**改什么**

| 步骤 | 位置 | 量 |
|---|---|---|
| 3.1 `Plugin` 加 `contributes() -> &'static [&'static str]` | `plexus/src/plugin.rs` | +6 行 |
| 3.2 两个 chokepoint 记录(行 id + 项名) | `harness/plugins/tools.rs:31`(`mount`)、`:43`(`contribute_prompt`) | +4 行 |
| 3.3 projections 加第三个入口 | `harness/plugins/session.rs:146` | ~10 行 |
| 3.4 收回 4 个绕过点 | `self_knowledge.rs:367`、`recall.rs:326`、`capabilities.rs:307`、`tui/plugin.rs:1453` | 4 处 |
| 3.5 `--dump-seams` 多一列 `contributing` | `harness/src/seam_map.rs` + `launch.rs` 帮助 | 中 |
| 3.6 `--audit` 一致性:声明了没贡献 / 贡献了没声明 → 判红 | `harness/src/seam_map.rs` | 中 |
| 3.7 `adjust_layout` + `tui-layout` 提示词拆成一个行 | `tui/src/plugin.rs` → 新行 | 中 |

**判据**

- 3.6 的两个方向各有测试(用已有的 `tests/seam_convention.rs` 形状);
- `--dump-seams` 输出里 `tools` 的 `contributing` 列**至少列出 10 项**(今天列 0);
- 负反馈验证:故意删掉一处 `contributes` 声明 → `--audit` 判红并指名那一行。

**风险**:低。`contributes()` 有默认空实现,不改任何装载行为。

## 四、W1 配置折树统一(ADR 0017)

**改什么**

| 步骤 | 位置 | 量 |
|---|---|---|
| 1.1 `Expander` trait + `from_layers_with(layers, &dyn Expander)` | `plexus/src/loader.rs`(247 行) | +40 行 |
| 1.2 宿主注入环境展开器 | `launch.rs` / `profile.rs` | +20 行 |
| 1.3 两份 `expand_env_vars` 收敛成一份 | `config/provider.rs:332` + `capabilities/mcp/config.rs:524` | −1 份 |
| 1.4 `Secret` 值类型(`Serialize`/`Debug` 打码,`reveal()` 唯一出口) | 落点待定(§六) | 中 |
| 1.5 未定义变量 fail-fast | `loader.rs` | 小 |
| 1.6 `runtime_skill_dirs` 收 `atomcode_home` 参数(L1 断环境读) | `capabilities/skills/registry.rs:264`、`render.rs:98`、`paths.rs:18` + 4 个调用点 | 中 |
| 1.7 `skills` 行从「无 config + `row.home` 兜底」变成 `config = { home = "${HOME}" }` | `bundle.rs:114`、`plugins/capabilities.rs:90` | 小 |

**判据**

- 展开**幂等**:同一份 config 折两次、展开两次,结果相同;
- 未定义变量报错并**报出变量名**;`${VAR:-默认}` 不报错;
- `Secret` 不出现在任何打印:拿一棵含密钥的树跑 `--dump-config` 与 patch 报告,断言无明文、有 `<redacted>`;
- `from_layers`(不展开)与 `from_layers_with`(展开)各有判据——否则「测试里 `${HOME}` 成字面量」这种差异抓不住;
- **capabilities 对进程环境零读取**:沿用本轮那两条读源码守卫的形状,范围扩到 `atomcode-capabilities`。

**风险**:中。**主要成本在 18 个测试文件**都手搓配置树(`from_layers`),展开点若有行为变化它们会一起响。缓解:`from_layers` 保持不展开,测试默认不变。

## 五、W2 宿主收口(ADR 0018)——**最大的一步**

分两段,**不要合成一趟**。

### W2-L1 协议中立化(先做)

目标:`atomcode-tui` 不依赖 `atomcode-harness`。

**要中立化的东西**(按今天 tui 实际用到的分):

| 类别 | 体量 | 归属 |
|---|---|---|
| 26 个服务键 + 11 个 trait(`seams.rs` 603 行) | 大 | 中立协议 |
| 会话词汇 `SessionEvent` / `SessionLog` / `SessionHeader` / `InjectionOrigin`(`session.rs` 736 行) | 大 | 中立协议(UI 要读会话) |
| 事件(`events.rs` 321 行) | 中 | 中立协议 |
| `Agent` / `AgentStatus`(`agent.rs` 721 行) | 中 | 中立协议 |
| `plugins::handle::{spawn, Answers}`(1127 行) | 中 | **Host**(泵) |
| `profile::Profiles` / `launch` / `plugins`(目录) | 小 | **Host** |

落点在 §六(待定)。

**判据**

- `crates/atomcode-tui/Cargo.toml` 除 `[dev-dependencies]` 外**不出现 `atomcode-harness`**;
- 读 `Cargo.toml` 的守卫钉住这条(形状同 1.6 那两条读源码的);
- tui 的 424 条判据全绿(它们是这次的主要回归面)。

### W2-rest 搬迁 + 入口统一

| 步骤 | 内容 |
|---|---|
| 2.1 | `launch.rs` / `bundle.rs` / `ui*.rs` / `model_source.rs` / `profile.rs` / `seam_map.rs`(3,562 行)搬进宿主 crate |
| 2.2 | `TUI2` overlay → `bundle` 的 **`TUI_APP`** 常量(与 6 个既有 `*_APP` 同形),`ui_overlay` 加 `"tui" => "tui2"` 的必需映射 |
| 2.3 | `UI_NAMES` 加 `"tui"`;删 `--tui` 的两份拒绝(`launch.rs:120`、`atui.rs:150`) |
| 2.4 | 删三个糖 flag(`--repl`/`-i`/`--web`/`--sdk`) |
| 2.5 | 入口:`atomcode --tui`;`atui` 二进制消失;`harness` 二进制降级/删除 |
| 2.6 | `gates/tui.sh:80` 改用新入口;删 `atomcode-tui/Cargo.toml:9-10` 的 `[[bin]]` |
| 2.7 | 一条测试**反向**:`the_full_screen_front_end_is_not_a_row_here` 从「`--tui` 报 exit 2」改成「`--tui` 挂 `ui-tui2` 行」 |

**判据**

- `atomcode --tui` 与 `atomcode -p oneshot` 由**同一个二进制**接受;
- `--repl`/`--web`/`--sdk` 变成未知 flag(预期判红,是欠债闸门);
- `gates/tui.sh` 全过;`differential.baseline` 只降不升。

**风险**:高。3,562 行搬迁 + 入口改名会影响 `gates/tui.sh`、`docs/`、以及所有手工命令。
缓解:2.5 与 2.1 分开提交;入口改名单独一个 commit 便于回滚。

## 六、顺序与依赖

```
W3 贡献可声明 (最小,先验证)
 └─> W1 配置折树统一 (独立,有守卫可钉)
      └─> W2-L1 协议中立化 (断 tui→harness)
           └─> W2-rest 搬迁 + 入口统一 (最大,最后)
```

- W3 与 W1 之间**无依赖**,顺序可换(先 W3 是因为它小、能先验证方法);
- W1 与 W2 **无依赖**,但 W1 先做能让 W2 少一个变量(宿主读配置时已经是终态);
- W2-L1 必须在 W2-rest **之前**:搬迁的前提是 UI 侧已经不依赖 harness;
- 2.7 那条测试反向与 2.3/2.4 同一个 commit,否则中途 CI 会红。

## 七、需要你定的(三个)

1. **中立协议放哪**(W2-L1 的前提):
   - (a) **kernel 里加模块**(它已有 `agent`/`event`/`tool`/`message`,6,176 + 531 + 604 + 2,070 行);
   - (b) 新 crate。
   AGENTS.md 明写「不预设必须先创建 `atomcode-protocol`…否则复用现有 kernel/coding 中立类型」,所以我倾向 **(a)**;但 `SessionLog` 是 harness 的会话模型,搬进 kernel 会让 kernel 知道会话持久化的形状——**这一条要你点头**。

2. **范围**:`atomcode-clix` 与 `atomcode-cli` 里的**旧 tuix 栈**算不算这次「唯一入口」?
   今天 `atomcode` 就是旧栈在跑。若含,工作量与验证面完全不同(旧栈有差分基线要守)。

3. **`harness` 二进制**:删除,还是留成不带产品承诺的 dev 驱动?留着的唯一价值是不必为了
   看一眼 oneshot 而走产品入口。

## 八、不做(明确边界)

- **不造「容器收集贡献」机制**(0019 §5 已作废):参考实现 dsh 也是命令式注册;
- **不扫源码出图**:dsh 用 TypeScript 编译器 API,atomcode 的入口在运行时已经知道答案,
  更硬(0019 §4);
- **不预设 `atomcode-protocol` 新 crate**(AGENTS.md);
- **不改 `Op::Patch` 的整体替换语义**(`loader.rs:41-43` 明写 replace-never-deep-merge);
- **不动版本号、发布配置**(AGENTS.md)。

## 九、每步交付前必过的门

```sh
cargo nextest run -p <受影响 crate>        # 不用 cargo test(见 AGENTS.md)
cargo fmt --all -- --check                 # exit 0,CI 的 lint 唯一阻塞项
bash gates/tui.sh                          # 碰 tui 时
gates/differential.baseline 只降不升        # 碰 tui 时
```

**引用检查**:本轮文档里出现过 5 次 `file:line` 写错(87→90、launch→bundle、四个→五个、
seam_map 137→140、capabilities 87→90),每次都是靠人工核对抓到的。建议把「`docs/` 里的
`file:line` 逐条打开比对」做成一个小 gate——**这项要不要做,也请一并定**。
