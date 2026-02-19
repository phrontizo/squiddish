use super::{CacheEntry, CacheKey};
use crate::error::{ProxyError, Result};
use bytes::Bytes;
use parking_lot::RwLock;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::SystemTime;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct DiskCacheMetadata {
    headers: Vec<(String, String)>,
    status: u16,
    timestamp: SystemTime,
    ttl_seconds: u64,
    data_size: usize,
}

pub struct DiskCache {
    cache_dir: PathBuf,
    max_size: u64,
    current_size: RwLock<u64>,
    /// Eviction order: (hash_hex, entry_size) — oldest entries at the front
    eviction_order: RwLock<VecDeque<(String, u64)>>,
}

impl DiskCache {
    pub async fn new(cache_dir: PathBuf, max_size: u64) -> Result<Self> {
        fs::create_dir_all(&cache_dir).await?;

        let cache = Self {
            cache_dir,
            max_size,
            current_size: RwLock::new(0),
            eviction_order: RwLock::new(VecDeque::new()),
        };

        cache.rebuild_index().await?;

        Ok(cache)
    }

    async fn rebuild_index(&self) -> Result<()> {
        let mut total_size = 0u64;
        let mut files: Vec<(String, u64, SystemTime)> = Vec::new();

        // Walk the cache directory
        let mut stack = vec![self.cache_dir.clone()];

        while let Some(dir) = stack.pop() {
            let mut entries = match fs::read_dir(&dir).await {
                Ok(e) => e,
                Err(_) => continue,
            };

            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|s| s.to_str()) == Some("meta") {
                    if let Ok(metadata) = fs::metadata(&path).await {
                        let file_size = metadata.len();
                        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);

                        // Also add data file size
                        let data_path = path.with_extension("data");
                        let data_size = match fs::metadata(&data_path).await {
                            Ok(data_meta) => data_meta.len(),
                            Err(_) => 0,
                        };

                        let entry_size = file_size + data_size;
                        total_size += entry_size;

                        // Extract hash hex from filename (filename without extension)
                        if let Some(hash_hex) = path.file_stem().and_then(|s| s.to_str()) {
                            files.push((hash_hex.to_string(), entry_size, modified));
                        }
                    }
                }
            }
        }

        // Sort by modification time (oldest first) for eviction order
        files.sort_by_key(|(_, _, time)| *time);

        let mut eviction_order = self.eviction_order.write();
        eviction_order.clear();
        for (hash_hex, size, _) in files {
            eviction_order.push_back((hash_hex, size));
        }

        *self.current_size.write() = total_size;

        tracing::info!(
            "Disk cache index rebuilt: {} bytes used, {} entries tracked",
            total_size,
            eviction_order.len()
        );
        Ok(())
    }

    async fn evict_if_needed(&self, needed_size: u64) -> Result<()> {
        let current_size = *self.current_size.read();

        if current_size + needed_size <= self.max_size {
            return Ok(());
        }

        let target_size = self.max_size.saturating_sub(needed_size);
        let mut freed = 0u64;

        loop {
            let entry = {
                let mut order = self.eviction_order.write();
                order.pop_front()
            };

            let Some((hash_hex, tracked_size)) = entry else {
                break;
            };

            if current_size.saturating_sub(freed) <= target_size {
                // Put it back, we've freed enough
                self.eviction_order.write().push_front((hash_hex, tracked_size));
                break;
            }

            // Construct file paths from hash_hex
            let shard = &hash_hex[0..2.min(hash_hex.len())];
            let subshard = if hash_hex.len() >= 4 { &hash_hex[2..4] } else { "" };
            let meta_path = self.cache_dir.join(shard).join(subshard).join(format!("{}.meta", hash_hex));
            let data_path = meta_path.with_extension("data");

            // Get actual file sizes
            let meta_size = fs::metadata(&meta_path).await.ok().map(|m| m.len()).unwrap_or(0);
            let data_size = fs::metadata(&data_path).await.ok().map(|m| m.len()).unwrap_or(0);
            let actual_size = meta_size + data_size;

            let _ = fs::remove_file(&meta_path).await;
            let _ = fs::remove_file(&data_path).await;

            freed += actual_size;
            tracing::debug!("Evicted disk cache entry {}: {} bytes freed", hash_hex, actual_size);
        }

        let mut current = self.current_size.write();
        *current = current.saturating_sub(freed);
        Ok(())
    }

    async fn write_entry(&self, key: &CacheKey, entry: &CacheEntry) -> Result<u64> {
        let base_path = key.file_path(&self.cache_dir);

        // Create parent directories
        if let Some(parent) = base_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let meta_path = base_path.with_extension("meta");
        let data_path = base_path.with_extension("data");

        // Write metadata
        let metadata = DiskCacheMetadata {
            headers: entry.headers.clone(),
            status: entry.status,
            timestamp: entry.timestamp,
            ttl_seconds: entry.ttl_seconds,
            data_size: entry.data.len(),
        };

        let meta_json = serde_json::to_vec(&metadata)
            .map_err(|e| ProxyError::Cache(format!("Failed to serialize metadata: {}", e)))?;

        let mut meta_file = fs::File::create(&meta_path).await?;
        meta_file.write_all(&meta_json).await?;
        meta_file.flush().await?;

        // Write data
        let mut data_file = fs::File::create(&data_path).await?;
        data_file.write_all(&entry.data).await?;
        data_file.flush().await?;

        let total_size = meta_json.len() as u64 + entry.data.len() as u64;

        Ok(total_size)
    }

    async fn read_entry(&self, key: &CacheKey) -> Result<Option<CacheEntry>> {
        let base_path = key.file_path(&self.cache_dir);
        let meta_path = base_path.with_extension("meta");
        let data_path = base_path.with_extension("data");

        // Use async file open to check existence (no blocking .exists())
        let mut meta_file = match fs::File::open(&meta_path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        let mut data_file = match fs::File::open(&data_path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        // Read metadata
        let mut meta_content = Vec::new();
        meta_file.read_to_end(&mut meta_content).await?;

        let metadata: DiskCacheMetadata = serde_json::from_slice(&meta_content)
            .map_err(|e| ProxyError::Cache(format!("Failed to deserialize metadata: {}", e)))?;

        // Read data
        let mut data_content = Vec::with_capacity(metadata.data_size);
        data_file.read_to_end(&mut data_content).await?;

        Ok(Some(CacheEntry {
            data: Bytes::from(data_content),
            headers: metadata.headers,
            status: metadata.status,
            timestamp: metadata.timestamp,
            ttl_seconds: metadata.ttl_seconds,
        }))
    }

    async fn remove_entry(&self, key: &CacheKey) -> Result<u64> {
        let base_path = key.file_path(&self.cache_dir);
        let meta_path = base_path.with_extension("meta");
        let data_path = base_path.with_extension("data");

        let meta_size = fs::metadata(&meta_path).await.ok().map(|m| m.len()).unwrap_or(0);
        let data_size = fs::metadata(&data_path).await.ok().map(|m| m.len()).unwrap_or(0);

        let _ = fs::remove_file(&meta_path).await;
        let _ = fs::remove_file(&data_path).await;

        Ok(meta_size + data_size)
    }

    pub async fn get(&self, key: &CacheKey) -> Result<Option<CacheEntry>> {
        self.read_entry(key).await
    }

    pub async fn put(&self, key: CacheKey, entry: CacheEntry) -> Result<()> {
        let entry_size = entry.size() as u64;

        // Evict if needed
        self.evict_if_needed(entry_size).await?;

        // Remove old entry if exists
        let old_size = self.remove_entry(&key).await.unwrap_or(0);

        // Write the new entry
        let new_size = self.write_entry(&key, &entry).await?;

        // Update size tracking
        let mut current_size = self.current_size.write();
        *current_size = current_size.saturating_sub(old_size) + new_size;

        // Track in eviction order
        self.eviction_order.write().push_back((key.hash_hex(), new_size));

        tracing::debug!("Cached entry on disk: {} bytes, total: {}/{}",
                       new_size, *current_size, self.max_size);

        Ok(())
    }

    pub async fn remove(&self, key: &CacheKey) -> Result<()> {
        let removed_size = self.remove_entry(key).await.unwrap_or(0);

        let mut current_size = self.current_size.write();
        *current_size = current_size.saturating_sub(removed_size);

        // Note: we don't remove from eviction_order (O(n) search).
        // Stale entries are skipped during eviction if files don't exist.

        Ok(())
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_entry(size: usize) -> CacheEntry {
        CacheEntry {
            data: Bytes::from(vec![0u8; size]),
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            status: 200,
            timestamp: SystemTime::now(),
            ttl_seconds: 3600,
        }
    }

    #[tokio::test]
    async fn test_disk_cache_basic() {
        let temp_dir = TempDir::new().unwrap();
        let cache = DiskCache::new(temp_dir.path().to_path_buf(), 10 * 1024 * 1024)
            .await
            .unwrap();

        let key = CacheKey::new("GET", "http://example.com/test", &[]);
        let entry = create_entry(1024);

        cache.put(key.clone(), entry.clone()).await.unwrap();
        let retrieved = cache.get(&key).await.unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().data.len(), 1024);
    }

    #[tokio::test]
    async fn test_disk_cache_remove() {
        let temp_dir = TempDir::new().unwrap();
        let cache = DiskCache::new(temp_dir.path().to_path_buf(), 10 * 1024 * 1024)
            .await
            .unwrap();

        let key = CacheKey::new("GET", "http://example.com/test", &[]);
        let entry = create_entry(1024);

        cache.put(key.clone(), entry).await.unwrap();
        assert!(cache.get(&key).await.unwrap().is_some());

        cache.remove(&key).await.unwrap();
        assert!(cache.get(&key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_disk_cache_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let key = CacheKey::new("GET", "http://example.com/test", &[]);
        let entry = create_entry(1024);

        {
            let cache = DiskCache::new(temp_dir.path().to_path_buf(), 10 * 1024 * 1024)
                .await
                .unwrap();
            cache.put(key.clone(), entry).await.unwrap();
        }

        // Create a new cache instance with the same directory
        let cache = DiskCache::new(temp_dir.path().to_path_buf(), 10 * 1024 * 1024)
            .await
            .unwrap();

        let retrieved = cache.get(&key).await.unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().data.len(), 1024);

        // Verify eviction order was rebuilt
        assert!(!cache.eviction_order.read().is_empty());
    }
}
