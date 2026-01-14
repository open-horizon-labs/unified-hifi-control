# Build stage
FROM rust:1.85-slim AS builder

WORKDIR /app

# Install build dependencies (minimal - using rustls, no OpenSSL needed)
RUN apt-get update && apt-get install -y \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests
COPY Cargo.toml Cargo.lock ./

# Create dummy source for dependency caching
RUN mkdir -p src && \
    echo "fn main() {}" > src/main.rs && \
    echo "// lib stub" > src/lib.rs

# Build dependencies only (cached layer)
RUN cargo build --release --bin unified-hifi-control 2>/dev/null || true
RUN rm -rf src

# Copy actual source
COPY src/ ./src/

# Build only the main binary (skip protocol-checker dev tool)
RUN cargo build --release --bin unified-hifi-control

# Runtime stage
FROM debian:bookworm-slim

WORKDIR /app

# Install runtime dependencies (minimal - using rustls, no OpenSSL needed)
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy binary from builder
COPY --from=builder /app/target/release/unified-hifi-control /app/

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
