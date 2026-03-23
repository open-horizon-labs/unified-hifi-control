# syntax=docker/dockerfile:1.4
# Build stage - ADR 002: Single binary with embedded web assets
FROM rust:1.92-slim AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    curl \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Install wasm32 target for client build
RUN rustup target add wasm32-unknown-unknown

# Install Dioxus CLI and Tailwind (with cache mount for cargo registry)
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo install dioxus-cli@0.7.3

# Copy manifests first (for dependency caching)
COPY Cargo.toml Cargo.lock Dioxus.toml Makefile ./
COPY muse-events/Cargo.toml ./muse-events/

# Create dummy source for dependency caching
RUN mkdir -p src/app && \
    echo "fn main() {}" > src/main.rs && \
    echo "pub mod app;" > src/lib.rs && \
    echo "// stub" > src/app/mod.rs && \
    mkdir -p muse-events/src && \
    echo "// stub" > muse-events/src/lib.rs

# Build dependencies only (with cache mounts)
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --release 2>/dev/null || true

# Copy actual source
COPY src/ ./src/
COPY muse-events/ ./muse-events/
COPY public/ ./public/

# Build Tailwind CSS (downloads standalone CLI if needed)
RUN apt-get update && apt-get install -y make && rm -rf /var/lib/apt/lists/*
RUN make css

# Version info for compile-time env!() macros
ARG APP_VERSION=dev
ENV UHC_VERSION=$APP_VERSION
ENV UHC_GIT_SHA=docker

# ADR 002: Build WASM assets first, then build server which embeds them
# Force rebuild to pick up UHC_VERSION/UHC_GIT_SHA env vars
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    dx build --fullstack --release @client --no-default-features --features web @server --features server && \
    cargo build --release && \
    cp target/release/unified-hifi-control /app/unified-hifi-control-bin

# Runtime stage - minimal image, no public/ directory needed
FROM debian:bookworm-slim

WORKDIR /app

# Install runtime dependencies (minimal - using rustls, no OpenSSL needed)
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy binary with embedded web assets (ADR 002 - no public/ directory)
COPY --from=builder /app/unified-hifi-control-bin /app/unified-hifi-control

# Create data directory for config persistence
RUN mkdir -p /data

# Version from build arg
ARG APP_VERSION=dev
ENV APP_VERSION=$APP_VERSION

# Environment
ENV PORT=8088
ENV CONFIG_DIR=/data
ENV RUST_LOG=info

EXPOSE 8088

CMD ["/app/unified-hifi-control"]
