# Multi-stage build for Squiddish caching proxy
# Uses musl for static linking to create a minimal scratch-based image
FROM rust:1.83-alpine as builder

WORKDIR /build

# Install musl build tools for static linking
RUN apk add --no-cache musl-dev

# Copy manifests
COPY Cargo.toml ./

# Create dummy main to build dependencies
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release --target x86_64-unknown-linux-musl && \
    rm -rf src

# Copy source code
COPY src ./src
COPY tests ./tests

# Build the actual binary with static linking
RUN touch src/main.rs && \
    cargo build --release --target x86_64-unknown-linux-musl && \
    strip target/x86_64-unknown-linux-musl/release/squiddish

# Runtime stage - from scratch for minimal image
FROM scratch

# Copy CA certificates for HTTPS
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/

# Copy the statically linked binary
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/squiddish /squiddish

# Environment variables with defaults
# Size units: B, KB, MB, GB, TB (e.g., "1GB", "512MB", "2.5GB")
# Time units: s, m, h, d (e.g., "5m", "2h", "7d")
# Plain numbers: bytes for sizes, seconds for times
ENV SQUIDDISH_BIND_ADDR="0.0.0.0:3128" \
    SQUIDDISH_MEMORY_SIZE="1GB" \
    SQUIDDISH_DISK_SIZE="100GB" \
    SQUIDDISH_CACHE_DIR="/cache" \
    SQUIDDISH_COMPRESSION="true" \
    SQUIDDISH_TTL="7d" \
    SQUIDDISH_APT_ENABLED="true" \
    SQUIDDISH_APT_LIST_TTL="1h" \
    SQUIDDISH_MAX_BODY_SIZE="10GB" \
    SQUIDDISH_MAX_CONNECTIONS="1000" \
    SQUIDDISH_TIMEOUT="5m" \
    SQUIDDISH_STRICT_HTTPS="true" \
    RUST_LOG="squiddish=info"

# Run as non-root user
USER 1000:1000

EXPOSE 3128

VOLUME ["/cache"]

ENTRYPOINT ["/squiddish"]
