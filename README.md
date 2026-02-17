# Squiddish

A high-performance HTTP caching proxy server written in Rust, optimized for package managers and content delivery.

This project was created for 2 reasons:

1. The existing apt-cache-ng wasn't working for me. I run quite a few Debian and Ubuntu VMs, and the caching just didn't
   seem efficient; not to mention apt-cache-ng kept crashing for reasons I couldn't understand.
2. I wanted to try out Claude for writing code and see how well it worked.

This proxy should work effectively for other purposes such as video streaming, but I have only tested it with deb
packages. At some point, I'll probably implement MITM support for HTTPS, but at the moment I don't need it.

## Features

- **Streaming Architecture**: Efficient memory usage with concurrent request deduplication
- **Two-Tier Cache**: Memory (LRU) + Disk persistence
- **APT-Aware Caching**: Intelligent TTL handling for Debian/Ubuntu packages
- **HTTP/2 Support**: For direct HTTP connections
- **CONNECT Tunneling**: HTTPS passthrough without interception
- **Configurable via Environment Variables**: No config files required

## Limitations

- **No HTTPS Interception**: Squiddish is not a MITM proxy. HTTPS traffic passes through via CONNECT tunneling without
  inspection or caching.
- **HTTP/2 over plaintext only**: ALPN negotiation requires TLS termination, which is not implemented.

## Installation

```bash
cargo build --release
```

## Usage

```bash
# Basic usage with defaults
./target/release/squiddish

# Custom configuration
SQUIDDISH_BIND_ADDR=0.0.0.0:8080 SQUIDDISH_DISK_SIZE=2GB ./target/release/squiddish
```

Configure your client to use `http://localhost:3128` as the HTTP proxy.

## Configuration

All configuration is done via environment variables:

### Server Settings

| Variable              | Default          | Description           |
|-----------------------|------------------|-----------------------|
| `SQUIDDISH_BIND_ADDR` | `127.0.0.1:3128` | Bind address and port |

### Cache Settings

| Variable                | Default   | Description                                        |
|-------------------------|-----------|----------------------------------------------------|
| `SQUIDDISH_CACHE_DIR`   | `./cache` | Disk cache directory                               |
| `SQUIDDISH_DISK_SIZE`   | `100GB`   | Max disk cache size (supports KB, MB, GB)          |
| `SQUIDDISH_MEMORY_SIZE` | `1GB`     | In-memory cache size                               |
| `SQUIDDISH_TTL`         | `7d`      | Default TTL for cached items (supports s, m, h, d) |
| `SQUIDDISH_COMPRESSION` | `true`    | Enable compression for cached data                 |

### APT-Specific Settings

APT requests are automatically detected and given optimized TTL values:

| Variable                    | Default | Description                                |
|-----------------------------|---------|--------------------------------------------|
| `SQUIDDISH_APT_ENABLED`     | `true`  | Enable APT-specific caching logic          |
| `SQUIDDISH_APT_PACKAGE_TTL` | `30d`   | TTL for `.deb` files (immutable)           |
| `SQUIDDISH_APT_LIST_TTL`    | `1h`    | TTL for package lists (frequently updated) |
| `SQUIDDISH_APT_OTHER_TTL`   | `1d`    | TTL for other APT files                    |

### Security Settings

| Variable                    | Default   | Description                                               |
|-----------------------------|-----------|-----------------------------------------------------------|
| `SQUIDDISH_MAX_BODY_SIZE`   | `10GB`    | Maximum response body size                                |
| `SQUIDDISH_MAX_CONNECTIONS` | `1000`    | Maximum concurrent connections                            |
| `SQUIDDISH_TIMEOUT`         | `5m`      | Request timeout                                           |
| `SQUIDDISH_STRICT_HTTPS`    | `true`    | Only allow CONNECT on port 443                            |
| `SQUIDDISH_ALLOWED_HOSTS`   | _(empty)_ | Comma-separated allowed host patterns (empty = allow all) |
| `SQUIDDISH_BLOCKED_HOSTS`   | _(empty)_ | Comma-separated blocked host patterns                     |

### Logging

| Variable   | Default | Description                                 |
|------------|---------|---------------------------------------------|
| `RUST_LOG` | `info`  | Log level (error, warn, info, debug, trace) |

## HTTP Headers

### Request Headers Respected

Squiddish respects standard HTTP caching headers:

