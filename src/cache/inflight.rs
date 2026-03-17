use bytes::Bytes;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, watch};

use super::CacheKey;

/// Represents a chunk of data in the download stream
#[derive(Debug, Clone)]
pub enum DownloadChunk {
    Data(Bytes),
    Complete,
    Error(String),
}

/// Response metadata (status + headers) for in-flight downloads.
/// Shared with joiners so they receive correct response headers.
#[derive(Debug, Clone)]
pub struct ResponseMeta {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

/// Manages in-flight downloads to handle concurrent requests for the same resource
pub struct InflightDownloads {
    downloads: Arc<RwLock<HashMap<String, DownloadState>>>,
}

/// Result type for response metadata: Ok with headers, or Err with error message.
pub type ResponseMetaResult = Result<ResponseMeta, String>;

struct DownloadState {
    sender: broadcast::Sender<DownloadChunk>,
    accumulated: Arc<RwLock<Vec<Bytes>>>,
    meta_tx: watch::Sender<Option<ResponseMetaResult>>,
}

/// Result of an atomic join-or-start operation on an in-flight download.
pub enum DownloadAction {
    /// Joined an existing download: (broadcast receiver, accumulated chunks, meta receiver)
    Joined(
        broadcast::Receiver<DownloadChunk>,
        Vec<Bytes>,
        watch::Receiver<Option<ResponseMetaResult>>,
    ),
    /// Started a new download: (broadcast sender, meta receiver)
    Started(
        broadcast::Sender<DownloadChunk>,
        watch::Receiver<Option<ResponseMetaResult>>,
    ),
}

impl Default for InflightDownloads {
    fn default() -> Self {
        Self {
            downloads: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl InflightDownloads {
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically join an existing download or start a new one.
    /// Holds a single write lock across the check-and-insert to prevent
    /// TOCTOU races where two callers both see "no download" and both start one.
    pub fn join_or_start_download(&self, key: &CacheKey) -> DownloadAction {
        let key_str = key.hash_hex();
        let mut downloads = self.downloads.write();

        if let Some(state) = downloads.get(&key_str) {
            // Subscribe first, then read accumulated.
            // The write lock blocks add_chunk (which needs a read lock),
            // so no chunks can arrive between subscribe and accumulated read.
            let receiver = state.sender.subscribe();
            let accumulated = state.accumulated.read().clone();
            let meta_rx = state.meta_tx.subscribe();
            tracing::debug!(
                "Joining existing download for {}, already have {} chunks",
                key_str,
                accumulated.len()
            );
            DownloadAction::Joined(receiver, accumulated, meta_rx)
        } else {
            let (sender, _) = broadcast::channel(1000);
            let (meta_tx, meta_rx) = watch::channel(None);
            let state = DownloadState {
                sender: sender.clone(),
                accumulated: Arc::new(RwLock::new(Vec::new())),
                meta_tx,
            };
            downloads.insert(key_str.clone(), state);
            tracing::debug!("Started new download for {}", key_str);
            DownloadAction::Started(sender, meta_rx)
        }
    }

    /// Check if a download is in progress and subscribe to it.
    /// Superseded by `join_or_start_download` in production; retained for tests.
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    pub fn join_download(
        &self,
        key: &CacheKey,
    ) -> Option<(
        broadcast::Receiver<DownloadChunk>,
        Vec<Bytes>,
        watch::Receiver<Option<ResponseMetaResult>>,
    )> {
        let key_str = key.hash_hex();
        let downloads = self.downloads.write();

        if let Some(state) = downloads.get(&key_str) {
            let receiver = state.sender.subscribe();
            let accumulated = state.accumulated.read().clone();
            let meta_rx = state.meta_tx.subscribe();
            tracing::debug!(
                "Joining existing download for {}, already have {} chunks",
                key_str,
                accumulated.len()
            );
            Some((receiver, accumulated, meta_rx))
        } else {
            None
        }
    }

    /// Register a new download and get a sender to broadcast chunks.
    /// Superseded by `join_or_start_download` in production; retained for tests.
    #[cfg(test)]
    pub fn start_download(
        &self,
        key: &CacheKey,
    ) -> (
        broadcast::Sender<DownloadChunk>,
        watch::Receiver<Option<ResponseMetaResult>>,
    ) {
        let key_str = key.hash_hex();
        let mut downloads = self.downloads.write();

        let (sender, _) = broadcast::channel(1000);
        let (meta_tx, meta_rx) = watch::channel(None);
        let state = DownloadState {
            sender: sender.clone(),
            accumulated: Arc::new(RwLock::new(Vec::new())),
            meta_tx,
        };
        downloads.insert(key_str.clone(), state);

        tracing::debug!("Started new download for {}", key_str);
        (sender, meta_rx)
    }

    /// Set response metadata (status + headers) for an in-flight download.
    /// Called when upstream response headers are received.
    pub fn set_response_meta(&self, key: &CacheKey, meta: ResponseMeta) {
        let key_str = key.hash_hex();
        let downloads = self.downloads.read();
        if let Some(state) = downloads.get(&key_str) {
            let _ = state.meta_tx.send(Some(Ok(meta)));
        }
    }

    /// Signal that the download failed before response headers were received.
    /// Unblocks waiters with the actual error message.
    pub fn set_response_error(&self, key: &CacheKey, error: String) {
        let key_str = key.hash_hex();
        let downloads = self.downloads.read();
        if let Some(state) = downloads.get(&key_str) {
            let _ = state.meta_tx.send(Some(Err(error)));
        }
    }

    /// Add a chunk to the accumulated data and broadcast it to all subscribers.
    /// Both operations happen under the same read lock on `downloads`, making
    /// them atomic with respect to `join_or_start_download` (which holds a write
    /// lock while subscribing + reading accumulated). This prevents a race where
    /// a joiner could receive the same chunk from both accumulated and broadcast.
    pub fn add_chunk(&self, key: &CacheKey, chunk: Bytes) {
        let key_str = key.hash_hex();
        let downloads = self.downloads.read();

        if let Some(state) = downloads.get(&key_str) {
            state.accumulated.write().push(chunk.clone());
            let _ = state.sender.send(DownloadChunk::Data(chunk));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> CacheKey {
        CacheKey::new("GET", "http://example.com/file.deb", &[])
    }

    #[test]
    fn test_start_and_join_download() {
        let inflight = InflightDownloads::new();
        let key = test_key();

        // No download in progress
        assert!(inflight.join_download(&key).is_none());

        // Start download
        let (_sender, _meta_rx) = inflight.start_download(&key);

        // Now joining should succeed
        let result = inflight.join_download(&key);
        assert!(result.is_some());
    }

    #[test]
    fn test_join_or_start_atomicity() {
        let inflight = InflightDownloads::new();
        let key = test_key();

        // First call should start a new download
        let action1 = inflight.join_or_start_download(&key);
        assert!(matches!(action1, DownloadAction::Started(_, _)));

        // Second call for same key should join
        let action2 = inflight.join_or_start_download(&key);
        assert!(matches!(action2, DownloadAction::Joined(_, _, _)));

        // After completion, next call should start a new one
        inflight.complete_download(&key);
        let action3 = inflight.join_or_start_download(&key);
        assert!(matches!(action3, DownloadAction::Started(_, _)));
    }

    #[tokio::test]
    async fn test_response_meta_propagates_to_joiner() {
        let inflight = InflightDownloads::new();
        let key = test_key();

        // Start download
        let (_sender, _meta_rx) = inflight.start_download(&key);

        // Join the download
        let (_receiver, _chunks, mut joiner_meta_rx) = inflight.join_download(&key).unwrap();

        // Initially no metadata
        assert!(joiner_meta_rx.borrow().is_none());

        // Set response metadata
        inflight.set_response_meta(
            &key,
            ResponseMeta {
                status: 200,
                headers: vec![
                    (
                        "content-type".to_string(),
                        "application/octet-stream".to_string(),
                    ),
                    ("x-custom".to_string(), "value".to_string()),
                ],
            },
        );

        // Joiner should now see the metadata
        joiner_meta_rx.changed().await.unwrap();
        let meta = joiner_meta_rx.borrow().clone().unwrap().unwrap();
        assert_eq!(meta.status, 200);
        assert_eq!(meta.headers.len(), 2);
        assert_eq!(
            meta.headers[0],
            (
                "content-type".to_string(),
                "application/octet-stream".to_string()
            )
        );
    }

    #[tokio::test]
    async fn test_response_error_propagates_to_joiner() {
        let inflight = InflightDownloads::new();
        let key = test_key();

        let (_sender, _meta_rx) = inflight.start_download(&key);
        let (_receiver, _chunks, mut joiner_meta_rx) = inflight.join_download(&key).unwrap();

        // Signal an error
        inflight.set_response_error(&key, "DNS lookup failed: evil.example.com".to_string());

        // Joiner should see the error with the actual message
        joiner_meta_rx.changed().await.unwrap();
        let result = joiner_meta_rx.borrow().clone().unwrap();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "DNS lookup failed: evil.example.com");
    }

    #[test]
    fn test_complete_download_removes_entry() {
        let inflight = InflightDownloads::new();
        let key = test_key();

        let (_sender, _meta_rx) = inflight.start_download(&key);
        assert!(inflight.join_download(&key).is_some());

        inflight.complete_download(&key);
        assert!(inflight.join_download(&key).is_none());
    }
}
