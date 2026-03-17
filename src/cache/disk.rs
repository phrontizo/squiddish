use super::{CacheEntry, CacheKey};
use crate::error::{ProxyError, Result};
use bytes::Bytes;
use parking_lot::RwLock;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::SystemTime;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Construct a cache file base path from a hash hex string.
/// Uses the same 2-level sharding as `CacheKey::file_path`: base_dir/XX/YY/hash.
fn file_path_from_hash(base_dir: &std::path::Path, hash_hex: &str) -> PathBuf {
    let shard = &hash_hex[0..2];
    let subshard = &hash_hex[2..4];
    base_dir.join(shard).join(subshard).join(hash_hex)
}

/// On-disk cache entry metadata. Uses `#[serde(default)]` so that new fields
/// added in future versions won't break deserialization of existing cache files.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct DiskCacheMetadata {
    headers: Vec<(String, String)>,
    status: u16,
    timestamp: SystemTime,
    ttl_seconds: u64,
    data_size: usize,
}

impl Default for DiskCacheMetadata {
    fn default() -> Self {
        Self {
            headers: Vec::new(),
            status: 200,
            timestamp: SystemTime::UNIX_EPOCH,
            ttl_seconds: 0,
            data_size: 0,
        }
    }
}

pub struct DiskCache {
    cache_dir: PathBuf,
    max_size: u64,
    current_size: RwLock<u64>,
    /// Eviction order: hash_hex strings, oldest entries at the front
    eviction_order: RwLock<VecDeque<String>>,
    /// Serializes write operations (evict + put) to prevent TOCTOU races
    /// where concurrent puts both read the same current_size and over-evict.
    write_mutex: tokio::sync::Mutex<()>,
}

