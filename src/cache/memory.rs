use super::{Cache, CacheEntry, CacheKey};
use crate::error::Result;
use async_trait::async_trait;
use lru::LruCache;
use parking_lot::RwLock;
use std::num::NonZeroUsize;

pub struct MemoryCache {
    cache: RwLock<LruCache<CacheKey, CacheEntry>>,
    max_size: usize,
    current_size: RwLock<usize>,
}

impl MemoryCache {
    pub fn new(max_size: usize) -> Self {
        // LRU cache with capacity based on max_size / avg_entry_size estimate
        let capacity = NonZeroUsize::new((max_size / 1024).max(100)).unwrap();

        Self {
            cache: RwLock::new(LruCache::new(capacity)),
            max_size,
            current_size: RwLock::new(0),
        }
    }

    fn evict_if_needed(&self, needed_size: usize) {
        let mut cache = self.cache.write();
        let mut current_size = self.current_size.write();

        while *current_size + needed_size > self.max_size && cache.len() > 0 {
            if let Some((_, entry)) = cache.pop_lru() {
                let entry_size = entry.size();
                *current_size = current_size.saturating_sub(entry_size);
                tracing::debug!("Evicted entry from memory cache, freed {} bytes", entry_size);
            } else {
                break;
            }
        }
    }
}

#[async_trait]
impl Cache for MemoryCache {
    async fn get(&self, key: &CacheKey) -> Result<Option<CacheEntry>> {
        let mut cache = self.cache.write();
        Ok(cache.get(key).cloned())
    }

    async fn put(&self, key: CacheKey, entry: CacheEntry) -> Result<()> {
        let entry_size = entry.size();

        // Don't cache if entry is too large
        if entry_size > self.max_size {
            tracing::debug!("Entry too large for memory cache: {} bytes", entry_size);
            return Ok(());
        }

        // Evict old entries if needed
        self.evict_if_needed(entry_size);

        let mut cache = self.cache.write();
        let mut current_size = self.current_size.write();

        // Remove old entry if exists
        if let Some(old) = cache.pop(&key) {
            *current_size = current_size.saturating_sub(old.size());
        }

        cache.put(key, entry);
        *current_size += entry_size;

        tracing::debug!("Cached entry in memory: {} bytes, total: {}/{}",
                       entry_size, *current_size, self.max_size);

        Ok(())
    }

    async fn remove(&self, key: &CacheKey) -> Result<()> {
        let mut cache = self.cache.write();
        let mut current_size = self.current_size.write();

        if let Some(entry) = cache.pop(key) {
            *current_size = current_size.saturating_sub(entry.size());
        }

        Ok(())
    }

    async fn clear(&self) -> Result<()> {
        let mut cache = self.cache.write();
        let mut current_size = self.current_size.write();

        cache.clear();
        *current_size = 0;

        Ok(())
    }

    async fn size(&self) -> usize {
        *self.current_size.read()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::time::SystemTime;

    fn create_entry(size: usize) -> CacheEntry {
        CacheEntry {
            data: Bytes::from(vec![0u8; size]),
            headers: vec![],
            status: 200,
            timestamp: SystemTime::now(),
            ttl_seconds: 3600,
        }
    }

    #[tokio::test]
    async fn test_memory_cache_basic() {
        let cache = MemoryCache::new(1024 * 1024);
        let key = CacheKey::new("GET", "http://example.com/test", &[]);
        let entry = create_entry(1024);

        cache.put(key.clone(), entry.clone()).await.unwrap();
        let retrieved = cache.get(&key).await.unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().data.len(), 1024);
    }

    #[tokio::test]
    async fn test_memory_cache_eviction() {
        let cache = MemoryCache::new(2048);
        let key1 = CacheKey::new("GET", "http://example.com/1", &[]);
        let key2 = CacheKey::new("GET", "http://example.com/2", &[]);
        let key3 = CacheKey::new("GET", "http://example.com/3", &[]);

        cache.put(key1.clone(), create_entry(1000)).await.unwrap();
        cache.put(key2.clone(), create_entry(1000)).await.unwrap();
        cache.put(key3.clone(), create_entry(1000)).await.unwrap();

        // key1 should be evicted
        assert!(cache.get(&key1).await.unwrap().is_none());
        assert!(cache.get(&key2).await.unwrap().is_some());
        assert!(cache.get(&key3).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_memory_cache_too_large() {
        let cache = MemoryCache::new(1024);
        let key = CacheKey::new("GET", "http://example.com/large", &[]);
        let entry = create_entry(2048);

        cache.put(key.clone(), entry).await.unwrap();
        assert!(cache.get(&key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_memory_cache_remove() {
        let cache = MemoryCache::new(1024 * 1024);
        let key = CacheKey::new("GET", "http://example.com/test", &[]);
        let entry = create_entry(1024);

        cache.put(key.clone(), entry).await.unwrap();
        assert!(cache.get(&key).await.unwrap().is_some());

        cache.remove(&key).await.unwrap();
        assert!(cache.get(&key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_memory_cache_clear() {
        let cache = MemoryCache::new(1024 * 1024);
        let key1 = CacheKey::new("GET", "http://example.com/1", &[]);
        let key2 = CacheKey::new("GET", "http://example.com/2", &[]);

        cache.put(key1.clone(), create_entry(1024)).await.unwrap();
        cache.put(key2.clone(), create_entry(1024)).await.unwrap();

        cache.clear().await.unwrap();
        assert!(cache.get(&key1).await.unwrap().is_none());
        assert!(cache.get(&key2).await.unwrap().is_none());
        assert_eq!(cache.size().await, 0);
    }
}
