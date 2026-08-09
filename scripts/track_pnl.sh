#!/bin/bash
# 持续跟踪实盘收益：每次运行追加一行快照到 backtests/tracking/live_pnl.csv
# 由 cron 定期调用（见 crontab）。只读 dashboard，不影响交易。
set -e
cd "$(dirname "$0")/.."
mkdir -p backtests/tracking
F="backtests/tracking/live_pnl.csv"
TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
ST=$(curl -s --max-time 8 http://localhost:3028/api/status 2>/dev/null)
POS=$(curl -s --max-time 8 http://localhost:3028/api/positions 2>/dev/null)
[ -z "$ST" ] && exit 0   # dashboard 不可达则跳过本次，不写脏数据
val(){ echo "$1" | grep -o "\"$2\":[0-9.eE+-]*" | head -1 | cut -d: -f2; }
EQ=$(val "$ST" equity); TR=$(val "$ST" total_realized_pnl); DR=$(val "$ST" daily_realized_pnl)
UP=$(val "$ST" total_pnl); NT=$(val "$ST" total_trades)
PSZ=$(echo "$POS" | grep -o '"size":[0-9.eE+-]*' | head -1 | cut -d: -f2)
[ ! -f "$F" ] && echo "timestamp,equity,total_realized_pnl,daily_realized_pnl,unrealized_pnl,total_trades,btc_pos_size" > "$F"

# 停滞检测：2026-07-24 曾死锁停摆 15.5h，cron 老老实实记了 62 行却没告警——记录 ≠ 监控。
# 只看 total_realized_pnl 与 btc_pos_size：两者都是持久化值，不像 total_trades 会在重启后归零。
# 两者连续 STALL_N 次采样均不变 → 判定为无成交，写告警文件。
STALL_N=8   # 8 × 15min = 2 小时无成交
if [ -f "$F" ]; then
  SAME=$(awk -F, -v tr="$TR" -v ps="$PSZ" 'NR>1{print $3","$7}' "$F" | tail -n "$STALL_N" \
         | grep -c "^${TR},${PSZ}$" || true)
  if [ "${SAME:-0}" -ge "$STALL_N" ]; then
    echo "[$TS] ⚠️ STALL: 已连续 ${STALL_N} 次采样(~$((STALL_N*15))min)无成交 — realized_pnl=$TR pos=$PSZ。检查是否死锁: pm2 logs multi-venue-quant-bot | grep 'kept .* reducing'（N>6 即逃生阀失效）" \
      >> backtests/tracking/ALERTS.log
  fi
fi

echo "$TS,$EQ,$TR,$DR,$UP,$NT,$PSZ" >> "$F"
