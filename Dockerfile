# Multi-stage build for Squiddish caching proxy
# Uses musl for static linking to create a minimal scratch-based image
# Supports multi-arch: linux/amd64, linux/arm64
# Uses native compilation on each platform (no cross-compilation)

FROM rust:1.83-alpine AS builder

ARG TARGETPLATFORM

WORKDIR /build

# Install musl build tools for static linking
RUN apk add --no-cache musl-dev

# Determine the Rust target based on current architecture (not TARGETPLATFORM)
# This works because buildx runs native builds on each platform
RUN case "$(uname -m)" in \
    "x86_64") echo "x86_64-unknown-linux-musl" > /tmp/rust-target ;; \
    "aarch64") echo "aarch64-unknown-linux-musl" > /tmp/rust-target ;; \
    *) echo "Unsupported architecture: $(uname -m)" && exit 1 ;; \
    esac && \
    export RUST_TARGET=$(cat /tmp/rust-target) && \
    rustup target add $RUST_TARGET

# Copy manifests
COPY Cargo.toml ./

# Create dummy src files to build dependencies (both lib.rs and main.rs needed)
RUN export RUST_TARGET=$(cat /tmp/rust-target) && \
    mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    touch src/lib.rs && \
    cargo build --release --target $RUST_TARGET 2>/dev/null || true && \
    rm -rf src

# Copy source code
COPY src ./src
COPY tests ./tests

# Build the actual binary with static linking
RUN export RUST_TARGET=$(cat /tmp/rust-target) && \
    touch src/main.rs src/lib.rs && \
    cargo build --release --target $RUST_TARGET && \
    cp target/$RUST_TARGET/release/squiddish /build/squiddish

# Runtime stage - from scratch for minimal image
FROM scratch

# Copy CA certificates for HTTPS
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/

# Copy the statically linked binary
COPY --from=builder /build/squiddish /squiddish

# Environment variables with defaults
# Size units: B, KB, MB, GB, TB (e.g., "1GB", "512MB", "2.5GB")
# Time units: s, m, h, d (e.g., "5m", "2h", "7d")
# Plain numbers: bytes for sizes, seconds for times
ENV SQUIDDISH_BIND_ADDR="0.0.0.0:3128" \
    SQUIDDISH_MEMORY_SIZE="1GB" \
    SQUIDDISH_DISK_SIZE="100GB" \
    SQUIDDISH_CACHE_DIR="/cache" \
    SQUIDDISH_TTL="7d" \
    SQUIDDISH_APT_ENABLED="true" \
    SQUIDDISH_APT_LIST_TTL="1h" \
    SQUIDDISH_APT_PACKAGE_TTL="30d" \
    SQUIDDISH_APT_OTHER_TTL="1d" \
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
