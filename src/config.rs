use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind_addr")]
    pub bind_addr: SocketAddr,

    #[serde(default)]
    pub cache: CacheConfig,

    #[serde(default)]
    pub apt: AptConfig,

    #[serde(default)]
    pub security: SecurityConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Memory cache size in bytes (default: 1GB)
    #[serde(default = "default_memory_cache_size")]
    pub memory_size: usize,

    /// Disk cache size in bytes (default: 100GB)
    #[serde(default = "default_disk_cache_size")]
    pub disk_size: u64,

    /// Disk cache directory
    #[serde(default = "default_cache_dir")]
    pub cache_dir: PathBuf,

    /// Enable compression for cached content
    #[serde(default = "default_true")]
    pub compression: bool,

    /// TTL for cache entries in seconds (default: 7 days)
    #[serde(default = "default_cache_ttl")]
    pub ttl_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AptConfig {
    /// Enable APT-specific optimizations
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// APT repositories to cache
    #[serde(default)]
    pub repositories: Vec<String>,

    /// Cache package lists longer (default: 1 hour)
    #[serde(default = "default_apt_list_ttl")]
    pub list_ttl_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Maximum request body size (default: 10GB for large packages)
    #[serde(default = "default_max_body_size")]
    pub max_body_size: u64,

    /// Maximum concurrent connections
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,

    /// Request timeout in seconds
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,

    /// Enable strict HTTPS validation
    #[serde(default = "default_true")]
    pub strict_https: bool,

    /// Allowed host patterns (empty = allow all)
    #[serde(default)]
    pub allowed_hosts: Vec<String>,

    /// Blocked host patterns
    #[serde(default)]
    pub blocked_hosts: Vec<String>,
}

fn default_bind_addr() -> SocketAddr {
    "127.0.0.1:3128".parse().unwrap()
}

fn default_memory_cache_size() -> usize {
    1024 * 1024 * 1024 // 1GB
}

fn default_disk_cache_size() -> u64 {
    100 * 1024 * 1024 * 1024 // 100GB
}

fn default_cache_dir() -> PathBuf {
    PathBuf::from("./cache")
}

fn default_cache_ttl() -> u64 {
    7 * 24 * 60 * 60 // 7 days
}

fn default_apt_list_ttl() -> u64 {
    60 * 60 // 1 hour
}

fn default_max_body_size() -> u64 {
    10 * 1024 * 1024 * 1024 // 10GB
}

fn default_max_connections() -> usize {
    1000
}

fn default_timeout() -> u64 {
    300 // 5 minutes
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: default_bind_addr(),
            cache: CacheConfig::default(),
            apt: AptConfig::default(),
            security: SecurityConfig::default(),
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            memory_size: default_memory_cache_size(),
            disk_size: default_disk_cache_size(),
            cache_dir: default_cache_dir(),
            compression: true,
            ttl_seconds: default_cache_ttl(),
        }
    }
}

impl Default for AptConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            repositories: vec![],
            list_ttl_seconds: default_apt_list_ttl(),
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            max_body_size: default_max_body_size(),
            max_connections: default_max_connections(),
            timeout_seconds: default_timeout(),
            strict_https: true,
            allowed_hosts: vec![],
            blocked_hosts: vec![],
        }
    }
}

impl Config {
    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        Ok(config)
    }

    /// Load config from environment variables with SQUIDDISH_ prefix
    pub fn from_env() -> Self {
        let mut config = Self::default();

        // Bind address
        if let Ok(addr) = std::env::var("SQUIDDISH_BIND_ADDR") {
            if let Ok(parsed) = addr.parse() {
                config.bind_addr = parsed;
            }
        }

        // Cache config
        if let Ok(size) = std::env::var("SQUIDDISH_MEMORY_SIZE") {
            if let Ok(parsed) = size.parse() {
                config.cache.memory_size = parsed;
            }
        }

        if let Ok(size) = std::env::var("SQUIDDISH_DISK_SIZE") {
            if let Ok(parsed) = size.parse() {
                config.cache.disk_size = parsed;
            }
        }

        if let Ok(dir) = std::env::var("SQUIDDISH_CACHE_DIR") {
            config.cache.cache_dir = dir.into();
        }

        if let Ok(compression) = std::env::var("SQUIDDISH_COMPRESSION") {
            config.cache.compression = compression.parse().unwrap_or(true);
        }

        if let Ok(ttl) = std::env::var("SQUIDDISH_TTL_SECONDS") {
            if let Ok(parsed) = ttl.parse() {
                config.cache.ttl_seconds = parsed;
            }
        }

        // APT config
        if let Ok(enabled) = std::env::var("SQUIDDISH_APT_ENABLED") {
            config.apt.enabled = enabled.parse().unwrap_or(true);
        }

        if let Ok(ttl) = std::env::var("SQUIDDISH_APT_LIST_TTL") {
            if let Ok(parsed) = ttl.parse() {
                config.apt.list_ttl_seconds = parsed;
            }
        }

        // Security config
        if let Ok(size) = std::env::var("SQUIDDISH_MAX_BODY_SIZE") {
            if let Ok(parsed) = size.parse() {
                config.security.max_body_size = parsed;
            }
        }

        if let Ok(conns) = std::env::var("SQUIDDISH_MAX_CONNECTIONS") {
            if let Ok(parsed) = conns.parse() {
                config.security.max_connections = parsed;
            }
        }

        if let Ok(timeout) = std::env::var("SQUIDDISH_TIMEOUT_SECONDS") {
            if let Ok(parsed) = timeout.parse() {
                config.security.timeout_seconds = parsed;
            }
        }

        if let Ok(strict) = std::env::var("SQUIDDISH_STRICT_HTTPS") {
            config.security.strict_https = strict.parse().unwrap_or(true);
        }

        if let Ok(hosts) = std::env::var("SQUIDDISH_ALLOWED_HOSTS") {
            config.security.allowed_hosts = hosts
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }

        if let Ok(hosts) = std::env::var("SQUIDDISH_BLOCKED_HOSTS") {
            config.security.blocked_hosts = hosts
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }

        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.bind_addr.port(), 3128);
        assert_eq!(config.cache.memory_size, 1024 * 1024 * 1024);
        assert!(config.apt.enabled);
        assert!(config.security.strict_https);
    }

    #[test]
    fn test_config_serialization() {
        let config = Config::default();
        let serialized = toml::to_string(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(config.bind_addr, deserialized.bind_addr);
    }
}
