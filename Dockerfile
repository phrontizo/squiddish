# Multi-stage build for Squiddish caching proxy
# Uses cross-compilation via xx (no QEMU emulation) for fast multi-arch builds
# Supports: linux/amd64, linux/arm64
#
# Build: docker buildx build --platform linux/amd64,linux/arm64 -t squiddish .

# xx provides cross-compilation helpers (maintained by Docker Inc)
FROM --platform=$BUILDPLATFORM tonistiigi/xx:1.6.1 AS xx

FROM --platform=$BUILDPLATFORM rust:1.85-alpine AS builder
COPY --from=xx / /

ARG TARGETPLATFORM

WORKDIR /build

# Install cross-compilation toolchain:
# - clang/lld: natively cross-compile (no QEMU needed)
# - musl-dev: native headers for build scripts
# - xx-apk: installs target-arch musl headers into the correct sysroot
RUN apk add --no-cache clang lld musl-dev && \
    xx-apk add --no-cache musl-dev

# Determine Rust target triple and add it
RUN RUST_TARGET="$(xx-info march)-unknown-linux-musl" && \
    echo "$RUST_TARGET" > /tmp/rust-target && \
    rustup target add "$RUST_TARGET"

# Configure cargo to use xx-clang as the C compiler and linker for both targets.
# Only the matching target's settings are used; the other is ignored.
ENV CC=xx-clang \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=xx-clang \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=xx-clang

# Copy manifests and lock file for reproducible, cacheable dependency builds
COPY Cargo.toml Cargo.lock ./

# Build dependencies only (Docker layer cache — rebuilds only when Cargo.toml/lock change)
RUN export RUST_TARGET=$(cat /tmp/rust-target) && \
    mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    touch src/lib.rs && \
    cargo build --release --target $RUST_TARGET 2>/dev/null || true && \
    rm -rf src

# Copy source code
COPY src ./src
COPY tests ./tests

# Build the actual binary and verify it targets the correct platform
RUN export RUST_TARGET=$(cat /tmp/rust-target) && \
    touch src/main.rs src/lib.rs && \
    cargo build --release --target $RUST_TARGET && \
    xx-verify target/$RUST_TARGET/release/squiddish && \
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
