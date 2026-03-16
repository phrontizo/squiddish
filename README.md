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
- **Two-Tier Cache**: Memory (moka/TinyLFU) + Disk persistence
- **APT-Aware Caching**: Intelligent TTL handling for Debian/Ubuntu packages
- **HTTP/1.1 Support**: With connection keep-alive and header case preservation
- **CONNECT Tunneling**: HTTPS passthrough without interception
- **Graceful Shutdown**: Clean shutdown on SIGINT with in-flight request draining
- **Connection Limiting**: Semaphore-based max concurrent connections
- **Configurable via Environment Variables**: No config files required

## Limitations

- **No HTTPS Interception**: Squiddish is not a MITM proxy. HTTPS traffic passes through via CONNECT tunneling without
  inspection or caching.
- **HTTP/1.1 only**: HTTP/2 is not supported (would require TLS termination for ALPN negotiation).

## Installation

```bash
cargo build --release
```

## Docker

```bash
# Build and run with Docker Compose
docker compose up -d

# Or build the image directly
docker build -t squiddish .
docker run -d -p 3128:3128 -v ./cache:/cache squiddish
```

The Docker image uses a multi-stage build with musl for static linking, producing a minimal scratch-based image.
Supports multi-arch: `linux/amd64` and `linux/arm64`.

## Usage

```bash
# Basic usage with defaults
./target/release/squiddish

# Custom configuration
SQUIDDISH_BIND_ADDR=0.0.0.0:8080 SQUIDDISH_DISK_SIZE=2GB ./target/release/squiddish
```

Configure your client to use `http://localhost:3128` as the HTTP proxy.

**Note**: Invalid configuration values cause the process to exit with a descriptive error message.

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

### Response Headers Added

| Header        | Values        | Description                             |
|---------------|---------------|-----------------------------------------|
| `X-Cache`     | `HIT`         | Present on cache hits (absent on miss)  |
| `X-Cache-TTL` | Seconds       | Remaining TTL for cached items (on HIT) |
| `Via`         | `1.1 squiddish` | Standard proxy identification header |

### Cache Behavior

Squiddish respects standard HTTP caching semantics:

- **`Cache-Control: s-maxage`** takes precedence over `max-age` (RFC 7234 shared cache behavior)
- **`Cache-Control: no-store`, `no-cache`, `private`** bypass caching entirely
- **`Pragma: no-cache`** respected for HTTP/1.0 compatibility
- **`Expires`** header used as fallback when no `Cache-Control` is present
- **Non-2xx responses** are never cached
- **`Accept-Encoding`** is included in the cache key to serve correct content variants

### Host Filtering

Host patterns use domain suffix matching: pattern `example.com` matches `example.com` and `sub.example.com`
but NOT `evil-example.com`.

### TTL Determination

1. **APT Requests** (auto-detected by URL patterns):
    - `.deb` files: 30 days (immutable packages)
    - Package lists (`Packages`, `InRelease`, etc.): 1 hour
    - Other APT files: 1 day

2. **Non-APT Requests**:
    - Respects `Cache-Control: s-maxage` (highest priority)
    - Falls back to `Cache-Control: max-age`
    - Falls back to `Expires` header
    - Uses default TTL if no cache headers present

### Streaming & Deduplication

When multiple clients request the same uncached resource:

- Only one upstream request is made
- Response is streamed to all waiting clients simultaneously via broadcast channels
- Late joiners receive accumulated chunks before joining the live stream
- Response is cached after the download completes

### Cache Storage

- **Memory Cache**: moka concurrent cache with TinyLFU admission policy, weighted by entry size
- **Disk Cache**: Persistent across restarts, sharded by content hash, VecDeque-based eviction
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
# Run all tests
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
├── main.rs              # Entry point, logging setup, config loading
├── lib.rs               # Public module exports
├── config.rs            # Environment variable configuration with validation
├── error.rs             # Error types (thiserror)
├── apt.rs               # APT request detection and categorization
├── cache/
│   ├── mod.rs           # CacheEntry, TieredCache coordinator
│   ├── key.rs           # SHA-256 cache key generation with sharding
│   ├── memory.rs        # In-memory cache (moka concurrent cache)
│   ├── disk.rs          # Disk-based cache with VecDeque eviction
│   └── inflight.rs      # In-flight download deduplication (broadcast channels)
└── proxy/
    ├── mod.rs           # ProxyServer, TCP accept loop, connection limiting, graceful shutdown
    ├── handler.rs       # Request routing, caching logic, streaming downloads
    ├── client.rs        # Upstream HTTP client (hyper-util)
    ├── tunnel.rs        # CONNECT tunnel (copy_bidirectional)
    └── streaming.rs     # StreamingBody (BroadcastStream-backed hyper Body impl)

tests/
└── integration_test.rs  # Full proxy integration tests with test HTTP server
```

## Architecture

```
Client Request
       │
       ▼
  ┌─────────┐     ┌─────────────┐
  │ Accept   │────▶│ Semaphore   │  (connection limiting)
  │ Loop     │     │ Permit      │
  └─────────┘     └──────┬──────┘
                         │
                         ▼
                 ┌───────────────┐
                 │ ProxyHandler  │
                 └───────┬───────┘
                         │
            ┌────────────┼────────────┐
            ▼            ▼            ▼
       CONNECT       GET/HEAD      Other
       (tunnel)     (cacheable)   (passthrough)
            │            │
            ▼            ▼
    ┌──────────┐  ┌─────────────┐
    │ TCP      │  │ TieredCache │
    │ bidir    │  │ lookup      │
    │ copy     │  └──────┬──────┘
    └──────────┘    HIT? │ MISS?
                    │    │
                    ▼    ▼
               Return  ┌──────────┐
               cached  │ Inflight │──▶ Join existing?
                       │ check    │
                       └────┬─────┘
                            │ New download
                            ▼
                     ┌──────────────┐
                     │ Fetch +      │
                     │ Broadcast    │──▶ Stream to all clients
                     │ + Cache      │
                     └──────────────┘
```

## Performance Characteristics

- **Memory-efficient streaming**: No full response buffering during downloads
- **Concurrent request deduplication**: N clients = 1 upstream request
- **Lock-free memory cache reads**: moka uses concurrent data structures internally
- **Async I/O**: Non-blocking throughout using Tokio
- **Multi-threaded runtime**: Tokio work-stealing scheduler uses all CPU cores
- **Connection pooling**: Upstream connections are reused via hyper-util's connection pool
