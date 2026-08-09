#!/usr/bin/env python3
"""Download full-year BTC-USD 1h candles from Arcus mainnet API.

Arcus /v1/candles supports arbitrary `to` (unix microseconds) + countback<=1500,
so we page backwards in 1500-candle chunks until we pass the start boundary.
Output: backtests/data/BTC-arcus-1h-{start}-{end}.csv (same schema as backtest loader).
"""
import argparse
import csv
import json
import sys
import time
import urllib.request
from datetime import datetime, timezone

BASE = "https://api.arcus.xyz"
CHUNK = 1500


def fetch(market: str, timeframe: str, to_us: int, countback: int) -> list[dict]:
    url = f"{BASE}/v1/candles?market={market}&timeframe={timeframe}&to={to_us}&countback={countback}"
    for attempt in range(5):
        try:
            with urllib.request.urlopen(url, timeout=30) as r:
                data = json.loads(r.read().decode())
            return data.get("candles", [])
        except Exception as e:
            print(f"  retry {attempt + 1}: {e}", file=sys.stderr)
            time.sleep(1 + attempt * 2)
    raise RuntimeError(f"failed to fetch {url}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--market", default="BTC-USD")
    ap.add_argument("--timeframe", default="1h")
    ap.add_argument("--start", required=True, help="YYYY-MM-DD (UTC, inclusive)")
    ap.add_argument("--end", required=True, help="YYYY-MM-DD (UTC, inclusive)")
    ap.add_argument("--output", required=True)
    args = ap.parse_args()

    start_dt = datetime.strptime(args.start, "%Y-%m-%d").replace(tzinfo=timezone.utc)
    end_dt = datetime.strptime(args.end, "%Y-%m-%d").replace(tzinfo=timezone.utc)
    step_us = 3600 * 1_000_000  # 1h

    # page backwards from end
    to_us = end_dt.timestamp() * 1_000_000
    candles: dict[int, dict] = {}
    while to_us >= start_dt.timestamp() * 1_000_000:
        batch = fetch(args.market, args.timeframe, int(to_us), CHUNK)
        if not batch:
            print(f"  empty batch at {datetime.fromtimestamp(to_us/1e6, timezone.utc)}; stop")
            break
        added = 0
        for c in batch:
            ot = int(c["openTime"])
            if ot < start_dt.timestamp() * 1_000_000:
                continue
            if ot > end_dt.timestamp() * 1_000_000:
                continue
            if ot not in candles:
                candles[ot] = c
                added += 1
        earliest = min(int(c["openTime"]) for c in batch)
        print(f"  batch@{datetime.fromtimestamp(to_us/1e6, timezone.utc):%Y-%m-%d %H:%M} "
              f"earliest={datetime.fromtimestamp(earliest/1e6, timezone.utc):%Y-%m-%d %H:%M} new={added} total={len(candles)}")
        to_us = earliest - step_us
        time.sleep(0.2)

    rows = sorted(candles.values(), key=lambda c: c["openTime"])
    print(f"total candles: {len(rows)}")

    with open(args.output, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["timestamp", "open", "high", "low", "close", "volume", "symbol"])
        sym = args.market.split("-")[0]
        for c in rows:
            ts = datetime.fromtimestamp(int(c["openTime"]) / 1e6, timezone.utc).strftime(
                "%Y-%m-%dT%H:%M:%S+00:00"
            )
            w.writerow(
                [
                    ts,
                    c["open"],
                    c["high"],
                    c["low"],
                    c["close"],
                    c.get("volume", "0"),
                    sym,
                ]
            )
    print(f"written {args.output}")


if __name__ == "__main__":
    main()
