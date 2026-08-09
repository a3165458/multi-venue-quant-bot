#!/usr/bin/env bash
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT_ROOT"

VENUE="${1:-${TRADING_VENUE:-lighter-mainnet}}"
case "$VENUE" in
    mainnet|lighter-mainnet) VENUE="lighter-mainnet"; CONFIG="config/settings.yaml" ;;
    robinhood|lighter-robinhood) VENUE="lighter-robinhood"; CONFIG="config/settings.robinhood.yaml" ;;
    arcus-mainnet) CONFIG="config/settings.arcus.yaml" ;;
    arcus-testnet) CONFIG="config/settings.arcus-testnet.yaml" ;;
    *)
        echo "Usage: $0 [lighter-mainnet|lighter-robinhood|arcus-mainnet|arcus-testnet]" >&2
        exit 2
        ;;
esac

echo "Building Multi-Venue Quant Bot..."
cargo build --release --locked

echo "Starting Multi-Venue Quant Bot on ${VENUE}..."
exec env \
    TRADING_VENUE="$VENUE" \
    RUST_LOG="${RUST_LOG:-info}" \
    ./target/release/multi-venue-quant-bot live --config "$CONFIG"
