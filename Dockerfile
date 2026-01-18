# Build stage
FROM rust:1.84-slim AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Install wasm32 target for client build
RUN rustup target add wasm32-unknown-unknown

# Install Dioxus CLI (pinned version for reproducible builds)
RUN cargo install dioxus-cli@0.7.3 --locked

# Copy manifests
COPY Cargo.toml Cargo.lock Dioxus.toml ./

# Create dummy source for dependency caching
RUN mkdir -p src/app && \
    echo "fn main() {}" > src/main.rs && \
    echo "pub mod app;" > src/lib.rs && \
    echo "// stub" > src/app/mod.rs

# Build dependencies only (cached layer)
RUN cargo build --release 2>/dev/null || true
RUN rm -rf src

# Copy actual source
COPY src/ ./src/

# Build with Dioxus (fullstack: server + WASM client)
RUN dx build --release --platform web --features web

# Runtime stage
FROM debian:bookworm-slim

WORKDIR /app

# Install runtime dependencies (minimal - using rustls, no OpenSSL needed)
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy binary and web assets from builder
COPY --from=builder /app/target/dx/unified-hifi-control/release/web/unified-hifi-control /app/
COPY --from=builder /app/target/dx/unified-hifi-control/release/web/public /app/public

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
