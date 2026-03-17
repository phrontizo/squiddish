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
    let (num_str, unit) = if let Some(n) = s.strip_suffix("TB") {
        (n, 1024u64 * 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("GB") {
        (n, 1024u64 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("MB") {
        (n, 1024u64 * 1024)
    } else if let Some(n) = s.strip_suffix("KB") {
        (n, 1024u64)
    } else if let Some(n) = s.strip_suffix('B') {
        (n, 1u64)
    } else {
        return None;
    };

    // Parse the numeric part (supports decimals, rejects negative/infinite values)
    if let Ok(num) = num_str.trim().parse::<f64>() {
        if num >= 0.0 && num.is_finite() {
            Some((num * unit as f64) as u64)
        } else {
            None
        }
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
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('d') {
        (n, 86400u64)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600u64)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60u64)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1u64)
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

    /// TTL for cache entries in seconds (default: 7 days)
    pub ttl_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct AptConfig {
    /// Enable APT-specific optimizations
    pub enabled: bool,

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

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:3128".parse().unwrap(),
            cache: CacheConfig::default(),
            apt: AptConfig::default(),
            security: SecurityConfig::default(),
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            memory_size: 1024 * 1024 * 1024,     // 1GB
            disk_size: 100 * 1024 * 1024 * 1024, // 100GB
            cache_dir: PathBuf::from("./cache"),
            ttl_seconds: 7 * 24 * 60 * 60, // 7 days
        }
    }
}

impl Default for AptConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            list_ttl_seconds: 60 * 60,              // 1 hour
            package_ttl_seconds: 30 * 24 * 60 * 60, // 30 days
            other_ttl_seconds: 24 * 60 * 60,        // 1 day
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            max_body_size: 10 * 1024 * 1024 * 1024, // 10GB
            max_connections: 1000,
            timeout_seconds: 300, // 5 minutes
            strict_https: true,
            allowed_hosts: vec![],
            blocked_hosts: vec![],
        }
    }
}

impl Config {
    /// Load config from environment variables with SQUIDDISH_ prefix.
    /// Returns an error if any set variable has an invalid value.
    pub fn from_env() -> Result<Self, String> {
        Self::from_vars(|key| std::env::var(key).ok())
    }

