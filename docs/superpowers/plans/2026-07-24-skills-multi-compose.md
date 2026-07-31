# `/skills` 多 skill 组合 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `/skills` 命令用贪婪前缀匹配接受多个 skill 名，把它们的正文一起注入到一个任务回合；单 skill 用法完全不变。

**Architecture:** 新增一个纯函数 `split_skill_names`（TDD 全覆盖）把参数串切成"skill 名前缀列表 + 任务描述"；`/skills` 分支改用它，循环调用现有 `expand_skill` 拼接注入，并回显已加载的 skill 名。新增一条 i18n `Msg` 供回显。不改内核、不改 `atomcode-core::skill`。

**Tech Stack:** Rust；`atomcode-tuix`（命令层）+ `atomcode-config`（i18n）。

## Global Constraints

- 单 skill（无第二个 skill 词）行为必须与当前逐字节一致——零回归。
- 首词非 skill → 沿用现有 `Msg::SkillUnknown { name }` 报错，`name` = 第一个词。
- 任务参数传给**每个** skill 的 `expand`（保留 `$ARGUMENTS`/`$N` 占位符语义）。
- 任务描述从原始串按偏移切尾，**不得**用 `split_whitespace().join(" ")` 重组（会压平多空格/换行）。
- 新增 i18n `Msg` 必须同时在 `en.rs` 与 `zh_cn.rs` 加匹配臂（编译器强制穷尽）。
- 不引入 `--` 显式分隔符等新语法（YAGNI）。
- skill 名判定口径与 `expand_skill` 一致：`registry.get(name)` 存在且 `user_invocable == true`。

---

### Task 1: `split_skill_names` 纯函数 + 单元测试

**Files:**
- Modify: `crates/atomcode-tuix/src/event_loop/commands.rs`（在 `expand_skill`（约 3644 行）附近新增函数 + 同文件 `#[cfg(test)]` 测试）

**Interfaces:**
- Produces: `fn split_skill_names(arg: &str, resolve: impl Fn(&str) -> bool) -> (Vec<String>, String)` —
  返回 (去重后保持首见顺序的 skill 名列表, 任务描述串)。`resolve(token)` 返回 `true` 表示该 token 是已知 user-invocable skill。

- [ ] **Step 1: 写失败测试**

在 `commands.rs` 末尾的 `#[cfg(test)] mod tests`（若无则新建）中加入：

```rust
#[cfg(test)]
mod split_skill_names_tests {
    use super::split_skill_names;

    /// 测试用假解析器：这几个名字算已知 skill。
    fn known(name: &str) -> bool {
        matches!(
            name,
            "adapt-agent" | "skill-creator" | "brainstorming" | "a"
        )
    }

    #[test]
    fn multiple_skills_then_task() {
        let (skills, task) = split_skill_names("adapt-agent skill-creator 路径在哪", known);
        assert_eq!(skills, vec!["adapt-agent", "skill-creator"]);
        assert_eq!(task, "路径在哪");
    }

    #[test]
    fn single_skill_with_task_unchanged() {
        let (skills, task) = split_skill_names("brainstorming 做个登录页", known);
        assert_eq!(skills, vec!["brainstorming"]);
        assert_eq!(task, "做个登录页");
    }

    #[test]
    fn single_skill_no_task_unchanged() {
        let (skills, task) = split_skill_names("brainstorming", known);
        assert_eq!(skills, vec!["brainstorming"]);
        assert_eq!(task, "");
    }

    #[test]
    fn first_token_not_a_skill_yields_empty() {
        let (skills, task) = split_skill_names("路径在哪", known);
        assert!(skills.is_empty());
        assert_eq!(task, "路径在哪");
    }

    #[test]
    fn typo_second_skill_falls_into_task() {
        let (skills, task) = split_skill_names("adapt-agent skil-creator 路径在哪", known);
        assert_eq!(skills, vec!["adapt-agent"]);
        assert_eq!(task, "skil-creator 路径在哪");
    }

    #[test]
    fn duplicate_skill_deduped() {
        let (skills, task) = split_skill_names("a a 任务", known);
        assert_eq!(skills, vec!["a"]);
        assert_eq!(task, "任务");
    }

    #[test]
    fn task_whitespace_preserved_verbatim() {
        let (skills, task) = split_skill_names("brainstorming line1\n  line2", known);
        assert_eq!(skills, vec!["brainstorming"]);
        assert_eq!(task, "line1\n  line2");
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p atomcode-tuix split_skill_names_tests 2>&1 | tail -20`
Expected: 编译失败 `cannot find function split_skill_names in this scope`（函数尚未定义）。

