# 配置统一在折树时装配:变量展开 + 敏感值类型

状态: 提议(2026-09-14),尚未实现。承接 [`0013`](./0013-agent-product-host-ui.md)
(Host 读配置)。与「下一步」第 10/11 条同源,但独立成页,因为它是底座形状的决定,
不只是 harness 一处的搬迁。

## 背景

今天一个 `apply(ctx, config)` 拿到的 config **不是终态**。同一份"这台机器上模型和
路径是什么"的信息,在进程里有好几个读者、好几套规则:

| 事实 | 谁在算 | 在哪 |
|---|---|---|
| 模型端点与密钥 | 两条 `llm*` 行各读一遍环境 | `plugins/llm.rs` |
| 模型选择 | `llm-atomcode-config` 行读 `config.toml` + `resolve_model` | `plugins/llm.rs` |
| 密钥回落 | `resolve_account_api_key` 按 `OPENAI_API_KEY` 等裸名回落 | `config/provider.rs` |
| `$HOME` | skills 行 | `plugins/capabilities.rs:90`(`row.home` 的 `.or_else`) |
| `$ATOMCODE_HOME` | skills 目录(**三次**) | `capabilities/skills/registry.rs:264`、`skills/render.rs:98`、`paths.rs:18` |
| `$ATOMCODE_HOME` | config 目录 | `config/mod.rs:2170` |

`${VAR}` 展开**已经存在**,而且**存在两份**:`config/provider.rs:332` 与
`capabilities/mcp/config.rs:524`(同名函数、各自带测试、各自支持 `$VAR` /
`${VAR}` / `${VAR:-默认}`)。两份都在**读点逐个字段**展开,不是一趟遍历。

后果不是文案问题:

- **同一个网关能被两套规则解析**(同一个进程里),`--env-model` 与 config 来源的
  回落不一致;
- **缺变量时报出的清单取决于哪一行先报**;
- **一个事实有多个真源**,改一处不会让另一处跟着变(`render.rs:98` 的注释自己写着
  "the two must stay in step"——靠人记);
- **L1 在读进程环境**(`capabilities`),按 0013 那是 Host 的事。

目标(用户原话):**"从配置文件和环境变量装配出统一的 config"**,而且"装配时拿到的
值就已经是变量展开之后的值了"。也就是:变量展开、配置文件读取、patch 合并三者都在
底层做完,`apply` 只读自己那份 config。

## 决策

### 1. 展开是折树时的一趟遍历,不是读点的逐字段行为

插入点只有一个候选:`ConfigTree::from_layers`(`plexus/src/loader.rs:166`)。它是
漏斗——`Profiles::resolve`(`harness/src/profile.rs:134`)走它,**手搓配置树的测试也
走它**(约 20 个测试文件直接 `from_layers`)。放在 `Profiles::resolve` 会漏掉后者,
而失败形态不是报错,是**测试里的 `${HOME}` 成了字面量**,极难发现。

**顺序:先合并所有层,后展开一次。** 于是 patch 可以覆盖一个含占位符的值,而展开只
发生一次(不会被上游层的展开结果再展开一遍)。

**语法沿用既有的**,不发明:`$VAR` / `${VAR}` / `${VAR:-默认}`。两份现有实现收敛成
一份,放进 plexus 或一个独立的叶子 crate。

### 2. plexus 提供那一趟遍历,展开器由宿主注入

plexus 是通用容器,不该知道"进程环境"是什么(那是宿主关注点,0013)。所以:

```rust
// plexus
pub trait Expander: Send + Sync {
    fn expand(&self, raw: &str) -> Result<String, PlexusError>;
}

impl ConfigTree {
    pub fn from_layers_with(
        layers: impl IntoIterator<Item = Layer>,
        expand: &dyn Expander,
    ) -> Result<Self>;
    // from_layers(layers) == from_layers_with(layers, &Identity)  ← 不破坏现有调用
}
```

宿主(`launch.rs`)注入环境展开器。于是"变量是什么"由宿主决定,而"什么时候展开"
由容器保证——两边各拿自己该拿的那半。`from_layers` 保持存在(等价于不做展开),
手搓树的测试不被强迫引入环境。

### 3. 未定义变量:启动即失败

`${VAR}` 无默认值且变量未定义/为空 ⇒ **`from_layers` 报错**,树挂不上。理由是这个
仓库已经这么做事(角色 frontmatter 写错让 mount 失败,不是静默跳过),而静默展开成
空串的失败形态特别坏:一个 `base_url = "${TYPO}"` 会变成 `base_url = ""`,报出来的是
"连不上",不是"变量名拼错"。

要兜底就写 `${VAR:-默认}`——这是显式意图,与"我忘了设"能区分开。

**这条对测试有直接影响**:任何 `config` 里带 `$` 的字符串会被当作占位符。既有配置
里若有字面量 `$`,要么转义(`$$`),要么改用 `from_layers`(不展开)。迁移时要先扫一遍。

### 4. 敏感值是一个类型,不是一条例外通道

密钥**不需要**被排除在统一装配之外——它需要一个**值类型**:

