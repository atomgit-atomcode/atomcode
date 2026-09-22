//! 种子安装：全新机器上第一次 `/setup` 那一步，以及它读的那个口子。
//!
//! 和 [`crate::plugins`] 是同一副骨架，理由也同一条（`docs/adr/0022` §3）：
//! **种子是什么、装到哪、装了什么** 是产品那边才能回答的 —— 这个 crate 刻意只开
//! `atomcode-capabilities` 的 `tools` feature，而装种子要 `setup` feature（解压 tar.zst、
//! 扫描项目、原子写、文件锁）。所以端口是 async 的，实现在启动器那一侧。
//!
//! **为什么要有这个口子**：`/setup` 这条命令本身不用新写 —— `harness` 把每个
//! `user_invocable` 的 skill 自动登记成一条命令，而种子 skill
//! （`assets/setup-seeds/skills/atomcode-automation-recommender/SKILL.md`，frontmatter
//! 是 `name: setup`）正是这样一条。装过种子的机器上，屏幕侧什么都不必做。
//! 缺的只是**全新机器上第一次输 `/setup`**：那一刻种子还不在磁盘上，agent 的命令
//! 目录里也就没有它，命令会落到「没有 /setup 这条命令」。老前端
//! （`tuix/src/event_loop/commands.rs:3947`）为这一步写了一段专门的代码：装上种子 →
//! 重载插件 → 再把 `setup` skill 展开成人的一个回合。
//!
//! 这里把「装」拆成一个端口，「装完重载、然后转发」留在
//! [`crate::commands::SetupCommands`] —— 那一半是屏幕的编排，不是产品的事实。

/// 装种子，以及问一句「装过没有」。
///
/// 由启动屏幕的那一方填，和 [`crate::plugins::Plugins`] 同一个形状、同一条理由。
/// 回来的 `Ok` 是一句给人看的话（产物自带的报告原样带回来），`Err` 是启动器的拒绝。
#[async_trait::async_trait]
pub trait Setup: Send + Sync {
    /// 种子 skill 在不在。读本地几个小文件，同步。
    ///
    /// 拿来省掉一次多余的安装：装过的机器上重跑 `/setup`，本来只需要把那条命令
    /// 转发给 agent，再解压一遍、再锁一次文件、再重建一遍能力图都是白做的。
    fn installed(&self) -> bool;

    /// 装上种子，回一句给人看的话（装了什么、跳过了什么、有没有失败的）。
    async fn install(&self) -> Result<String, String>;
}
