use bytes::Bytes;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;

use super::CacheKey;

/// Manages in-flight downloads to handle concurrent requests for the same resource
pub struct InflightDownloads {
    downloads: Arc<RwLock<HashMap<String, broadcast::Sender<Result<Bytes, String>>>>>,
}

impl InflightDownloads {
    pub fn new() -> Self {
        Self {
            downloads: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Try to register a new download. Returns None if download is already in progress,
    /// or Some(receiver) if this is a new download that should be started.
    pub fn start_download(&self, key: &CacheKey) -> Option<broadcast::Receiver<Result<Bytes, String>>> {
        let key_str = key.hash_hex();
        let mut downloads = self.downloads.write();

        if let Some(sender) = downloads.get(&key_str) {
            // Download already in progress, subscribe to it
            Some(sender.subscribe())
        } else {
            // No download in progress, return None to signal caller should start one
            None
        }
    }

    /// Register that we're starting a download and get a sender to broadcast chunks
    pub fn register_download(&self, key: &CacheKey) -> broadcast::Sender<Result<Bytes, String>> {
        let key_str = key.hash_hex();
        let mut downloads = self.downloads.write();

        // Create a broadcast channel with reasonable buffer (100 chunks)
        let (sender, _) = broadcast::channel(100);
        downloads.insert(key_str.clone(), sender.clone());

        sender
    }

    /// Mark a download as complete and remove it from tracking
    pub fn complete_download(&self, key: &CacheKey) {
        let key_str = key.hash_hex();
        let mut downloads = self.downloads.write();
        downloads.remove(&key_str);
    }
}
