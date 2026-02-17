mod memory;
mod disk;
mod key;
mod inflight;

pub use memory::MemoryCache;
pub use disk::DiskCache;
pub use key::CacheKey;
pub use inflight::InflightDownloads;

use crate::error::Result;
use bytes::Bytes;
use async_trait::async_trait;
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
            + self.headers.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>()
            + 10 // status + timestamp overhead
    }
}

#[async_trait]
pub trait Cache: Send + Sync {
    async fn get(&self, key: &CacheKey) -> Result<Option<CacheEntry>>;
    async fn put(&self, key: CacheKey, entry: CacheEntry) -> Result<()>;
    async fn remove(&self, key: &CacheKey) -> Result<()>;
    #[allow(dead_code)]
    async fn clear(&self) -> Result<()>;
    #[allow(dead_code)]
    async fn size(&self) -> usize;
}

/// Two-tier cache: memory and disk
pub struct TieredCache {
    memory: MemoryCache,
    disk: DiskCache,
}

impl TieredCache {
    pub async fn new(
        memory_size: usize,
        disk_cache_dir: std::path::PathBuf,
        disk_size: u64,
    ) -> Result<Self> {
        let memory = MemoryCache::new(memory_size);
        let disk = DiskCache::new(disk_cache_dir, disk_size).await?;

        Ok(Self { memory, disk })
    }

    pub async fn get(&self, key: &CacheKey) -> Result<Option<CacheEntry>> {
        // Try memory first
        if let Some(entry) = self.memory.get(key).await? {
            if !entry.is_expired() {
                tracing::debug!("Cache hit (memory): {}", key.hash_hex());
                return Ok(Some(entry));
            }
            // Expired, remove from memory
            let _ = self.memory.remove(key).await;
        }

        // Try disk second
        if let Some(entry) = self.disk.get(key).await? {
            if !entry.is_expired() {
                tracing::debug!("Cache hit (disk): {}", key.hash_hex());
                // Promote to memory cache
                let _ = self.memory.put(key.clone(), entry.clone()).await;
                return Ok(Some(entry));
            }
            // Expired, remove from disk
            let _ = self.disk.remove(key).await;
        }

        tracing::debug!("Cache miss: {}", key.hash_hex());
        Ok(None)
    }

    pub async fn put(&self, key: CacheKey, entry: CacheEntry) -> Result<()> {
        // Always write to disk for large items
        if entry.size() > 1024 * 1024 {
            // > 1MB goes to disk
            self.disk.put(key.clone(), entry.clone()).await?;
        }

        // Try to fit in a memory cache
        self.memory.put(key, entry).await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn remove(&self, key: &CacheKey) -> Result<()> {
        let _ = self.memory.remove(key).await;
        let _ = self.disk.remove(key).await;
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn stats(&self) -> CacheStats {
        CacheStats {
            memory_size: self.memory.size().await,
            disk_size: self.disk.size().await,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub memory_size: usize,
    pub disk_size: usize,
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
        assert!(entry.size() > 0);
    }
}
