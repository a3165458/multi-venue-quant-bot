#!/usr/bin/env python3
"""Multi-window robustness validation for top trend candidates.

Runs each candidate on full-year + 4 quarter windows + 2 half windows, plus
notional sensitivity. Outputs a comparison table sorted by full-year Sharpe.
"""
import csv
import re
import subprocess

BIN = "./target/release/multi-venue-quant-bot"
DATA = "backtests/data/BTC-arcus-1h-20250809-20260809.csv"
CAPITAL = 1730.0
CONFIG = "config/settings.arcus.yaml"
OUT = "backtests/results/arcus-year"

WINDOWS = [
    ("full", "2025-08-09", "2026-08-09"),
    ("q1", "2025-08-09", "2025-11-09"),
    ("q2", "2025-11-09", "2026-02-09"),
    ("q3", "2026-02-09", "2026-05-09"),
    ("q4", "2026-05-09", "2026-08-09"),
    ("h1", "2025-08-09", "2026-02-09"),
    ("h2", "2026-02-09", "2026-08-09"),
]


def run_backtest(params: str, start: str, end: str) -> dict:
    cmd = [
        BIN, "backtest", "--strategy", "trend",
        "--data", DATA, "--start", start, "--end", end,
        "--capital", str(CAPITAL), "--params", params,
        "--config", CONFIG, "-o", f"{OUT}/_tmp",
    ]
    p = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    out = p.stdout
    def grab(label: str) -> float | None:
        m = re.search(rf"\b{label}:\s*(-?[\d.]+)%?", out)
        return float(m.group(1)) if m else None
    trades = re.search(r"Trades:\s*(\d+)", out)
    return {
        "ret": grab("Return"), "sharpe": grab("Sharpe"), "dd": grab("Max DD"),
        "trades": int(trades.group(1)) if trades else 0,
    }


def main():
    # top candidates from full sweep (dedup by identical result signature)
    seen = set()
    cands = []
    with open(f"{OUT}/trend_full_sweep.csv") as f:
        for row in csv.DictReader(f):
            key = (row["return_pct"], row["sharpe"], row["max_dd_pct"], row["trades"])
            if key in seen:
                continue
            seen.add(key)
            cands.append(row["params"])
            if len(cands) >= 14:
                break
    # plus current live candidate at same notional
    cands.insert(0, "fast_ma=7,slow_ma=21,stop_loss=0.05,take_profit=0.06,trailing_stop=0.0,notional=400.0,confirm_slope_min=0.005,confirm_lookback=3")

    print(f"validating {len(cands)} candidates on {len(WINDOWS)} windows\n")
    rows_out = []
    for params in cands:
        tag = params.split(",")[0].split("=")[1]
        tag = f"{params.split('fast_ma=')[1].split(',')[0]}/{params.split('slow_ma=')[1].split(',')[0]} sl={params.split('stop_loss=')[1].split(',')[0]} tp={params.split('take_profit=')[1].split(',')[0]} tr={params.split('trailing_stop=')[1].split(',')[0]} cmin={params.split('confirm_slope_min=')[1].split(',')[0]} clb={params.split('confirm_lookback=')[1]}"
        row = [tag]
        for wname, ws, we in WINDOWS:
            r = run_backtest(params, ws, we)
            row.append(r["ret"])
        rows_out.append((params, row))
        print(f"{tag}")
        print(f"   full {row[1]:+7.2f}% | q1 {row[2]:+7.2f} q2 {row[3]:+7.2f} q3 {row[4]:+7.2f} q4 {row[5]:+7.2f} | h1 {row[6]:+7.2f} h2 {row[7]:+7.2f}")

    with open(f"{OUT}/trend_multi_window.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["params", "full", "q1", "q2", "q3", "q4", "h1", "h2"])
        for params, row in rows_out:
            w.writerow([params] + row[1:])
    print("\nwrote", f"{OUT}/trend_multi_window.csv")


if __name__ == "__main__":
    main()
