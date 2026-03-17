mod disk;
mod inflight;
mod key;
mod memory;

pub use disk::DiskCache;
pub use inflight::{
    DownloadAction, DownloadChunk, InflightDownloads, ResponseMeta, ResponseMetaResult,
};
pub use key::CacheKey;
pub use memory::MemoryCache;

use bytes::Bytes;
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub data: Bytes,
    pub headers: Vec<(String, String)>,
    pub status: u16,
    pub timestamp: SystemTime,
    pub ttl_seconds: u64,
}

impl CacheEntry {
    pub fn is_expired(&self) -> bool {
        if let Ok(elapsed) = self.timestamp.elapsed() {
            elapsed.as_secs() > self.ttl_seconds
        } else {
            true
        }
    }

    pub fn size(&self) -> usize {
        self.data.len()
            + self
                .headers
                .iter()
                .map(|(k, v)| k.len() + v.len() + 48) // +48 for two String heap allocs (24 bytes each)
                .sum::<usize>()
            + 64 // Bytes struct + Vec header + status + timestamp overhead
    }
}

/// Two-tier cache: memory and disk
pub struct TieredCache {
    memory: MemoryCache,
    disk: DiskCache,
    inflight: InflightDownloads,
}

impl TieredCache {
    pub async fn new(
        memory_size: usize,
        disk_cache_dir: std::path::PathBuf,
        disk_size: u64,
    ) -> crate::error::Result<Self> {
        let memory = MemoryCache::new(memory_size);
        let disk = DiskCache::new(disk_cache_dir, disk_size).await?;
        let inflight = InflightDownloads::new();

        Ok(Self {
            memory,
            disk,
            inflight,
        })
    }

    pub fn inflight(&self) -> &InflightDownloads {
        &self.inflight
    }

    pub async fn get(&self, key: &CacheKey) -> crate::error::Result<Option<CacheEntry>> {
        // Try memory first
        if let Some(entry) = self.memory.get(key).await {
            if !entry.is_expired() {
                tracing::debug!("Cache hit (memory): {}", key.hash_hex());
                return Ok(Some(entry));
            }
            // Expired, remove from memory
            self.memory.remove(key).await;
        }

        // Try disk second
        if let Some(entry) = self.disk.get(key).await? {
            if !entry.is_expired() {
                tracing::debug!("Cache hit (disk): {}", key.hash_hex());
                // Promote to memory cache
                self.memory.put(key.clone(), entry.clone()).await;
                return Ok(Some(entry));
            }
            // Expired, remove from disk
            let _ = self.disk.remove(key).await;
        }

        tracing::debug!("Cache miss: {}", key.hash_hex());
        Ok(None)
    }

    pub async fn put(&self, key: CacheKey, entry: CacheEntry) -> crate::error::Result<()> {
        // Large entries (>1MB) go to both memory and disk
        if entry.size() > 1024 * 1024 {
            self.disk.put(key.clone(), entry.clone()).await?;
        }

        // Try to fit in memory cache
        self.memory.put(key, entry).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_entry() -> CacheEntry {
        CacheEntry {
            data: Bytes::from("test data"),
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            status: 200,
            timestamp: SystemTime::now(),
            ttl_seconds: 3600,
        }
    }

    #[test]
    fn test_cache_entry_expiration() {
        let mut entry = create_test_entry();
        assert!(!entry.is_expired());

        // Simulate old timestamp
        entry.timestamp = SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(7200))
            .unwrap();
        assert!(entry.is_expired());
    }

    #[test]
    fn test_cache_entry_size() {
        let entry = create_test_entry();
        // 9 bytes data + (12 + 10 + 48) per-header with heap overhead + 64 struct overhead
        let expected = 9 + (12 + 10 + 48) + 64;
        assert_eq!(entry.size(), expected);
    }

    #[tokio::test]
    async fn test_tiered_cache_small_entry_memory_only() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cache = TieredCache::new(
            10 * 1024 * 1024,
            temp_dir.path().to_path_buf(),
            10 * 1024 * 1024,
        )
        .await
        .unwrap();

        let key = CacheKey::new("GET", "http://example.com/small", &[]);
        let entry = create_test_entry();

        // Small entry (<1MB) goes to memory only
        cache.put(key.clone(), entry.clone()).await.unwrap();

        // Should be retrievable
        let retrieved = cache.get(&key).await.unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().data, entry.data);

        // Should NOT be on disk (small entry)
        let disk_entry = cache.disk.get(&key).await.unwrap();
        assert!(
            disk_entry.is_none(),
            "Small entry should not be written to disk"
        );
    }

    #[tokio::test]
    async fn test_tiered_cache_large_entry_on_disk() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cache = TieredCache::new(
            10 * 1024 * 1024,
            temp_dir.path().to_path_buf(),
            10 * 1024 * 1024,
        )
        .await
        .unwrap();

        let key = CacheKey::new("GET", "http://example.com/large", &[]);
        // Create an entry >1MB to trigger disk write
        let entry = CacheEntry {
            data: Bytes::from(vec![0u8; 2 * 1024 * 1024]),
            headers: vec![],
            status: 200,
            timestamp: SystemTime::now(),
            ttl_seconds: 3600,
        };

        cache.put(key.clone(), entry.clone()).await.unwrap();

        // Should be on disk
        let disk_entry = cache.disk.get(&key).await.unwrap();
        assert!(
            disk_entry.is_some(),
            "Large entry should be written to disk"
        );
        assert_eq!(disk_entry.unwrap().data.len(), 2 * 1024 * 1024);
    }

    #[tokio::test]
    async fn test_tiered_cache_disk_to_memory_promotion() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cache = TieredCache::new(
            10 * 1024 * 1024,
            temp_dir.path().to_path_buf(),
            10 * 1024 * 1024,
        )
        .await
        .unwrap();

        let key = CacheKey::new("GET", "http://example.com/promote", &[]);
        let entry = CacheEntry {
            data: Bytes::from(vec![0u8; 2 * 1024 * 1024]),
            headers: vec![],
            status: 200,
            timestamp: SystemTime::now(),
            ttl_seconds: 3600,
        };

        // Put large entry (goes to both memory and disk)
        cache.put(key.clone(), entry).await.unwrap();

        // Remove from memory to simulate eviction
        cache.memory.remove(&key).await;
        assert!(cache.memory.get(&key).await.is_none());

        // Get should find it on disk and promote to memory
        let retrieved = cache.get(&key).await.unwrap();
        assert!(retrieved.is_some());

        // Now it should be back in memory
        let memory_entry = cache.memory.get(&key).await;
        assert!(memory_entry.is_some(), "Disk hit should promote to memory");
    }
}
