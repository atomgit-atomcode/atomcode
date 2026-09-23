//! 去过和要去的地方:`/cd` 的书签。
//!
//! 书签存在哪、怎么写,是启动器的事(配置文件),屏幕只问「有哪些」和「把这个记上
//! / 取消」(`docs/adr/0022` §3)。「最近去过的目录」不在这儿:那是会话自己带的事实,
//! `/cd` 问一趟宿主的会话目录就有,不必再存第二份。

use async_trait::async_trait;

/// 书签:人自己标下的目录,最新的在前。
#[async_trait]
pub trait Places: Send + Sync {
    /// 标下的目录,最新的在前。读不到就是空的——没有书签和读不到书签,对屏幕
    /// 是同一件事。
    async fn bookmarks(&self) -> Vec<String>;
    /// 把一个目录记上。已经记过就当没发生(但要挪到最前)。
    async fn pin(&self, dir: &str) -> Result<(), String>;
    /// 取消一个目录。没记过也算成功:人要的结果就是它不在里面。
    async fn unpin(&self, dir: &str) -> Result<(), String>;
}
