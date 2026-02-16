use sha2::{Sha256, Digest};
use std::fmt;

/// Cache key based on request method, URI, and relevant headers
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    hash: [u8; 32],
    uri: String,
}

impl CacheKey {
    pub fn new(method: &str, uri: &str, vary_headers: &[(String, String)]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(method.as_bytes());
        hasher.update(b"\0");
        hasher.update(uri.as_bytes());

        // Include vary headers in cache key
        for (name, value) in vary_headers {
            hasher.update(b"\0");
            hasher.update(name.as_bytes());
            hasher.update(b":");
            hasher.update(value.as_bytes());
        }

        let hash = hasher.finalize().into();

        Self {
            hash,
            uri: uri.to_string(),
        }
    }

    #[allow(dead_code)]
    pub fn hash(&self) -> &[u8; 32] {
        &self.hash
    }

    pub fn hash_hex(&self) -> String {
        hex::encode(self.hash)
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// Get cache file path (sharded by first 2 bytes of hash for performance)
    pub fn file_path(&self, base_dir: &std::path::Path) -> std::path::PathBuf {
        let hex = self.hash_hex();
        let shard = &hex[0..2];
        let subshard = &hex[2..4];
        base_dir.join(shard).join(subshard).join(&hex)
    }
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.uri, self.hash_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_key_consistency() {
        let key1 = CacheKey::new("GET", "http://example.com/file", &[]);
        let key2 = CacheKey::new("GET", "http://example.com/file", &[]);
        assert_eq!(key1, key2);
        assert_eq!(key1.hash_hex(), key2.hash_hex());
    }

    #[test]
    fn test_cache_key_different_methods() {
        let key1 = CacheKey::new("GET", "http://example.com/file", &[]);
        let key2 = CacheKey::new("POST", "http://example.com/file", &[]);
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_cache_key_with_headers() {
        let headers = vec![
            ("Accept-Encoding".to_string(), "gzip".to_string()),
        ];
        let key1 = CacheKey::new("GET", "http://example.com/file", &headers);
        let key2 = CacheKey::new("GET", "http://example.com/file", &[]);
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_file_path_sharding() {
        let key = CacheKey::new("GET", "http://example.com/file", &[]);
        let path = key.file_path(std::path::Path::new("/cache"));
        let components: Vec<_> = path.components().collect();
        assert!(components.len() >= 4); // /cache/XX/YY/hash
    }
}
