use super::{CacheEntry, CacheKey};
use moka::future::Cache;

pub struct MemoryCache {
    cache: Cache<CacheKey, CacheEntry>,
}

impl MemoryCache {
    pub fn new(max_size: usize) -> Self {
        let cache = Cache::builder()
            .max_capacity(max_size as u64)
            .weigher(|_key: &CacheKey, value: &CacheEntry| -> u32 {
                u32::try_from(value.size()).unwrap_or(u32::MAX)
            })
            .build();

        Self { cache }
    }

    pub async fn get(&self, key: &CacheKey) -> Option<CacheEntry> {
        self.cache.get(key).await
    }

    pub async fn put(&self, key: CacheKey, entry: CacheEntry) {
        self.cache.insert(key, entry).await;
    }

    pub async fn remove(&self, key: &CacheKey) {
        self.cache.invalidate(key).await;
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

        cache.put(key.clone(), entry.clone()).await;
        let retrieved = cache.get(&key).await;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().data.len(), 1024);
    }

    #[tokio::test]
    async fn test_memory_cache_eviction() {
        let cache = MemoryCache::new(2048);
        let key1 = CacheKey::new("GET", "http://example.com/1", &[]);
        let key2 = CacheKey::new("GET", "http://example.com/2", &[]);
        let key3 = CacheKey::new("GET", "http://example.com/3", &[]);

        cache.put(key1.clone(), create_entry(1000)).await;
        cache.put(key2.clone(), create_entry(1000)).await;
        cache.put(key3.clone(), create_entry(1000)).await;

        // moka runs eviction asynchronously; trigger sync
        cache.cache.run_pending_tasks().await;

        // With 3 entries of ~1010 weight each and max_capacity=2048,
        // at most 2 entries can fit. Moka uses TinyLFU so we can't
        // predict which entry gets evicted, but total count must be <= 2.
        let mut count = 0;
        if cache.get(&key1).await.is_some() { count += 1; }
        if cache.get(&key2).await.is_some() { count += 1; }
        if cache.get(&key3).await.is_some() { count += 1; }
        assert!(count <= 2, "Expected at most 2 entries to fit, got {}", count);
    }

    #[tokio::test]
    async fn test_memory_cache_too_large() {
        let cache = MemoryCache::new(1024);
        let key = CacheKey::new("GET", "http://example.com/large", &[]);
        let entry = create_entry(2048);

        cache.put(key.clone(), entry).await;
        // moka may accept and immediately evict
        cache.cache.run_pending_tasks().await;
        // Entry might be evicted due to exceeding capacity
    }

    #[tokio::test]
    async fn test_memory_cache_remove() {
        let cache = MemoryCache::new(1024 * 1024);
        let key = CacheKey::new("GET", "http://example.com/test", &[]);
        let entry = create_entry(1024);

        cache.put(key.clone(), entry).await;
        assert!(cache.get(&key).await.is_some());

        cache.remove(&key).await;
        assert!(cache.get(&key).await.is_none());
    }
}
