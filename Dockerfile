# Multi-stage build for Squiddish caching proxy
FROM rust:1.83-slim as builder

WORKDIR /build

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests
COPY Cargo.toml ./

# Create dummy main to build dependencies
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -rf src

# Copy source code
COPY src ./src
COPY tests ./tests

# Build the actual binary
RUN touch src/main.rs && \
    cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -u 1000 -s /bin/bash squiddish

# Create cache directory
RUN mkdir -p /cache && chown squiddish:squiddish /cache

WORKDIR /app

# Copy binary from builder
COPY --from=builder /build/target/release/squiddish /usr/local/bin/squiddish

# Copy example config
COPY config.toml.example /app/config.toml.example

USER squiddish

# Environment variables with defaults
ENV SQUIDDISH_BIND_ADDR="0.0.0.0:3128" \
    SQUIDDISH_MEMORY_SIZE="1073741824" \
    SQUIDDISH_DISK_SIZE="107374182400" \
    SQUIDDISH_CACHE_DIR="/cache" \
    SQUIDDISH_COMPRESSION="true" \
    SQUIDDISH_TTL_SECONDS="604800" \
    SQUIDDISH_APT_ENABLED="true" \
    SQUIDDISH_APT_LIST_TTL="3600" \
    SQUIDDISH_MAX_BODY_SIZE="10737418240" \
    SQUIDDISH_MAX_CONNECTIONS="1000" \
    SQUIDDISH_TIMEOUT_SECONDS="300" \
    SQUIDDISH_STRICT_HTTPS="true" \
    RUST_LOG="squiddish=info"

EXPOSE 3128

VOLUME ["/cache"]

# Use shell to expand environment variables
ENTRYPOINT ["/usr/local/bin/squiddish"]
