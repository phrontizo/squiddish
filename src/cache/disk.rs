use super::{Cache, CacheEntry, CacheKey};
use crate::error::{ProxyError, Result};
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::RwLock;
use std::collections::HashMap;
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
    index: RwLock<HashMap<CacheKey, u64>>, // key -> size mapping for LRU
}

impl DiskCache {
    pub async fn new(cache_dir: PathBuf, max_size: u64) -> Result<Self> {
        fs::create_dir_all(&cache_dir).await?;

        let cache = Self {
            cache_dir,
            max_size,
            current_size: RwLock::new(0),
            index: RwLock::new(HashMap::new()),
        };

        // Build index from existing cache
        cache.rebuild_index().await?;

        Ok(cache)
    }

    async fn rebuild_index(&self) -> Result<()> {
        let mut total_size = 0u64;
        let index = HashMap::new();

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
                    // This is a metadata file, calculate size
                    if let Ok(metadata) = fs::metadata(&path).await {
                        let file_size = metadata.len();

                        // Also add data file size
                        let data_path = path.with_extension("data");
                        let data_size = if let Ok(data_meta) = fs::metadata(&data_path).await {
                            data_meta.len()
                        } else {
                            0
                        };

                        let entry_size = file_size + data_size;
                        total_size += entry_size;

                        // Try to read metadata to get the key
                        if let Ok(meta_content) = fs::read(&path).await {
                            if let Ok(_meta) = serde_json::from_slice::<DiskCacheMetadata>(&meta_content) {
                                // We need to reconstruct the key somehow - for now just track size
                                // In a production system, we'd store the key in metadata
                            }
                        }
                    }
                }
            }
        }

        *self.current_size.write() = total_size;
        *self.index.write() = index;

        tracing::info!("Disk cache index rebuilt: {} bytes used", total_size);
        Ok(())
    }

    async fn evict_if_needed(&self, needed_size: u64) -> Result<()> {
        let mut current_size = *self.current_size.read();

        if current_size + needed_size <= self.max_size {
            return Ok(());
        }

        // Simple eviction: find and remove oldest files until we have space
        let target_size = self.max_size.saturating_sub(needed_size);
        let _to_remove: Vec<PathBuf> = Vec::new();

        // Collect files with their modification times
        let mut stack = vec![self.cache_dir.clone()];
        let mut files: Vec<(PathBuf, SystemTime)> = Vec::new();

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
                        if let Ok(modified) = metadata.modified() {
                            files.push((path, modified));
                        }
                    }
                }
            }
        }

        // Sort by modification time (oldest first)
        files.sort_by_key(|(_, time)| *time);

        // Remove oldest files until we reach target
        for (path, _) in files {
            if current_size <= target_size {
                break;
            }

            let data_path = path.with_extension("data");

            // Get file sizes
            let meta_size = fs::metadata(&path).await.ok().map(|m| m.len()).unwrap_or(0);
            let data_size = fs::metadata(&data_path).await.ok().map(|m| m.len()).unwrap_or(0);
            let entry_size = meta_size + data_size;

            // Remove files
            let _ = fs::remove_file(&path).await;
            let _ = fs::remove_file(&data_path).await;

            current_size = current_size.saturating_sub(entry_size);
            tracing::debug!("Evicted disk cache entry: {} bytes freed", entry_size);
        }

        *self.current_size.write() = current_size;
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

        // Write data
        let mut data_file = fs::File::create(&data_path).await?;
        data_file.write_all(&entry.data).await?;

        let total_size = meta_json.len() as u64 + entry.data.len() as u64;

        Ok(total_size)
    }

    async fn read_entry(&self, key: &CacheKey) -> Result<Option<CacheEntry>> {
        let base_path = key.file_path(&self.cache_dir);
        let meta_path = base_path.with_extension("meta");
        let data_path = base_path.with_extension("data");

        // Check if files exist
        if !meta_path.exists() || !data_path.exists() {
            return Ok(None);
        }

        // Read metadata
        let mut meta_file = fs::File::open(&meta_path).await?;
        let mut meta_content = Vec::new();
        meta_file.read_to_end(&mut meta_content).await?;

        let metadata: DiskCacheMetadata = serde_json::from_slice(&meta_content)
            .map_err(|e| ProxyError::Cache(format!("Failed to deserialize metadata: {}", e)))?;

        // Read data
        let mut data_file = fs::File::open(&data_path).await?;
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
}

#[async_trait]
impl Cache for DiskCache {
    async fn get(&self, key: &CacheKey) -> Result<Option<CacheEntry>> {
        self.read_entry(key).await
    }

    async fn put(&self, key: CacheKey, entry: CacheEntry) -> Result<()> {
        let entry_size = entry.size() as u64;

        // Evict if needed
        self.evict_if_needed(entry_size).await?;

        // Remove old entry if exists
        let old_size = self.remove_entry(&key).await.unwrap_or(0);

        // Write new entry
        let new_size = self.write_entry(&key, &entry).await?;

        // Update size tracking
        let mut current_size = self.current_size.write();
        *current_size = current_size.saturating_sub(old_size) + new_size;

        self.index.write().insert(key.clone(), new_size);

        tracing::debug!("Cached entry on disk: {} bytes, total: {}/{}",
                       new_size, *current_size, self.max_size);

        Ok(())
    }

    async fn remove(&self, key: &CacheKey) -> Result<()> {
        let removed_size = self.remove_entry(key).await.unwrap_or(0);

        let mut current_size = self.current_size.write();
        *current_size = current_size.saturating_sub(removed_size);

        self.index.write().remove(key);

        Ok(())
    }

    async fn clear(&self) -> Result<()> {
        // Remove all cache files
        let _ = fs::remove_dir_all(&self.cache_dir).await;
        fs::create_dir_all(&self.cache_dir).await?;

        *self.current_size.write() = 0;
        self.index.write().clear();

        Ok(())
    }

    async fn size(&self) -> usize {
        *self.current_size.read() as usize
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

        // Create new cache instance with same directory
        let cache = DiskCache::new(temp_dir.path().to_path_buf(), 10 * 1024 * 1024)
            .await
            .unwrap();

        let retrieved = cache.get(&key).await.unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().data.len(), 1024);
    }
}
