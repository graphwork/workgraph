//! In-memory per-session file content cache for the read_file tool.
//!
//! Provides LRU eviction at 25MB / 100 entries with mtime-based invalidation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

pub(crate) const MAX_CACHE_SIZE: usize = 25 * 1024 * 1024; // 25MB
pub(crate) const MAX_CACHE_ENTRIES: usize = 100;

struct CachedFile {
    content: String,
    mtime: SystemTime,
    last_accessed: Instant,
}

pub(crate) struct FileCache {
    entries: HashMap<PathBuf, CachedFile>,
    total_size: usize,
}

impl FileCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            total_size: 0,
        }
    }

    pub(crate) fn get(&mut self, path: &PathBuf, mtime: SystemTime) -> Option<String> {
        let is_hit = match self.entries.get(path) {
            Some(entry) => entry.mtime == mtime,
            None => return None,
        };

        if is_hit {
            let entry = self.entries.get_mut(path).unwrap();
            entry.last_accessed = Instant::now();
            Some(entry.content.clone())
        } else {
            let old = self.entries.remove(path).unwrap();
            self.total_size -= old.content.len();
            None
        }
    }

    pub(crate) fn insert(&mut self, path: PathBuf, content: String, mtime: SystemTime) {
        if let Some(old) = self.entries.remove(&path) {
            self.total_size -= old.content.len();
        }

        let content_size = content.len();

        while !self.entries.is_empty()
            && (self.total_size + content_size > MAX_CACHE_SIZE
                || self.entries.len() >= MAX_CACHE_ENTRIES)
        {
            let lru_key = self
                .entries
                .iter()
                .min_by_key(|(_, v)| v.last_accessed)
                .map(|(k, _)| k.clone())
                .unwrap();
            if let Some(evicted) = self.entries.remove(&lru_key) {
                self.total_size -= evicted.content.len();
            }
        }

        self.total_size += content_size;
        self.entries.insert(
            path,
            CachedFile {
                content,
                mtime,
                last_accessed: Instant::now(),
            },
        );
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn total_size(&self) -> usize {
        self.total_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_file_cache_hit() {
        let mut cache = FileCache::new();
        let path = PathBuf::from("/tmp/test_cache_hit.rs");
        let mtime = SystemTime::now();
        cache.insert(path.clone(), "hello world".to_string(), mtime);
        let result = cache.get(&path, mtime);
        assert_eq!(result, Some("hello world".to_string()));
    }

    #[test]
    fn test_file_cache_stale_mtime() {
        let mut cache = FileCache::new();
        let path = PathBuf::from("/tmp/test_cache_stale.rs");
        let mtime1 = SystemTime::now();
        cache.insert(path.clone(), "original content".to_string(), mtime1);
        let mtime2 = mtime1 + Duration::from_secs(1);
        let result = cache.get(&path, mtime2);
        assert_eq!(result, None);
        assert!(!cache.entries.contains_key(&path));
        assert_eq!(cache.total_size(), 0);
    }

    #[test]
    fn test_file_cache_lru_eviction() {
        let mut cache = FileCache::new();
        let mtime = SystemTime::now();
        for i in 0..MAX_CACHE_ENTRIES {
            cache.insert(
                PathBuf::from(format!("/tmp/file{}.rs", i)),
                "x".to_string(),
                mtime,
            );
        }
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
        let new_path = PathBuf::from("/tmp/file_new.rs");
        cache.insert(new_path.clone(), "y".to_string(), mtime);
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
        assert!(cache.entries.contains_key(&new_path));
    }

    #[test]
    fn test_file_cache_size_eviction() {
        let mut cache = FileCache::new();
        let mtime = SystemTime::now();
        let big_content = "a".repeat(20 * 1024 * 1024);
        cache.insert(PathBuf::from("/tmp/big.rs"), big_content, mtime);
        assert_eq!(cache.len(), 1);
        let medium_content = "b".repeat(10 * 1024 * 1024);
        cache.insert(PathBuf::from("/tmp/medium.rs"), medium_content, mtime);
        assert_eq!(cache.len(), 1);
        assert!(!cache.entries.contains_key(&PathBuf::from("/tmp/big.rs")));
        assert!(cache.entries.contains_key(&PathBuf::from("/tmp/medium.rs")));
    }

    #[test]
    fn test_file_cache_miss_on_unknown_path() {
        let mut cache = FileCache::new();
        let mtime = SystemTime::now();
        let result = cache.get(&PathBuf::from("/tmp/nonexistent.rs"), mtime);
        assert_eq!(result, None);
    }

    #[test]
    fn test_file_cache_insert_updates_existing() {
        let mut cache = FileCache::new();
        let path = PathBuf::from("/tmp/update.rs");
        let mtime1 = SystemTime::now();
        cache.insert(path.clone(), "version1".to_string(), mtime1);
        let mtime2 = mtime1 + Duration::from_secs(1);
        cache.insert(path.clone(), "version2".to_string(), mtime2);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.total_size(), "version2".len());
        let result = cache.get(&path, mtime2);
        assert_eq!(result, Some("version2".to_string()));
    }
}