impl DiskCache {
    pub async fn new(cache_dir: PathBuf, max_size: u64) -> Result<Self> {
        fs::create_dir_all(&cache_dir).await?;

        let cache = Self {
            cache_dir,
            max_size,
            current_size: RwLock::new(0),
            eviction_order: RwLock::new(VecDeque::new()),
            write_mutex: tokio::sync::Mutex::new(()),
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
                Err(e) => {
                    tracing::warn!("Failed to read cache directory {:?}: {}", dir, e);
                    continue;
                }
            };

            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                let is_dir = entry
                    .file_type()
                    .await
                    .map(|ft| ft.is_dir())
                    .unwrap_or(false);
                if is_dir {
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
        for (hash_hex, _, _) in files {
            eviction_order.push_back(hash_hex);
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
        // Safe to snapshot: callers hold write_mutex, so no concurrent modifications
        let current_size = *self.current_size.read();

        if current_size + needed_size <= self.max_size {
            return Ok(());
        }

        let target_size = self.max_size.saturating_sub(needed_size);
        let mut freed = 0u64;

        loop {
            if current_size.saturating_sub(freed) <= target_size {
                break;
            }

            let entry = {
                let mut order = self.eviction_order.write();
                order.pop_front()
            };

            let Some(hash_hex) = entry else {
                break;
            };

            let base_path = file_path_from_hash(&self.cache_dir, &hash_hex);
            let meta_path = base_path.with_extension("meta");
            let data_path = base_path.with_extension("data");

            // Get actual file sizes
            let meta_size = fs::metadata(&meta_path)
                .await
                .ok()
                .map(|m| m.len())
                .unwrap_or(0);
            let data_size = fs::metadata(&data_path)
                .await
                .ok()
                .map(|m| m.len())
                .unwrap_or(0);
            let actual_size = meta_size + data_size;

            let _ = fs::remove_file(&meta_path).await;
            let _ = fs::remove_file(&data_path).await;

            freed += actual_size;
            tracing::debug!(
                "Evicted disk cache entry {}: {} bytes freed",
                hash_hex,
                actual_size
            );
        }

        let mut current = self.current_size.write();
        *current = current.saturating_sub(freed);
        Ok(())
    }

    /// Write a cache entry to disk using pre-serialized metadata bytes.
    async fn write_entry(
        &self,
        key: &CacheKey,
        entry: &CacheEntry,
        meta_json: &[u8],
    ) -> Result<u64> {
        let base_path = key.file_path(&self.cache_dir);

        // Create parent directories
        if let Some(parent) = base_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let meta_path = base_path.with_extension("meta");
        let data_path = base_path.with_extension("data");

        // Write metadata
        let mut meta_file = fs::File::create(&meta_path).await?;
        meta_file.write_all(meta_json).await?;
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

        let meta_size = fs::metadata(&meta_path)
            .await
            .ok()
            .map(|m| m.len())
            .unwrap_or(0);
        let data_size = fs::metadata(&data_path)
            .await
            .ok()
            .map(|m| m.len())
            .unwrap_or(0);

        let _ = fs::remove_file(&meta_path).await;
        let _ = fs::remove_file(&data_path).await;

        Ok(meta_size + data_size)
    }

    pub async fn get(&self, key: &CacheKey) -> Result<Option<CacheEntry>> {
        self.read_entry(key).await
    }

    pub async fn put(&self, key: CacheKey, entry: CacheEntry) -> Result<()> {
        // Serialize writes to prevent TOCTOU races in evict_if_needed
        let _guard = self.write_mutex.lock().await;

        // Serialize metadata once — used for both eviction size estimate and disk write
        let meta_json = serde_json::to_vec(&DiskCacheMetadata {
            headers: entry.headers.clone(),
            status: entry.status,
            timestamp: entry.timestamp,
            ttl_seconds: entry.ttl_seconds,
            data_size: entry.data.len(),
        })
        .map_err(|e| ProxyError::Cache(format!("Failed to serialize metadata: {}", e)))?;
        let entry_size = meta_json.len() as u64 + entry.data.len() as u64;

        // Reject entries that exceed the entire cache capacity — writing them would
        // evict everything and then immediately exceed max_size with no recourse.
        if entry_size > self.max_size {
            tracing::debug!(
                "Entry too large for disk cache ({} > {}), skipping",
                entry_size,
                self.max_size
            );
            return Ok(());
        }

        // Evict if needed
        self.evict_if_needed(entry_size).await?;

        // Remove old entry if exists
        let old_size = self.remove_entry(&key).await.unwrap_or(0);
        let key_hex = key.hash_hex();

        // Remove any stale ghost entries from eviction order before adding the new one
        if old_size > 0 {
            self.eviction_order.write().retain(|h| *h != key_hex);
        }

        // Write the new entry (reuses pre-serialized metadata)
        let new_size = self.write_entry(&key, &entry, &meta_json).await?;

        // Update size tracking
        let mut current_size = self.current_size.write();
        *current_size = current_size.saturating_sub(old_size) + new_size;

        // Track in eviction order
        self.eviction_order.write().push_back(key_hex);

        tracing::debug!(
            "Cached entry on disk: {} bytes, total: {}/{}",
            new_size,
            *current_size,
            self.max_size
        );

        Ok(())
    }

    pub async fn remove(&self, key: &CacheKey) -> Result<()> {
        // Acquire write_mutex to prevent racing with put()/evict_if_needed()
        let _guard = self.write_mutex.lock().await;
        let removed_size = self.remove_entry(key).await.unwrap_or(0);
        let key_hex = key.hash_hex();

        let mut current_size = self.current_size.write();
        *current_size = current_size.saturating_sub(removed_size);

        // Remove from eviction order to prevent ghost entries causing over-eviction
        self.eviction_order.write().retain(|h| *h != key_hex);

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

    #[tokio::test]
    async fn test_disk_cache_overwrite_no_ghost_in_eviction() {
        // Regression: overwriting a key used to leave a ghost entry in eviction_order.
        // When eviction popped the ghost, it deleted the live entry's files.
        let temp_dir = TempDir::new().unwrap();
        // Small cache: 10KB
        let cache = DiskCache::new(temp_dir.path().to_path_buf(), 10 * 1024)
            .await
            .unwrap();

        let key = CacheKey::new("GET", "http://example.com/overwrite", &[]);

        // Write entry, then overwrite with different data
        cache.put(key.clone(), create_entry(1024)).await.unwrap();
        cache.put(key.clone(), create_entry(2048)).await.unwrap();

        // Eviction order should have exactly 1 entry for this key, not 2
        {
            let order = cache.eviction_order.read();
            let count = order.iter().filter(|h| *h == &key.hash_hex()).count();
            assert_eq!(
                count, 1,
                "Expected 1 eviction entry, got {} (ghost entry present)",
                count
            );
        }

        // The entry should still be retrievable
        let retrieved = cache.get(&key).await.unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().data.len(), 2048);
    }

    #[tokio::test]
    async fn test_disk_cache_overwrite_survives_eviction() {
        // After overwriting a key, filling the cache should evict other entries
        // but NOT the overwritten entry's live files.
        let temp_dir = TempDir::new().unwrap();
        // 8KB cache — tight so we trigger eviction
        let cache = DiskCache::new(temp_dir.path().to_path_buf(), 8 * 1024)
            .await
            .unwrap();

        let key_a = CacheKey::new("GET", "http://example.com/a", &[]);
        let key_b = CacheKey::new("GET", "http://example.com/b", &[]);

        // Write A (1KB), then overwrite A (1KB again)
        cache.put(key_a.clone(), create_entry(1024)).await.unwrap();
        cache.put(key_a.clone(), create_entry(1024)).await.unwrap();

        // Write B large enough to trigger eviction
        cache
            .put(key_b.clone(), create_entry(6 * 1024))
            .await
            .unwrap();

        // A should still be retrievable (eviction should not have destroyed it via ghost)
        let retrieved_a = cache.get(&key_a).await.unwrap();
        assert!(
            retrieved_a.is_some(),
            "Overwritten entry A was destroyed by eviction via ghost entry"
        );
    }

    #[test]
    fn test_disk_cache_metadata_serde_forward_compat() {
        // Simulate a metadata JSON from a future version with an unknown field
        let json_with_extra = r#"{
            "headers": [["content-type", "text/plain"]],
            "status": 200,
            "timestamp": {"secs_since_epoch": 1700000000, "nanos_since_epoch": 0},
            "ttl_seconds": 3600,
            "data_size": 1024,
            "new_future_field": "should be ignored"
        }"#;
        let meta: DiskCacheMetadata = serde_json::from_str(json_with_extra).unwrap();
        assert_eq!(meta.status, 200);
        assert_eq!(meta.data_size, 1024);
    }

    #[test]
    fn test_disk_cache_metadata_serde_missing_field() {
        // Simulate a metadata JSON missing a field (e.g., old version without ttl_seconds)
        let json_missing_field = r#"{
            "headers": [],
            "status": 200,
            "timestamp": {"secs_since_epoch": 1700000000, "nanos_since_epoch": 0},
            "data_size": 512
        }"#;
        let meta: DiskCacheMetadata = serde_json::from_str(json_missing_field).unwrap();
        assert_eq!(meta.status, 200);
        assert_eq!(meta.ttl_seconds, 0); // default
        assert_eq!(meta.data_size, 512);
    }
}
