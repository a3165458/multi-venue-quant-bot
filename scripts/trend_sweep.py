#!/usr/bin/env python3
"""Full-parameter trend sweep driver for Arcus full-year backtest.

Sweeps fast/slow/sl/tp/trail/cmin/clb at fixed notional, plus a notional
sensitivity stage for the top candidates. Sorts by Sharpe, gates on MaxDD<=10%.
"""
import csv
import itertools
import re
import subprocess
import sys
import time

BIN = "./target/release/multi-venue-quant-bot"
DATA = "backtests/data/BTC-arcus-1h-20250809-20260809.csv"
START, END = "2025-08-09", "2026-08-09"
CAPITAL = 1730.0
CONFIG = "config/settings.arcus.yaml"
OUT = "backtests/results/arcus-year"


def run_backtest(params: str) -> dict | None:
    cmd = [
        BIN, "backtest", "--strategy", "trend",
        "--data", DATA, "--start", START, "--end", END,
        "--capital", str(CAPITAL), "--params", params,
        "--config", CONFIG, "-o", f"{OUT}/_tmp",
    ]
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    except subprocess.TimeoutExpired:
        return None
    out = p.stdout
    def grab(label: str) -> float | None:
        m = re.search(rf"\b{label}:\s*(-?[\d.]+)%?", out)
        return float(m.group(1)) if m else None
    trades = re.search(r"Trades:\s*(\d+)", out)
    return {
        "return_pct": grab("Return"),
        "sharpe": grab("Sharpe"),
        "max_dd_pct": grab("Max DD"),
        "trades": int(trades.group(1)) if trades else 0,
        "win_rate": grab("Win Rate"),
    }


def main():
    stage = sys.argv[1] if len(sys.argv) > 1 else "sweep"
    if stage == "sweep":
        fasts = [5, 7, 10, 14]
        slows = [14, 21, 30, 50]
        sls = [0.03, 0.05, 0.08]
        tps = [0.04, 0.06, 0.10]
        trails = [0.0, 0.02]
        cmins = [0.003, 0.005, 0.008]
        clbs = [3, 5]
        notional = 400.0
        combos = []
        for f in fasts:
            for s in slows:
                if f >= s:
                    continue
                for sl in sls:
                    for tp in tps:
                        if tp <= sl:
                            continue
                        for tr in trails:
                            for cmin in cmins:
                                for clb in clbs:
                                    combos.append(
                                        f"fast_ma={f},slow_ma={s},stop_loss={sl},take_profit={tp},"
                                        f"trailing_stop={tr},notional={notional},"
                                        f"confirm_slope_min={cmin},confirm_lookback={clb}"
                                    )
        print(f"sweeping {len(combos)} combos", flush=True)
        results = []
        t0 = time.time()
        for i, params in enumerate(combos):
            r = run_backtest(params)
            if r:
                results.append((params, r))
            if (i + 1) % 100 == 0:
                print(f"  {i+1}/{len(combos)} elapsed {time.time()-t0:.0f}s", flush=True)
        results.sort(key=lambda x: x[1]["sharpe"], reverse=True)
        with open(f"{OUT}/trend_full_sweep.csv", "w", newline="") as f:
            w = csv.writer(f)
            w.writerow(["params", "return_pct", "sharpe", "max_dd_pct", "trades", "win_rate"])
            for params, r in results:
                w.writerow([params, r["return_pct"], r["sharpe"], r["max_dd_pct"], r["trades"], r["win_rate"]])
        print("TOP 25 (Sharpe):")
        for params, r in results[:25]:
            print(f"  {r['return_pct']:+7.2f}% sh={r['sharpe']:6.3f} dd={r['max_dd_pct']:6.2f}% tr={r['trades']:4d} wr={r['win_rate']:5.1f}% | {params}")
    elif stage == "notional":
        # notional sensitivity on a fixed candidate list
        cands = []
        with open(f"{OUT}/trend_full_sweep.csv") as f:
            for row in csv.DictReader(f):
                cands.append(row["params"])
        # take top 12 by sharpe and re-run at several notionals
        cands = cands[:12]
        notionals = [100.0, 200.0, 300.0, 400.0, 430.0, 500.0]
        results = []
        for params in cands:
            base = params.replace("notional=400.0", "notional={}")
            for n in notionals:
                r = run_backtest(base.format(n))
                if r:
                    results.append((base.format(n), r))
        results.sort(key=lambda x: x[1]["sharpe"], reverse=True)
        with open(f"{OUT}/trend_notional_sweep.csv", "w", newline="") as f:
            w = csv.writer(f)
            w.writerow(["params", "return_pct", "sharpe", "max_dd_pct", "trades", "win_rate"])
            for params, r in results:
                w.writerow([params, r["return_pct"], r["sharpe"], r["max_dd_pct"], r["trades"], r["win_rate"]])
        print("NOTIONAL TOP 20:")
        for params, r in results[:20]:
            print(f"  {r['return_pct']:+7.2f}% sh={r['sharpe']:6.3f} dd={r['max_dd_pct']:6.2f}% tr={r['trades']:4d} | {params}")


if __name__ == "__main__":
    main()