- [ ] **Step 3: 实现纯函数**

在 `commands.rs` 中 `expand_skill` 函数上方新增：

```rust
/// 贪婪切分 `/skills` 参数：从左到右扫 whitespace 分词，只要当前 token 被
/// `resolve` 判定为已知 user-invocable skill 就收入列表（去重、保持首见顺序）；
/// 遇到第一个非 skill 的 token，它及其之后的内容（按原串偏移，保留原空白）作为
/// 任务描述返回。单个 skill（后面无第二个 skill 词）等价于旧的 `splitn(2)` 行为。
fn split_skill_names(arg: &str, resolve: impl Fn(&str) -> bool) -> (Vec<String>, String) {
    let mut skills: Vec<String> = Vec::new();
    let mut rest = arg.trim_start();
    loop {
        let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..token_end];
        if token.is_empty() || !resolve(token) {
            break;
        }
        if !skills.iter().any(|s| s == token) {
            skills.push(token.to_string());
        }
        rest = rest[token_end..].trim_start();
    }
    (skills, rest.to_string())
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p atomcode-tuix split_skill_names_tests 2>&1 | tail -20`
Expected: `test result: ok. 7 passed`。

- [ ] **Step 5: 提交**

```bash
git add crates/atomcode-tuix/src/event_loop/commands.rs
git commit -m "feat(skills): split_skill_names 贪婪前缀解析纯函数"
```

---

### Task 2: i18n 回显文案 + 接线 `/skills` 分支

**Files:**
- Modify: `crates/atomcode-config/src/i18n/messages.rs:719`（`SkillUnknown` 之后加 `SkillsLoaded`）
- Modify: `crates/atomcode-config/src/i18n/en.rs:613`（`SkillUnknown` 臂之后加 `SkillsLoaded` 臂）
- Modify: `crates/atomcode-config/src/i18n/zh_cn.rs:596`（同上）
- Modify: `crates/atomcode-tuix/src/event_loop/commands.rs:3462-3480`（`"skills"` 分支的 `else` 块）

**Interfaces:**
- Consumes: `split_skill_names`（Task 1）；现有 `expand_skill(ctx: &LoopCtx, name: &str, arg: &str) -> Option<String>`；现有 `submit_agent_turn(ctx: &LoopCtx, state: &mut UiState, text: String)`；现有 `Msg::SkillUnknown { name: &str }`。
- Produces: `Msg::SkillsLoaded { names: &'a str }`。

- [ ] **Step 1: 新增 i18n `Msg` 变体**

在 `crates/atomcode-config/src/i18n/messages.rs` 的 `SkillUnknown { name: &'a str },`（719-721 行）之后插入：

```rust
    SkillsLoaded {
        names: &'a str,
    },
```

- [ ] **Step 2: 加 en 匹配臂**

在 `crates/atomcode-config/src/i18n/en.rs` 的 `Msg::SkillUnknown { name } => ...`（612-613 行）之后插入：

```rust
        Msg::SkillsLoaded { names } =>
            format!("  Loaded skills: {}\n", names).into(),
```

- [ ] **Step 3: 加 zh_cn 匹配臂**

在 `crates/atomcode-config/src/i18n/zh_cn.rs` 的 `Msg::SkillUnknown { name } => ...`（596 行附近）之后插入：

```rust
        Msg::SkillsLoaded { names } =>
            format!("  已加载 skills：{}\n", names).into(),
```

- [ ] **Step 4: 运行 i18n 测试确认新变体编译通过**

Run: `cargo test -p atomcode-config 2>&1 | tail -5`
Expected: 编译通过、`test result: ok`（穷尽匹配下 en/zh 都补齐才会编译成功）。

- [ ] **Step 5: 接线 `/skills` 分支**

把 `crates/atomcode-tuix/src/event_loop/commands.rs` 中 `"skills"` 分支里 `arg_trim` 非空的整个 `else { ... }` 块（当前 3462-3480 行，`splitn(2)` 那段）替换为：

