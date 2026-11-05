//! Bounded, process-local search indexes. Filesystem work runs under a per-root
//! lock on the caller's blocking thread, never under the cache-map lock.

use atomcode_capabilities::file_index::{split_token, Entry, FileIndex};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

type Index = Arc<Mutex<FileIndex>>;
struct CachedRoot {
    root: PathBuf,
    own: String,
    index: Index,
}

pub(crate) struct MentionSearchCache {
    roots: Mutex<VecDeque<CachedRoot>>,
    capacity: usize,
}

impl MentionSearchCache {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            roots: Mutex::new(VecDeque::new()),
            capacity: capacity.max(1),
        }
    }

    pub(crate) fn search(&self, root: &Path, token: &str, own: &str) -> Vec<Entry> {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let index = {
            let mut roots = self.roots.lock().unwrap_or_else(|error| error.into_inner());
            let position = roots
                .iter()
                .position(|item| item.root == root && item.own == own);
            let item = match position {
                Some(position) => roots.remove(position).unwrap(),
                None => CachedRoot {
                    index: Arc::new(Mutex::new(FileIndex::new(root.clone(), own))),
                    root,
                    own: own.to_owned(),
                },
            };
            let index = item.index.clone();
            roots.push_back(item);
            while roots.len() > self.capacity {
                roots.pop_front();
            }
            index
        };
        let (scope, filter) = split_token(token);
        let matches = index
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .filter_blocking(&scope, &filter);
        matches
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/first.rs"), "").unwrap();
        root
    }

    #[test]
    fn prefixes_reuse_full_entries_but_roots_and_product_names_are_isolated() {
        let root = fixture();
        let cache = MentionSearchCache::new(8);
        assert_eq!(cache.search(root.path(), "src/fir", ".ours").len(), 1);
        std::fs::write(root.path().join("src/final.rs"), "").unwrap();
        let names: Vec<_> = cache
            .search(root.path(), "src/fi", ".ours")
            .into_iter()
            .map(|item| item.rel_path)
            .collect();
        assert_eq!(
            names,
            ["src/first.rs"],
            "fresh queries reuse the complete index"
        );
        assert_eq!(cache.search(root.path(), "src/fi", ".another").len(), 2);
        let other = fixture();
        assert!(cache.search(other.path(), "src/final", ".ours").is_empty());
        assert!(cache.search(root.path(), "src/missing", ".ours").is_empty());
    }

    #[test]
    fn eviction_bounds_roots_and_rebuilds_the_evicted_index() {
        let first = fixture();
        let second = fixture();
        let cache = MentionSearchCache::new(1);
        cache.search(first.path(), "first", ".ours");
        std::fs::write(first.path().join("new.rs"), "").unwrap();
        assert!(cache.search(first.path(), "new", ".ours").is_empty());
        cache.search(second.path(), "first", ".ours");
        assert_eq!(cache.roots.lock().unwrap().len(), 1);
        assert_eq!(cache.search(first.path(), "new", ".ours").len(), 1);
    }
}