- `Cache-Control`: Honors `no-cache`, `no-store`, `max-age`
- `Pragma: no-cache`: Bypasses cache (HTTP/1.0 compatibility)
- `Accept-Encoding`, `Accept`, `User-Agent`: Included in cache key for Vary support

### Response Headers Added

| Header        | Values        | Description                             |
|---------------|---------------|-----------------------------------------|
| `X-Cache`     | `HIT`, `MISS` | Indicates cache hit/miss                |
| `X-Cache-TTL` | Seconds       | Remaining TTL for cached items (on HIT) |

### Response Headers Preserved

- `Cache-Control`: Used to determine TTL (max-age, s-maxage)
- `Expires`: Fallback TTL calculation
- `Vary`: Respected for cache key generation (common headers: Accept-Encoding, Accept, User-Agent)
- All origin headers are preserved in cached responses

## Cache Behavior

### Vary Header Support

Squiddish automatically includes common request headers in the cache key when they are typically used with `Vary`
responses:

- `Accept-Encoding`: Different cache entries for gzip/br/identity
- `Accept`: Different cache entries for JSON/HTML/XML responses
- `User-Agent`: Different cache entries for mobile/desktop

This ensures that responses that vary based on these headers are cached separately and served correctly to different
clients.

### TTL Determination

1. **APT Requests** (auto-detected by URL patterns):
    - `.deb` files: 30 days (immutable packages)
    - Package lists (`Packages`, `InRelease`, etc.): 1 hour
    - Other APT files: 1 day

2. **Non-APT Requests**:
    - Respects `Cache-Control: max-age` or `s-maxage`
    - Falls back to `Expires` header
    - Uses default TTL if no cache headers present

3. **Cache Bypass**:
    - Responses with `Cache-Control: no-store` or `private`
    - Requests with `Cache-Control: no-cache` or `Pragma: no-cache`

### Streaming & Deduplication

When multiple clients request the same uncached resource:

- Only one upstream request is made
- Response is streamed to all waiting clients simultaneously
- Response is cached for future requests
- Efficient memory usage (no buffering entire response)

### Cache Storage

- **Memory Cache**: LRU eviction, fastest access
- **Disk Cache**: Persistent across restarts
- **Two-tier lookup**: Checks memory first, then disk
- **Automatic promotion**: Disk hits are promoted to memory

## APT Configuration Example

Configure APT to use Squiddish:

```bash
# /etc/apt/apt.conf.d/02proxy
Acquire::http::Proxy "http://127.0.0.1:3128";
```

## Development

### Running Tests

```bash
# All tests
cargo test

# Unit tests only
cargo test --lib

# Integration tests only
cargo test --test integration_test

# With logging
RUST_LOG=debug cargo test
```

### Project Structure

```
src/
├── main.rs              # Entry point, server setup
├── lib.rs               # Public module exports
├── config.rs            # Environment variable configuration
├── error.rs             # Error types
├── apt.rs               # APT request detection
├── cache/
│   ├── mod.rs           # Cache entry types
│   ├── key.rs           # Cache key generation
│   ├── memory.rs        # In-memory LRU cache
│   ├── disk.rs          # Disk-based cache
│   └── tiered.rs        # Two-tier cache coordinator
└── proxy/
    ├── mod.rs           # HTTP server setup
    ├── handler.rs       # Request handling & streaming
    ├── client.rs        # Upstream HTTP client
    └── tunnel.rs        # CONNECT tunnel handling

tests/
└── integration_test.rs  # Full proxy integration tests
```

## Performance Characteristics

- **Memory-efficient streaming**: No full response buffering
- **Concurrent request deduplication**: N clients = 1 upstream request
- **Fast cache lookups**: O(1) memory cache, optimized disk I/O
- **Async I/O**: Non-blocking throughout using Tokio
- **Multi-threaded runtime**: Tokio work-stealing scheduler uses all CPU cores
- **Zero-copy where possible**: Direct streaming from cache to client

### Threading Model

Squiddish uses Tokio's multi-threaded runtime with the following characteristics:

- **Default thread count**: Automatically uses all available CPU cores
- **Work-stealing scheduler**: Idle threads steal tasks from busy threads for optimal load balancing
- **Async task spawning**: Each TCP connection spawns a lightweight async task via `tokio::spawn()`
- **Non-blocking I/O**: All network operations are async, threads never block on I/O
- **Scalability**: Can handle thousands of concurrent connections with minimal threads

**Note**: Thread count is not configurable and defaults to the number of CPU cores. The async architecture means that
thread count doesn't limit connection capacity.