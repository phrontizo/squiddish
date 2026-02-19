use bytes::Bytes;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;

use super::CacheKey;

/// Represents a chunk of data in the download stream
#[derive(Debug, Clone)]
pub enum DownloadChunk {
    Data(Bytes),
    Complete,
    Error(String),
}

/// Manages in-flight downloads to handle concurrent requests for the same resource
pub struct InflightDownloads {
    downloads: Arc<RwLock<HashMap<String, DownloadState>>>,
}

struct DownloadState {
    sender: broadcast::Sender<DownloadChunk>,
    accumulated: Arc<RwLock<Vec<Bytes>>>,
}

impl InflightDownloads {
    pub fn new() -> Self {
        Self {
            downloads: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Check if a download is in progress and subscribe to it.
    /// Uses a write lock to prevent add_chunk from running between subscribe
    /// and accumulated read, which would cause duplicate data in the stream.
    pub fn join_download(&self, key: &CacheKey) -> Option<(broadcast::Receiver<DownloadChunk>, Vec<Bytes>)> {
        let key_str = key.hash_hex();
        let downloads = self.downloads.write();

        if let Some(state) = downloads.get(&key_str) {
            // Subscribe first, then read accumulated.
            // The write lock blocks add_chunk (which needs a read lock),
            // so no chunks can arrive between subscribe and accumulated read.
            let receiver = state.sender.subscribe();
            let accumulated = state.accumulated.read().clone();
            tracing::debug!("Joining existing download for {}, already have {} chunks", key_str, accumulated.len());
            Some((receiver, accumulated))
        } else {
            None
        }
    }

    /// Register a new download and get a sender to broadcast chunks
    pub fn start_download(&self, key: &CacheKey) -> broadcast::Sender<DownloadChunk> {
        let key_str = key.hash_hex();
        let mut downloads = self.downloads.write();

        // Create a broadcast channel with reasonable buffer (1000 chunks for large files)
        let (sender, _) = broadcast::channel(1000);
        let state = DownloadState {
            sender: sender.clone(),
            accumulated: Arc::new(RwLock::new(Vec::new())),
        };
        downloads.insert(key_str.clone(), state);

        tracing::debug!("Started new download for {}", key_str);
        sender
    }

    /// Add a chunk to the accumulated data (for late joiners)
    pub fn add_chunk(&self, key: &CacheKey, chunk: Bytes) {
        let key_str = key.hash_hex();
        let downloads = self.downloads.read();

        if let Some(state) = downloads.get(&key_str) {
            state.accumulated.write().push(chunk);
        }
    }

    /// Mark a download as complete and remove it from tracking
    pub fn complete_download(&self, key: &CacheKey) {
        let key_str = key.hash_hex();
        let mut downloads = self.downloads.write();
        downloads.remove(&key_str);
        tracing::debug!("Completed download for {}", key_str);
    }
}
