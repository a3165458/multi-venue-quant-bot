#!/bin/bash
set -e

echo "🚀 启动多交易所量化机器人..."

VENUE="${1:-lighter-mainnet}"
case "$VENUE" in
    mainnet|lighter-mainnet) VENUE="lighter-mainnet"; CONFIG="config/settings.yaml" ;;
    robinhood|lighter-robinhood) VENUE="lighter-robinhood"; CONFIG="config/settings.robinhood.yaml" ;;
    arcus-mainnet) CONFIG="config/settings.arcus.yaml" ;;
    arcus-testnet) CONFIG="config/settings.arcus-testnet.yaml" ;;
    *) echo "❌ 用法: $0 [lighter-mainnet|lighter-robinhood|arcus-mainnet|arcus-testnet]"; exit 1 ;;
esac

# 构建项目
echo "🔨 构建项目..."
cargo build --release

# 运行机器人
echo "🤖 运行交易机器人..."
TRADING_VENUE="$VENUE" RUST_LOG=${RUST_LOG:-info} ./target/release/lighter-bot live --config "$CONFIG"
