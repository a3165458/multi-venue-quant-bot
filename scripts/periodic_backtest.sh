#!/bin/bash
# 定期回测：拉取最近 30 天真实 BTC 数据，用实盘参数跑封顶版回测，
# 把关键指标追加到 backtests/tracking/backtest_history.csv。由 cron 周期调用。
set -e
cd "$(dirname "$0")/.."
BIN=./target/release/multi-venue-quant-bot
[ ! -x "$BIN" ] && exit 0
mkdir -p backtests/tracking backtests/data
END=$(date -u +%Y-%m-%d)
START=$(date -u -d '30 days ago' +%Y-%m-%d)
DATA="backtests/data/BTC-mainnet-1h-$(date -u -d "$START" +%Y%m%d)-$(date -u +%Y%m%d).csv"
$BIN download --symbol BTC --interval 1h --start "$START" --end "$END" >/dev/null 2>&1 || exit 0
# 下载输出文件名由程序按 START/END 生成，重新推断
DATA=$(ls -t backtests/data/BTC-mainnet-1h-*.csv 2>/dev/null | head -1)
[ -z "$DATA" ] && exit 0
OUT="backtests/results/periodic-$(date -u +%Y%m%d)"
LOG=$($BIN backtest --strategy grid_trading --data "$DATA" \
  --start "$START" --end "$END" --capital 255 \
  --params "grid_count=12,investment=30,deviation=0.004" \
  --output "$OUT" 2>&1 | grep -iE "Return:|Sharpe:|Max DD:|Trades:|Win Rate:" \
  | sed -E 's/.*(Return|Sharpe|Max DD|Trades|Win Rate): *//' | tr '\n' ',')
F="backtests/tracking/backtest_history.csv"
[ ! -f "$F" ] && echo "timestamp,window_start,window_end,return,sharpe,max_dd,trades,win_rate" > "$F"
echo "$(date -u +%Y-%m-%dT%H:%M:%SZ),$START,$END,${LOG%,}" >> "$F"
