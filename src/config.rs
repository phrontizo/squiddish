use std::net::SocketAddr;
use std::path::PathBuf;

/// Parse size with units: B, KB, MB, GB, TB (case-insensitive)
/// Examples: "1GB", "512MB", "1024", "2.5GB"
fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_uppercase();

    // Try direct parse first (plain number)
    if let Ok(num) = s.parse::<u64>() {
        return Some(num);
    }

    // Parse with units
    let (num_str, unit) = if s.ends_with("TB") {
        (&s[..s.len()-2], 1024u64 * 1024 * 1024 * 1024)
    } else if s.ends_with("GB") {
        (&s[..s.len()-2], 1024u64 * 1024 * 1024)
    } else if s.ends_with("MB") {
        (&s[..s.len()-2], 1024u64 * 1024)
    } else if s.ends_with("KB") {
        (&s[..s.len()-2], 1024u64)
    } else if s.ends_with('B') {
        (&s[..s.len()-1], 1u64)
    } else {
        return None;
    };

    // Parse the numeric part (supports decimals)
    if let Ok(num) = num_str.trim().parse::<f64>() {
        Some((num * unit as f64) as u64)
    } else {
        None
    }
}

/// Parse duration with units: s, m, h, d (case-insensitive)
/// Examples: "5m", "2h", "7d", "300" (defaults to seconds)
fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();

    // Try direct parse first (plain number in seconds)
    if let Ok(num) = s.parse::<u64>() {
        return Some(num);
    }

    // Parse with units
    let (num_str, multiplier) = if s.ends_with('d') {
        (&s[..s.len()-1], 86400u64) // days
    } else if s.ends_with('h') {
        (&s[..s.len()-1], 3600u64) // hours
    } else if s.ends_with('m') {
        (&s[..s.len()-1], 60u64) // minutes
    } else if s.ends_with('s') {
        (&s[..s.len()-1], 1u64) // seconds
    } else {
        return None;
    };

    // Parse the numeric part
    if let Ok(num) = num_str.trim().parse::<u64>() {
        Some(num * multiplier)
    } else {
        None
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub cache: CacheConfig,
    pub apt: AptConfig,
    pub security: SecurityConfig,
}

#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Memory cache size in bytes (default: 1GB)
    pub memory_size: usize,

    /// Disk cache size in bytes (default: 100GB)
    pub disk_size: u64,

    /// Disk cache directory
    pub cache_dir: PathBuf,

    /// Enable compression for cached content
    pub compression: bool,

    /// TTL for cache entries in seconds (default: 7 days)
    pub ttl_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct AptConfig {
    /// Enable APT-specific optimizations
    pub enabled: bool,

    /// APT repositories to cache
    #[allow(dead_code)]
    pub repositories: Vec<String>,

    /// Cache package lists longer (default: 1 hour)
    pub list_ttl_seconds: u64,

    /// Cache .deb package files (default: 30 days)
    pub package_ttl_seconds: u64,

    /// Cache other APT files (default: 1 day)
    pub other_ttl_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct SecurityConfig {
    /// Maximum request body size (default: 10GB for large packages)
    pub max_body_size: u64,

    /// Maximum concurrent connections
    pub max_connections: usize,

    /// Request timeout in seconds
    pub timeout_seconds: u64,

    /// Enable strict HTTPS validation
    pub strict_https: bool,

    /// Allowed host patterns (empty = allow all)
    pub allowed_hosts: Vec<String>,

    /// Blocked host patterns
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

fn default_apt_package_ttl() -> u64 {
    30 * 24 * 60 * 60 // 30 days
}

fn default_apt_other_ttl() -> u64 {
    24 * 60 * 60 // 1 day
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

#[allow(dead_code)]
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
            package_ttl_seconds: default_apt_package_ttl(),
            other_ttl_seconds: default_apt_other_ttl(),
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
            if let Some(parsed) = parse_size(&size) {
                config.cache.memory_size = parsed as usize;
            }
        }

        if let Ok(size) = std::env::var("SQUIDDISH_DISK_SIZE") {
            if let Some(parsed) = parse_size(&size) {
                config.cache.disk_size = parsed;
            }
        }

        if let Ok(dir) = std::env::var("SQUIDDISH_CACHE_DIR") {
            config.cache.cache_dir = dir.into();
        }

        if let Ok(compression) = std::env::var("SQUIDDISH_COMPRESSION") {
            config.cache.compression = compression.parse().unwrap_or(true);
        }

        if let Ok(ttl) = std::env::var("SQUIDDISH_TTL") {
            if let Some(parsed) = parse_duration(&ttl) {
                config.cache.ttl_seconds = parsed;
            }
        }

        // APT config
        if let Ok(enabled) = std::env::var("SQUIDDISH_APT_ENABLED") {
            config.apt.enabled = enabled.parse().unwrap_or(true);
        }

        if let Ok(ttl) = std::env::var("SQUIDDISH_APT_LIST_TTL") {
            if let Some(parsed) = parse_duration(&ttl) {
                config.apt.list_ttl_seconds = parsed;
            }
        }

        if let Ok(ttl) = std::env::var("SQUIDDISH_APT_PACKAGE_TTL") {
            if let Some(parsed) = parse_duration(&ttl) {
                config.apt.package_ttl_seconds = parsed;
            }
        }

        if let Ok(ttl) = std::env::var("SQUIDDISH_APT_OTHER_TTL") {
            if let Some(parsed) = parse_duration(&ttl) {
                config.apt.other_ttl_seconds = parsed;
            }
        }

        // Security config
        if let Ok(size) = std::env::var("SQUIDDISH_MAX_BODY_SIZE") {
            if let Some(parsed) = parse_size(&size) {
                config.security.max_body_size = parsed;
            }
        }

        if let Ok(conns) = std::env::var("SQUIDDISH_MAX_CONNECTIONS") {
            if let Ok(parsed) = conns.parse() {
                config.security.max_connections = parsed;
            }
        }

        if let Ok(timeout) = std::env::var("SQUIDDISH_TIMEOUT") {
            if let Some(parsed) = parse_duration(&timeout) {
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
    fn test_env_var_config() {
        std::env::set_var("SQUIDDISH_BIND_ADDR", "0.0.0.0:8080");
        std::env::set_var("SQUIDDISH_MEMORY_SIZE", "2GB");

        let config = Config::from_env();
        assert_eq!(config.bind_addr.port(), 8080);
        assert_eq!(config.cache.memory_size, 2 * 1024 * 1024 * 1024);

        std::env::remove_var("SQUIDDISH_BIND_ADDR");
        std::env::remove_var("SQUIDDISH_MEMORY_SIZE");
    }

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("1KB"), Some(1024));
        assert_eq!(parse_size("1MB"), Some(1024 * 1024));
        assert_eq!(parse_size("1GB"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size("2.5GB"), Some((2.5 * 1024.0 * 1024.0 * 1024.0) as u64));
        assert_eq!(parse_size("1TB"), Some(1024u64 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("100mb"), Some(100 * 1024 * 1024)); // case insensitive
        assert_eq!(parse_size("invalid"), None);
    }

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("60"), Some(60));
        assert_eq!(parse_duration("5s"), Some(5));
        assert_eq!(parse_duration("5m"), Some(5 * 60));
        assert_eq!(parse_duration("2h"), Some(2 * 3600));
        assert_eq!(parse_duration("7d"), Some(7 * 86400));
        assert_eq!(parse_duration("1D"), Some(86400)); // case insensitive
        assert_eq!(parse_duration("invalid"), None);
    }
}
