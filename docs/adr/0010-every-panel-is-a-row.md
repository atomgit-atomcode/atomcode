# 每个面板是一行

状态: 已实现(`crates/atomcode-tui/src/rows.rs`)

## 背景

用户看完新 TUI 的评价:「一点也不像是『插件化』的架构」。他是对的,证据两条:

`plugin.rs` 里面板是**写死的三行代码加一个 bool**:

```rust
let _ = mods.add_producer(transcript::Transcript::new());
let _ = mods.add_view(Arc::new(Mounted::<status::Status>::new()));
let _ = mods.add_view(Arc::new(Mounted::<input::Input>::new()));
if mascot { let _ = mods.add_view(Arc::new(Mounted::<status::Mascot>::new())); }
```

`bundle.rs` 里整个 TUI 在配置树上只占 `ui-tui` **一行**。

于是:`[[remove]] id = "tui-panel-status"` 做不到,`--audit` 看不见任何面板,
第三方加一个面板必须改 `plugin.rs` 重新编译,`--mascot` 是个 CLI 开关点亮一个 `if`。

**这是 Rust 意义上的插件化(trait + 注册表 + 一致性套件),不是这个仓库意义上的
插件化(「一个能力就是一行」)。** harness 那边守的规矩,到了 TUI 自己身上没守。

注册表本身是对的——`Modules::add_view` 的重复报错甚至写着「disable the row that
owns it first」,可那些行根本不存在。缺的只是行。

## 决策

**一个面板一行,一个命令集一行。** 注册表不动,`assemble()` 改成造一块**空屏**,
由树来填。

```
[[insert]] name = "tui-panel-transcript"
[[insert]] name = "tui-panel-status"
[[insert]] name = "tui-panel-input"
[[insert]] name = "tui-panel-mascot"     disabled = true
[[insert]] name = "tui-commands-screen" / -session / -tree / -layout / -help
```

三件顺带被摆正的事:

1. **`--mascot` 从 `ui` 行的一个布尔,变成 `tui-panel-mascot` 这一行。**
   而且这一行**自己用 `LayoutOp::Show` 上屏**——和快捷键、`/show`、模型的
   `adjust_layout` 走同一套词汇。之前它是 `assemble` 里的一个分支,构造了一棵
   不同的 region tree:**布局词汇表存在的唯一理由,恰恰被唯一一个绕过它的东西
   证伪了。**

2. **`toggle_module` 不再动注册表。** 旧版把 view 从注册表删掉,而且只能放回
   一种——吉祥物,因为它硬编码了那一个类型。轴选错了:**「存在」是树的事,
   「可见」是布局的事**,快捷键属于后者。现在它就是一个 `Show`/`Hide`。

3. **`LayoutSvc` 成为服务。** 之前 layout 是 `Host` 的私有字段,只有本文件够得着
   ——这正是吉祥物不得不做成特例的原因。

## 放弃了什么

### 一、保留 `assemble(surface, mascot)` 只加行

想过让行和硬编码并存,少改点东西。不行:两个来源就有两份「屏幕由什么组成」的
答案,而 `Commands::add` 的冲突检测当场证明了这一点——命令集被注册两遍,挂载失败。
**冲突检测把这个折中直接判死了,这是好闸门该干的事。**

### 二、`builtin()` 留着当便捷函数

删了。每个命令集都是行之后,一个「挂上常用的五个」的函数就是对
「一块屏幕有哪些命令」的第二个答案,而第二个答案是会过期的那个。

### 三、启动器和测试各留一份屏幕配置

原本就是各留一份,而且**当场分叉**:启动器加了九行新行,测试还在画空屏,
18 条端到端全红。改成 `rows::SCREEN` 一个家,两边都从库里取。
配套判据:`SCREEN` 提到的行必须在 `catalog()` 里,反之亦然。

## 失效条件

- 若面板需要按会话动态增删(而不是按树),`[[insert]]` 就不够了,得走 `Control` 缝。
- 若第三方 crate 真的开始提供面板,`catalog()` 返回本 crate 全部行的做法要让位给
  「宿主自己组装注册表」。
