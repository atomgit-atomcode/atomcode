# AtomCode i18n 风格指南

## 总则

翻译的目标是产出自然流畅的中文，而非逐词对照的机械翻译。译文应当读起来像是直接用中文写成的，而不是"能看出英文原文"的翻译腔。在保持准确性的前提下，优先选择符合中文表达习惯的句式。

---

## 标点规范

- 中文语境下统一使用全角标点：`，。；：「」（）`
- 不得混用半角逗号 `,` 和句号 `.` 代替全角标点
- 括号内如果全部是英文或代码，则可以使用半角括号：`(API key)`
- 引号优先使用直角引号 `「」`，嵌套时使用 `『』`

**正确示例：**

```
请输入你的 API key，然后点击「确认」。
该功能需要 Provider 配置（详见 CodingPlan 文档）。
错误信息：连接超时，请稍后重试。
```

**错误示例：**

```
请输入你的 API key,然后点击"确认".     ← 半角逗号、句号
该功能需要 Provider 配置(详见 CodingPlan 文档)  ← 中文语境用了半角括号
```

---

## 英文术语保留策略

以下类型的术语保持英文原文，不进行翻译：

| 类别 | 示例 |
|------|------|
| 产品名 / 品牌名 | AtomCode、AtomGit、Claude |
| 功能模块专有名词 | Provider、CodingPlan、Skill |
| 技术标识符 | API key、Base URL、model name、token |
| 命令 / CLI 参数 | `--provider`、`--model` |

**规则：** 英文术语前后必须加半角空格，使其与中文文字视觉分离。

```
使用 CodingPlan 配置你的工作流。       ← 正确
使用CodingPlan配置你的工作流。         ← 错误，缺少空格
```

---

## 数字

- 统一使用半角（ASCII）数字：`0-9`
- 数字与中文之间加半角空格
- 不使用中文数字（一、二、三），除非是固定用语（如「第一步」）

**正确示例：**

```
已使用 3 次，剩余 7 次。
最多支持 128 个并发连接。
```

**错误示例：**

```
已使用三次，剩余七次。               ← 不必要的中文数字
已使用3次，剩余7次。                 ← 缺少空格
```

---

## 中英混排空格

核心规则：

| 场景 | 规则 | 示例 |
|------|------|------|
| 中文与英文之间 | 加半角空格 | `使用 Provider 配置` |
| 中文与数字之间 | 加半角空格 | `共 3 个选项` |
| 中文与全角标点之间 | 不加空格 | `配置完成，请重启。` |
| 英文与半角标点之间 | 不加空格 | `API key` |

---

## 简繁体

- 统一使用**简体中文**（`zh_CN`）
- `zh_TW` 用户暂时映射到简体中文，后续视需求补充繁体翻译
- 避免出现繁体字混入简体文本的情况（常见于复制粘贴）

---

## Msg variant 命名规范

### 命名格式

使用 `<Surface><Detail>` 的 PascalCase 格式，清晰表达消息所属的界面区域和具体内容。

```rust
// 界面区域 + 具体内容
WelcomeBannerLine1
WelcomeBannerLine2
StatusNoProvider
StatusConnecting
SettingsLabelModel
ErrorNetworkTimeout
```

### 带变量的 variant

使用命名字段（named fields），不使用位置字段：

```rust
// 正确：命名字段
ErrLoginFailed { reason: &'a str }
StatusTokenUsage { used: u32, total: u32 }

// 错误：位置字段
ErrLoginFailed(&'a str)
```

### 命名前缀约定

| 前缀 | 用途 | 示例 |
|------|------|------|
| `Welcome*` | 欢迎页 / 首次启动 | `WelcomeBannerLine1` |
| `Status*` | 状态栏 / 连接状态 | `StatusNoProvider` |
| `Settings*` | 设置页面 | `SettingsLabelModel` |
| `Err*` | 错误提示 | `ErrLoginFailed` |
| `Confirm*` | 确认对话框 | `ConfirmDeleteProject` |
| `Tooltip*` | 悬浮提示 | `TooltipCopyToken` |

---

## 词表在哪

一个叶子 crate，两张表，一个 locale：

```text
crates/atomcode-i18n/
  locale.rs     Locale（en / zh_CN），serde 直接读写 config.toml
  runtime.rs    LOCALE / BRAND / OAUTH、set_locale、{brand} 代入、test_lock
  product/      产品说的话：CLI、daemon、setup、/login、tuix
  screen/       屏幕说的话：atomcode-tui 与它在 cli 里的启动器行
```

