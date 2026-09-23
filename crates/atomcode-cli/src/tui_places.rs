//! `/cd` 的书签住在配置文件里。
//!
//! 屏幕只知道「有哪些、记上、取消」,存在哪是启动器的事(`docs/adr/0022` §3)。
//! 存进 `[ui] cd_bookmarks`,因为它就是这个人的一项设置——换台机器不跟着走,
//! 和主题、界面一样。
//!
//! **最新的在前,不留重复,最多十条**:这份清单是给人一眼扫的,不是目录索引。
//! 再标一次已经标过的地方不是错,是「把它挪到前面来」。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::places::Places;
use atomcode_tui::plugin::PlacesSvc;
use serde_json::Value;

/// 行的名字。
pub const ROW: &str = "tui-places";

/// 最多记几条。
const MOST: usize = 10;

pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// 把书签那条端口填上的行。
pub struct PlacesRow {
    pub config_path: PathBuf,
}

#[async_trait]
impl Plugin for PlacesRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-places"]
    }
    fn description(&self) -> &'static str {
        "the directories a person marked for `/cd`, kept in the configuration file"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<PlacesSvc>(Arc::new(ConfigPlaces {
                path: self.config_path.clone(),
            }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

struct ConfigPlaces {
    path: PathBuf,
}

#[async_trait]
impl Places for ConfigPlaces {
    async fn bookmarks(&self) -> Vec<String> {
        read(&self.path)
    }

    async fn pin(&self, dir: &str) -> Result<(), String> {
        let dir = dir.to_string();
        write(&self.path, move |marked| {
            marked.retain(|already| already != &dir);
            marked.insert(0, dir.clone());
            marked.truncate(MOST);
        })
    }

    async fn unpin(&self, dir: &str) -> Result<(), String> {
        let dir = dir.to_string();
        write(&self.path, move |marked| {
            marked.retain(|already| already != &dir);
        })
    }
}

fn read(path: &std::path::Path) -> Vec<String> {
    if !path.exists() {
        return Vec::new();
    }
    atomcode_config::config::Config::load(path)
        .map(|config| config.ui.cd_bookmarks)
        .unwrap_or_default()
}

fn write(path: &std::path::Path, change: impl FnOnce(&mut Vec<String>)) -> Result<(), String> {
    atomcode_config::ConfigStore::new(path.to_path_buf())
        .update(|config| {
            change(&mut config.ui.cd_bookmarks);
            Ok(())
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places() -> (tempfile::TempDir, ConfigPlaces) {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        std::fs::write(&path, "language = \"zh_CN\"\n").unwrap();
        (home, ConfigPlaces { path })
    }

    #[tokio::test]
    async fn marking_the_same_place_twice_moves_it_to_the_front() {
        let (_home, places) = places();
        places.pin("/w/a").await.unwrap();
        places.pin("/w/b").await.unwrap();
        assert_eq!(places.bookmarks().await, vec!["/w/b", "/w/a"]);
        places.pin("/w/a").await.unwrap();
        assert_eq!(
            places.bookmarks().await,
            vec!["/w/a", "/w/b"],
            "不是加第二份,是挪到前面"
        );
    }

    #[tokio::test]
    async fn the_list_stays_short_and_unmarking_something_unmarked_is_fine() {
        let (_home, places) = places();
        for i in 0..(MOST + 3) {
            places.pin(&format!("/w/{i}")).await.unwrap();
        }
        let marked = places.bookmarks().await;
        assert_eq!(marked.len(), MOST, "给人扫的清单,不是目录索引");
        assert_eq!(marked[0], format!("/w/{}", MOST + 2), "最新的在最前");

        places.unpin("/w/never-marked").await.unwrap();
        places.unpin(&marked[0]).await.unwrap();
        assert!(!places.bookmarks().await.contains(&marked[0]));
    }

    /// 配置文件里别的东西不能被这次写动掉。
    #[tokio::test]
    async fn marking_a_place_keeps_the_rest_of_the_file() {
        let (_home, places) = places();
        places.pin("/w/a").await.unwrap();
        let config = atomcode_config::config::Config::load(&places.path).unwrap();
        assert_eq!(
            config.language,
            Some(atomcode_config::locale::Locale::ZhCn),
            "语言还在"
        );
    }
}
