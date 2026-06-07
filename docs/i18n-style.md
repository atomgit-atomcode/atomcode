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

## 新增翻译的流程

添加一条新的可翻译文本时，必须同时修改三个文件，缺一不可。Rust 编译器会通过 `match` 穷尽性检查保证不会遗漏。

### 步骤

1. **在 `messages.rs` 添加 variant**

   ```rust
   pub enum Msg<'a> {
       // ... 已有 variants
       StatusTokenUsage { used: u32, total: u32 },  // 新增
   }
   ```

2. **在 `en.rs` 添加英文 match arm**

   ```rust
   Msg::StatusTokenUsage { used, total } => {
       format!("{used}/{total} tokens used")
   }
   ```

3. **在 `zh_cn.rs` 添加中文 match arm**

   ```rust
   Msg::StatusTokenUsage { used, total } => {
       format!("已使用 {used}/{total} 个 token")
   }
   ```

4. **编译验证**

   ```bash
   cargo build -p atomcode-core
   ```

   如果任一语言文件遗漏了新 variant，编译将失败并明确指出缺少的分支，从而杜绝翻译遗漏。

### 检查清单

- [ ] `messages.rs` 中添加了新 variant
- [ ] `en.rs` 中添加了对应的英文文本
- [ ] `zh_cn.rs` 中添加了对应的中文文本
- [ ] 中文文本符合本风格指南的标点、空格、术语规范
- [ ] 编译通过，无 `non-exhaustive patterns` 错误
