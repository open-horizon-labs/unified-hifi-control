FROM node:20-slim

WORKDIR /app

# Install dependencies for sharp and mDNS
RUN apt-get update && apt-get install -y \
    libvips-dev \
    avahi-daemon \
    libnss-mdns \
    && rm -rf /var/lib/apt/lists/*

# Copy package files
COPY package*.json ./

# Install production dependencies
RUN npm ci --omit=dev

# Copy source
COPY src/ ./src/

# Create data directory for config persistence
RUN mkdir -p /data

# Version from build arg
ARG APP_VERSION=dev
ENV APP_VERSION=$APP_VERSION

# Environment
ENV NODE_ENV=production
ENV PORT=8088
ENV CONFIG_DIR=/data

EXPOSE 8088

# Run as non-root user
RUN useradd -r -s /bin/false appuser && chown -R appuser:appuser /app /data
USER appuser

CMD ["node", "src/index.js"]