```rust
pub struct Secret(String);
impl fmt::Debug for Secret { /* "<redacted>" */ }
impl fmt::Display for Secret { /* "<redacted>" */ }
impl Serialize for Secret { /* "<redacted>" */ }   // 关键
impl<'de> Deserialize<'de> for Secret { /* 正常读入 */ }
impl Secret { pub fn reveal(&self) -> &str; }
```

`Serialize` 打码是关键的一条:树里**所有打印路径都走 Value 的序列化**——
`--dump-config`(`app.rs` 的 dump)、`control.rs::row_label`(patch 后报"哪行变了")、
日志、`describe_self`。把打码做在类型上,**新加的打印点自动安全**,不必维护一份
"哪些字段敏感"的名单。反过来(按字段名叫 `api_key` 就在打印时抹掉)是位置式的,
漏一个打印点就泄一次。

于是密钥和非密钥走同一条路:展开进树、以 `Secret` 落进 config、`apply` 里是真值
(类型不同而已)。`model_source::DEFAULT_API_KEY_ENV` 那种"config 里放变量名"的做法
随之不再必要。

### 5. 合并语义不变

`Op::Patch` 仍是**整体替换该行的 config,不做深合并**(`loader.rs:41-43`:replace,
never deep-merge)。这是独立的一条决定,本页不改它。展开发生在合并**之后**,所以
与合并语义正交。

## 完成后 0013 的边界才真正立住

- `capabilities`(L1)**不再读进程环境**:`runtime_skill_dirs(home, project,
  atomcode_home)` 收参数,由宿主解析一次传下去;`registry.rs:264` 与 `render.rs:98`
  那两个读者消失。
- `harness` 里的 `model_source` 瘦身成"宿主此刻想用哪个来源"的策略,机器派生值不再
  经它。
- 行**只读 config**:skills 行从 `[[insert]] name = "skills"`(无 config,靠
  `row.home` 兜底)变成 `config = { home = "${HOME}" }`,那个 `.or_else(user_home)`
  兜底才能删。

## 权衡过、没做的

- **把展开点放进 `Profiles::resolve`**:约 20 个测试文件绕开 Profiles 自己折树,
  会得到"测试里 `${HOME}` 是字面量"的静默差异。
- **让 plexus 自己读环境**:通用容器里塞宿主关注点,0013 的边界反过来破。
- **按字段名抹敏感值**:位置式,漏一个打印点泄一次;值类型是位置无关的。
- **展开结果写回 `Entry.config` 之外的地方**(例如另开一份"已解析值"表):两份状态
  就有两个真源,正是本页要消掉的东西。

## 已知的代价与必须写明的行为

1. **`dump → 重载`不对称。** `Deserialize` 明文进、`Serialize` 打码出,所以 dump 出来
   的树**喂不回去**。今天这条路径不存在(树由 layers 折出,不从 dump 反序列化),
   但以后有人做"导出配置"会踩,得先知道。
2. **`row_label` 的 diff 看不出密钥更换。** `control.rs` 把 config 拼进标签来报"哪行
   变了",密钥打码后两把不同的密钥在那条标签上一样——换了密钥会显示成没变。要么在
   那一个打印点用 `reveal()`(即开一个口子),要么接受(密钥变更不进 diff 标签)。
   倾向前者限死一处 + 注释,或后者并写明。
3. **既有配置里若有字面量 `$`**,展开会改变它的含义。迁移要先扫一遍,并支持 `$$`
   转义。
4. **两份 `expand_env_vars` 收敛**时,注意 `mcp/config.rs` 那份额外与 `expand_tilde`
   组合使用(先展开变量再展开 `~`),收敛后这个组合顺序要在**一处**定义,不能丢。

## 闸门(实现时)

- **展开只在折树时发生一次**:同一份 config 折两次、展开两次,结果相同(幂等)。
- **未定义变量让 `from_layers_with` 报错**,且报出变量名;`${VAR:-默认}` 不报错。
- **`Secret` 不出现在任何打印输出里**:拿一棵含密钥的树跑 `--dump-config` 与
  `control.rs` 的 patch 报告,断言输出里没有密钥明文,且出现 `<redacted>`。
- **`from_layers` 不做展开**(手搓树的测试保持现状),`from_layers_with` 才做——
  两条路径各一条判据,否则"测试里 `${HOME}` 变字面量"这种差异抓不住。
- **capabilities 对进程环境零读取**:沿用本轮在 `tests/reasoning_effort.rs` 里那两条
  读源码的守卫形状(`nothing_outside_the_source_module_reaches_the_environment`),
  把范围扩到 `atomcode-capabilities`。

## 未做

- 本页只是形状决定。实现要动 plexus(遍历 + `Expander`)、宿主(注入)、
  capabilities(3 处读取改参数)、config(两份展开收敛、`Secret` 类型落地)——
  跨 crate,且碰到约 20 个测试文件的建树方式。
- 与 `atomcode-config` 的 `$VAR` 密钥机制(测试里的 `$TEST_API_KEY_ENV` /
  裸名即变量名)如何收敛**未定**:要么 config.toml 走同一个展开器,要么明确说它是
  宿主另一条输入、两边都必须是 `Secret`。动手前需要先读一遍那段代码确认现状。