```rust
            } else {
                // 贪婪多 skill 解析：前缀是一串已知 skill 名，其余是任务描述，
                // 任务描述会传给每个 skill（保留 $ARGUMENTS 占位符语义）。单个
                // skill（无第二个 skill 词）解析结果与旧 splitn(2) 一致，零回归。
                let resolve = |name: &str| {
                    ctx.skill_registry
                        .read()
                        .ok()
                        .and_then(|r| r.get(name).map(|s| s.user_invocable))
                        .unwrap_or(false)
                };
                let (skills, skill_args) = split_skill_names(arg_trim, resolve);
                if skills.is_empty() {
                    // 首词不是 skill —— 沿用旧的 unknown 报错，指名第一个词。
                    let first = arg_trim.split_whitespace().next().unwrap_or("");
                    renderer.render(UiLine::Error(
                        t(Msg::SkillUnknown { name: first }).into_owned(),
                    ));
                    renderer.flush();
                } else {
                    // 按顺序展开每个 skill；expand_skill 可能因竞态返回 None。
                    let blocks: Vec<String> = skills
                        .iter()
                        .filter_map(|name| expand_skill(ctx, name.as_str(), &skill_args))
                        .collect();
                    if blocks.is_empty() {
                        renderer.render(UiLine::Error(
                            t(Msg::SkillUnknown {
                                name: skills[0].as_str(),
                            })
                            .into_owned(),
                        ));
                        renderer.flush();
                    } else {
                        // 回显已加载 skill：第二个及以后的 skill 名若打错字会静默
                        // 落进任务描述，这行让用户一眼看出"只加载了 N 个"。
                        let names = skills.join(" · ");
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::SkillsLoaded {
                                names: names.as_str(),
                            })
                            .into_owned(),
                        ));
                        renderer.flush();
                        let rendered = blocks.join("\n\n---\n\n");
                        submit_agent_turn(ctx, state, rendered);
                    }
                }
            }
```

- [ ] **Step 6: 构建确认接线编译通过**

Run: `cargo build -p atomcode-tuix 2>&1 | tail -8`
Expected: `Finished`，无 error（可能存在与本改动无关的既有告警）。

- [ ] **Step 7: 回归 —— 全量相关测试**

Run: `cargo test -p atomcode-tuix -p atomcode-config 2>&1 | grep -E "test result: FAIL|test result: ok|error\[" | tail -20`
Expected: 全部 `test result: ok`，无 `FAIL`/`error`。

- [ ] **Step 8: 手动验证（真机/交互，无法自动化的 TUI 路径）**

在 atomcode 交互会话中依次执行并观察：
- `/skills brainstorming` → 仅加载 brainstorming（同今天）。
- `/skills brainstorming 做个登录页` → 加载 brainstorming、任务=`做个登录页`（同今天）。
- `/skills adapt-agent skill-creator 路径在哪` → 回显 `已加载 skills：adapt-agent · skill-creator`，任务=`路径在哪`。
- `/skills 不存在的名字` → `Unknown skill: 不存在的名字`。
（`adapt-agent`/`skill-creator` 需为当前工程已安装的 user-invocable skill；否则换两个实际已安装的 skill 名验证。）

- [ ] **Step 9: 提交**

```bash
git add crates/atomcode-config/src/i18n/messages.rs \
        crates/atomcode-config/src/i18n/en.rs \
        crates/atomcode-config/src/i18n/zh_cn.rs \
        crates/atomcode-tuix/src/event_loop/commands.rs
git commit -m "feat(skills): /skills 支持贪婪匹配多个 skill 名 + 回显已加载"
```

---

## Self-Review

**Spec coverage：**
- §1 解析（贪婪前缀纯函数）→ Task 1。
- §2 单 skill 向后兼容 → Task 1 的 `single_skill_*` 测试 + Global Constraints。
- §3 组合注入（顺序=输入顺序、分隔符、任务传给每个 skill）→ Task 2 Step 5（`blocks.join("\n\n---\n\n")`、`expand_skill(.., &skill_args)`）。
- §4 回显防 typo → Task 2 Step 1-3（Msg）+ Step 5（`SkillsLoaded` 渲染）。
- §5 边界（首词非 skill 报错 / 去重）→ Task 2 Step 5（`skills.is_empty()` 分支）+ Task 1 `duplicate_skill_deduped`。
- 测试清单 → Task 1 七个用例逐条对应。

**Placeholder scan：** 无 TBD/TODO；每个代码步骤含完整代码；手动验证步骤给了具体命令与预期。

**Type consistency：** `split_skill_names(&str, impl Fn(&str)->bool) -> (Vec<String>, String)` 在 Task 1 定义、Task 2 Step 5 消费一致；`Msg::SkillsLoaded { names: &'a str }` 定义（messages.rs）与消费（`names: names.as_str()`）一致；`expand_skill(ctx, name.as_str(), &skill_args)` 与现有签名 `(&LoopCtx, &str, &str)` 一致。
