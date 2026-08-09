# ═══════════════════════════════════════════════════════
# Multi-Venue Quant Bot — Docker Build
# Multi-stage: build Rust binary + fetch signer .so
# ═══════════════════════════════════════════════════════

# ── Stage 1: Build the Rust binary ──
FROM rust:1.88-bookworm AS builder

WORKDIR /usr/src/app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY config/ config/
COPY benches/ benches/

RUN cargo build --release --locked

# ── Stage 2: Fetch lighter-signer.so from PyPI package ──
FROM python:3.12-slim-bookworm AS signer

RUN pip install --no-cache-dir lighter-sdk && \
    cp /usr/local/lib/python3.12/site-packages/lighter/signers/lighter-signer-linux-amd64.so /tmp/lighter-signer.so || \
    cp /usr/local/lib/python3.12/site-packages/lighter/signers/lighter-signer-linux-arm64.so /tmp/lighter-signer.so

# ── Stage 3: Runtime image ──
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home quantbot

WORKDIR /app

# Copy binary from builder
COPY --from=builder /usr/src/app/target/release/multi-venue-quant-bot /usr/local/bin/multi-venue-quant-bot

# Copy signer .so from signer stage
COPY --from=signer /tmp/lighter-signer.so /app/lighter-signer.so

# Copy config
COPY config/ /app/config/

# Create data directory for PnL persistence
RUN mkdir -p /app/data /app/logs && chown -R quantbot:quantbot /app

USER quantbot

# Expose dashboard port
EXPOSE 4028

# Health check
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -sf http://localhost:4028/api/status || exit 1

# Default command: live trading
CMD ["multi-venue-quant-bot", "live"]