- **零 atomcode 依赖**，所以两边都能读它而不破分层：`atomcode-config` 依赖它并按旧路径
  re-export（`atomcode_config::i18n` = `atomcode_i18n::product`，`atomcode_config::locale`
  同理），`atomcode-tui` 直接依赖它。`gates/layers.sh` 守着这条叶子。
- **一个 locale**。`/language` 只调一次 `set_locale`，两张表同时换。屏幕自带第二张表、
  第二个 locale 的写法已经试过：结果是欢迎块换了、状态栏没换。
- 前端各有一行本地导入面，形状一样：`tui/src/i18n/mod.rs` 是
  `pub use atomcode_i18n::screen::*;`，`tuix/src/i18n/mod.rs` 是
  `pub use atomcode_config::i18n::*;`。调用点一律 `crate::i18n::t(Msg::X)`，需要
  `String` 时 `.into_owned()`——**不要**为省几个字再造一个返回 `String` 的别名。

### 屏幕怎么读产品那张表

`atomcode_i18n::screen` re-export 了 `product`，所以屏幕代码写：

```rust
use crate::i18n::product::{t as pt, Msg as PMsg};
pt(PMsg::ApprovalAllowOnce)
```

**产品表已经有的那句话，屏幕读它，不重写。** 「允许一次」「Accounts」「{n}m ago」、
策略介入的四个选项、待办面板的表头，都是两个前端说同一件事——写第二遍就是给它们两
种说法的机会。`gates/tui-i18n.sh` 的第二条判据数的就是这个，基线 0。

## 新增翻译的流程

添加一条新的可翻译文本时，必须同时修改三个文件，缺一不可。Rust 编译器会通过 `match`
穷尽性检查保证不会遗漏。

**先问一句：产品表里有没有？** 有就读它，这一步到此为止。

### 步骤

1. **在 `messages.rs` 添加 variant**（`product/` 还是 `screen/`，看这句话是谁说的）

   ```rust
   pub enum Msg<'a> {
       // ... 已有 variants
       StatusTokenUsage { used: u32, total: u32 },  // 新增
   }
   ```

2. **在 `en.rs` 添加英文 match arm**

   ```rust
   Msg::StatusTokenUsage { used, total } => {
       format!("{used}/{total} tokens used").into()
   }
   ```

3. **在 `zh_cn.rs` 添加中文 match arm**

   ```rust
   Msg::StatusTokenUsage { used, total } => {
       format!("已使用 {used}/{total} 个 token").into()
   }
   ```

4. **编译验证**

   ```bash
   cargo check -p atomcode-i18n
   ```

   如果任一语言文件遗漏了新 variant，编译将失败并明确指出缺少的分支，从而杜绝翻译遗漏。

### 检查清单

- [ ] 产品表里没有同义的条目（有就读它，不新增）
- [ ] `messages.rs` 中添加了新 variant，字段是**具名**的
- [ ] `en.rs` 中添加了对应的英文文本，而且**真的是英文**
- [ ] `zh_cn.rs` 中添加了对应的中文文本
- [ ] 中文文本符合本风格指南的标点、空格、术语规范
- [ ] 编译通过，无 `non-exhaustive patterns` 错误

## 机器判的部分

散文管不住的三件事，各有判据：

| 判据 | 在哪 | 判什么 |
| --- | --- | --- |
| `hardcoded_cjk` | `gates/tui-i18n.sh` | 屏幕与启动器的生产代码里还有几处写死的中文字面量（棘轮，只能降） |
| `said_twice` | 同上 | 同一句话（中英都相同）在两张表里各写了一遍（基线 0） |
| `no_english_arm_is_left_in_chinese` | `atomcode-i18n/tests/tables.rs` | 英文表里还留着中文的条目——复制上一行忘了改后半句 |
| `the_two_tables_say_nothing_twice` | 同上 | 与 `said_twice` 同一条规则，跟着测试跑 |
| `atomcode-tui/tests/language.rs` | 五条 | 同一批界面画两遍，必须读起来不同、而且各自是对的文字系统 |

闸门本身有阴性对照（`gates/tui-i18n.spec.sh`）：每条规则各造一个违规 fixture 断言它判红，
再造合规的断言判绿——包括注释里的中文、测试里的中文、被豁免的 fixture 文件都不许误报。

**测试断言哪种语言**：`atomcode-tui` 与 `atomcode-cli` 的测试二进制各有一个
`#[cfg(test)] #[ctor]` 把 locale 设成 `zh_CN`，因为那些断言是照中文写的，意思是「中文下
这一行读作 X」。另一种语言由上表最后一行的五条判据单独钉住。要断言英文的单个测试
自己 `test_lock()` + `set_locale(En)`。