    /// Load config from a variable lookup function.
    /// Returns an error if any provided variable has an invalid value.
    pub fn from_vars(get_var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mut config = Self::default();

        // Bind address
        if let Some(addr) = get_var("SQUIDDISH_BIND_ADDR") {
            config.bind_addr = addr
                .parse()
                .map_err(|e| format!("Invalid SQUIDDISH_BIND_ADDR '{}': {}", addr, e))?;
        }

        // Cache config
        if let Some(size) = get_var("SQUIDDISH_MEMORY_SIZE") {
            let bytes = parse_size(&size)
                .ok_or_else(|| format!("Invalid SQUIDDISH_MEMORY_SIZE '{}': expected number with optional unit (KB, MB, GB, TB)", size))?;
            config.cache.memory_size = usize::try_from(bytes).map_err(|_| {
                format!(
                    "SQUIDDISH_MEMORY_SIZE '{}' exceeds platform address space",
                    size
                )
            })?;
        }

        if let Some(size) = get_var("SQUIDDISH_DISK_SIZE") {
            config.cache.disk_size = parse_size(&size)
                .ok_or_else(|| format!("Invalid SQUIDDISH_DISK_SIZE '{}': expected number with optional unit (KB, MB, GB, TB)", size))?;
        }

        if let Some(dir) = get_var("SQUIDDISH_CACHE_DIR") {
            config.cache.cache_dir = dir.into();
        }

        if let Some(ttl) = get_var("SQUIDDISH_TTL") {
            config.cache.ttl_seconds = parse_duration(&ttl).ok_or_else(|| {
                format!(
                    "Invalid SQUIDDISH_TTL '{}': expected number with optional unit (s, m, h, d)",
                    ttl
                )
            })?;
        }

        // APT config
        if let Some(enabled) = get_var("SQUIDDISH_APT_ENABLED") {
            config.apt.enabled = enabled
                .parse()
                .map_err(|e| format!("Invalid SQUIDDISH_APT_ENABLED '{}': {}", enabled, e))?;
        }

        if let Some(ttl) = get_var("SQUIDDISH_APT_LIST_TTL") {
            config.apt.list_ttl_seconds = parse_duration(&ttl)
                .ok_or_else(|| format!("Invalid SQUIDDISH_APT_LIST_TTL '{}': expected number with optional unit (s, m, h, d)", ttl))?;
        }

        if let Some(ttl) = get_var("SQUIDDISH_APT_PACKAGE_TTL") {
            config.apt.package_ttl_seconds = parse_duration(&ttl)
                .ok_or_else(|| format!("Invalid SQUIDDISH_APT_PACKAGE_TTL '{}': expected number with optional unit (s, m, h, d)", ttl))?;
        }

        if let Some(ttl) = get_var("SQUIDDISH_APT_OTHER_TTL") {
            config.apt.other_ttl_seconds = parse_duration(&ttl)
                .ok_or_else(|| format!("Invalid SQUIDDISH_APT_OTHER_TTL '{}': expected number with optional unit (s, m, h, d)", ttl))?;
        }

        // Security config
        if let Some(size) = get_var("SQUIDDISH_MAX_BODY_SIZE") {
            config.security.max_body_size = parse_size(&size)
                .ok_or_else(|| format!("Invalid SQUIDDISH_MAX_BODY_SIZE '{}': expected number with optional unit (KB, MB, GB, TB)", size))?;
        }

        if let Some(conns) = get_var("SQUIDDISH_MAX_CONNECTIONS") {
            config.security.max_connections = conns
                .parse()
                .map_err(|e| format!("Invalid SQUIDDISH_MAX_CONNECTIONS '{}': {}", conns, e))?;
        }

        if let Some(timeout) = get_var("SQUIDDISH_TIMEOUT") {
            config.security.timeout_seconds = parse_duration(&timeout)
                .ok_or_else(|| format!("Invalid SQUIDDISH_TIMEOUT '{}': expected number with optional unit (s, m, h, d)", timeout))?;
        }

        if let Some(strict) = get_var("SQUIDDISH_STRICT_HTTPS") {
            config.security.strict_https = strict
                .parse()
                .map_err(|e| format!("Invalid SQUIDDISH_STRICT_HTTPS '{}': {}", strict, e))?;
        }

        if let Some(hosts) = get_var("SQUIDDISH_ALLOWED_HOSTS") {
            config.security.allowed_hosts = hosts
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }

        if let Some(hosts) = get_var("SQUIDDISH_BLOCKED_HOSTS") {
            config.security.blocked_hosts = hosts
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }

        // Validate that critical parameters are not zero
        if config.cache.memory_size == 0 {
            return Err("SQUIDDISH_MEMORY_SIZE must be greater than 0".to_string());
        }
        if config.cache.disk_size == 0 {
            return Err("SQUIDDISH_DISK_SIZE must be greater than 0".to_string());
        }
        if config.security.max_body_size == 0 {
            return Err("SQUIDDISH_MAX_BODY_SIZE must be greater than 0".to_string());
        }
        if config.security.max_connections == 0 {
            return Err("SQUIDDISH_MAX_CONNECTIONS must be greater than 0".to_string());
        }
        if config.security.timeout_seconds == 0 {
            return Err("SQUIDDISH_TIMEOUT must be greater than 0".to_string());
        }

        Ok(config)
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
    fn test_from_vars_valid() {
        let config = Config::from_vars(|key| match key {
            "SQUIDDISH_BIND_ADDR" => Some("0.0.0.0:8080".to_string()),
            "SQUIDDISH_MEMORY_SIZE" => Some("2GB".to_string()),
            "SQUIDDISH_STRICT_HTTPS" => Some("false".to_string()),
            _ => None,
        })
        .unwrap();

        assert_eq!(config.bind_addr.port(), 8080);
        assert_eq!(config.cache.memory_size, 2 * 1024 * 1024 * 1024);
        assert!(!config.security.strict_https);
    }

    #[test]
    fn test_from_vars_invalid_bind_addr() {
        let result = Config::from_vars(|key| match key {
            "SQUIDDISH_BIND_ADDR" => Some("garbage".to_string()),
            _ => None,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SQUIDDISH_BIND_ADDR"));
    }

    #[test]
    fn test_from_vars_invalid_memory_size() {
        let result = Config::from_vars(|key| match key {
            "SQUIDDISH_MEMORY_SIZE" => Some("not_a_size".to_string()),
            _ => None,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SQUIDDISH_MEMORY_SIZE"));
    }

    #[test]
    fn test_from_vars_invalid_bool() {
        let result = Config::from_vars(|key| match key {
            "SQUIDDISH_APT_ENABLED" => Some("maybe".to_string()),
            _ => None,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SQUIDDISH_APT_ENABLED"));
    }

    #[test]
    fn test_from_vars_zero_max_connections() {
        let result = Config::from_vars(|key| match key {
            "SQUIDDISH_MAX_CONNECTIONS" => Some("0".to_string()),
            _ => None,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("greater than 0"));
    }

    #[test]
    fn test_from_vars_zero_timeout() {
        let result = Config::from_vars(|key| match key {
            "SQUIDDISH_TIMEOUT" => Some("0".to_string()),
            _ => None,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("greater than 0"));
    }

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("1KB"), Some(1024));
        assert_eq!(parse_size("1MB"), Some(1024 * 1024));
        assert_eq!(parse_size("1GB"), Some(1024 * 1024 * 1024));
        assert_eq!(
            parse_size("2.5GB"),
            Some((2.5 * 1024.0 * 1024.0 * 1024.0) as u64)
        );
        assert_eq!(parse_size("1TB"), Some(1024u64 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("100mb"), Some(100 * 1024 * 1024)); // case insensitive
        assert_eq!(parse_size("invalid"), None);
        // Negative and infinite values should be rejected
        assert_eq!(parse_size("-1GB"), None);
        assert_eq!(parse_size("-100MB"), None);
        assert_eq!(parse_size("infGB"), None);
        assert_eq!(parse_size("infinityMB"), None);
    }

    #[test]
    fn test_from_vars_zero_memory_size() {
        let result = Config::from_vars(|key| match key {
            "SQUIDDISH_MEMORY_SIZE" => Some("0".to_string()),
            _ => None,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("greater than 0"));
    }

    #[test]
    fn test_from_vars_zero_disk_size() {
        let result = Config::from_vars(|key| match key {
            "SQUIDDISH_DISK_SIZE" => Some("0".to_string()),
            _ => None,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("greater than 0"));
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
