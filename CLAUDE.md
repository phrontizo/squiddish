# Squiddish - Development Guide

## Project Overview

Squiddish is a high-performance HTTP caching proxy written in Rust, optimized for APT package managers.
It uses a two-tier cache (memory + disk), concurrent request deduplication via broadcast channels,
and CONNECT tunneling for HTTPS passthrough.

## Development Workflow

### Test-Driven Development (TDD)

All changes MUST follow TDD. No exceptions.

1. **Write a failing test first** that describes the expected behavior
2. **Run the test** to confirm it fails for the right reason
3. **Write the minimal implementation** to make the test pass
4. **Refactor** while keeping tests green
5. **Run the full test suite** before considering the change complete

```bash
# Run all tests
cargo test

# Run a specific test
cargo test test_name

# Run tests with output
cargo test -- --nocapture

# Run with logging
RUST_LOG=debug cargo test
```

### Test Organization

- **Unit tests**: In `#[cfg(test)] mod tests` blocks within each source file
- **Integration tests**: In `tests/integration_test.rs` — full proxy with test HTTP server
- Integration tests use pre-bound listeners (`server.serve(listener)`) to avoid port races
- Integration tests use retry-based `wait_for_server()` instead of fixed sleeps

## Architecture

### Key Design Decisions

- **No trait for Cache**: `TieredCache` calls `MemoryCache` and `DiskCache` directly (no dynamic dispatch overhead)
- **moka for memory cache**: Concurrent reads without write locks, TinyLFU admission policy, automatic weighted eviction
- **BroadcastStream for streaming**: `tokio_stream::wrappers::BroadcastStream` for correct async waker registration (never use manual `try_recv` + `wake_by_ref`)
- **copy_bidirectional for tunneling**: Correctly handles TCP half-close
- **Pre-connect before 200 OK**: CONNECT handler connects to target first, then sends success to client
- **Atomic join_or_start_download**: Single write lock prevents TOCTOU between check and insert, and blocks add_chunk during subscribe + accumulated read
- **s-maxage precedence**: Per RFC 7234, `s-maxage` takes priority over `max-age` for shared caches
- **Config returns Result**: Invalid env vars terminate the process with a descriptive error

### Module Responsibilities

| Module | Responsibility |
|--------|---------------|
| `config.rs` | Env var parsing with `from_vars()` for testability |
| `error.rs` | Error types (`ProxyError` enum, `Result` alias) via `thiserror` |
| `apt.rs` | APT request detection (host + path + file patterns) |
| `cache/mod.rs` | `CacheEntry`, `TieredCache` (memory-first, disk-fallback, auto-promote) |
| `cache/key.rs` | SHA-256 cache key with 2-level directory sharding |
| `cache/memory.rs` | moka concurrent cache, weighted by entry size |
| `cache/disk.rs` | File-based cache with VecDeque eviction order |
| `cache/inflight.rs` | Download deduplication via broadcast channels |
| `proxy/mod.rs` | TCP accept loop, semaphore connection limiting, graceful shutdown |
| `proxy/handler.rs` | Request routing, cache logic, streaming downloads, error pages |
| `proxy/client.rs` | Upstream HTTP client with connection pooling |
| `proxy/tunnel.rs` | CONNECT tunnel with `copy_bidirectional` |
| `proxy/streaming.rs` | `StreamingBody` implementing hyper's `Body` trait |

## Coding Conventions

### Rust Style

- Use `thiserror` for error types, not manual `impl Display`
- Use `tracing` for logging, not `println!` or `log`
- Prefer `parking_lot::RwLock` over `std::sync::RwLock` for synchronous locks
- Use `tokio::sync` types for async-aware synchronization
- No `#[allow(dead_code)]` — remove unused code instead
- No `async_trait` — use concrete types to avoid heap allocation on hot paths
- Use `strip_suffix`/`strip_prefix` instead of manual byte-index slicing

### Error Handling

- Config parsing errors terminate the process (not silently fall back to defaults)
- Network/proxy errors return HTML error pages to the client
- Cache errors are logged but don't crash the proxy

### Caching Rules

- Only cache 2xx responses
- Respect `Cache-Control: no-store`, `no-cache`, `private`, and `Pragma: no-cache`
- Only `Accept-Encoding` is included in the cache key (not `User-Agent`)
- Items > 1MB are written to both memory and disk; smaller items are memory-only

## Files to Keep Updated

When making changes, ensure these files stay in sync with the codebase:

- `README.md` — features, config options, project structure, architecture
- `Dockerfile` — env var defaults, build steps
- `compose.yml` — env var defaults and documentation
- `.github/workflows/` — CI and Docker publish pipelines
- `CLAUDE.md` — this file: architecture, conventions, module responsibilities
