# HIP-3 cross-dex basis (`io:SNDK` vs `xyz:SNDK`)

Optional same-ecosystem basis path for Hyperliquid HIP-3 books. This is **not**
CEX arb. Detection already existed (`probe_io_xyz_sndk_basis` every 30s). The
gap this document covers is execution + config: xyz was left unarmed because it
was not a configured coin, and the probe used conservative growth-mode taker
cost (~1.72 bps round-trip).

`maker_quote` is unchanged. Basis is a separate strategy and never replaces maker.

## Safety defaults

`config/settings.hyperliquid.yaml` ships with:

```yaml
trading:
  strategies:
    cross_dex_basis:
      enabled: false   # subscribe xyz + paper/log detect
      armed: false     # live simultaneous IOC legs
```

- `enabled: false` (default): REST probe only. xyz is **not** added to live
  symbols. Maker set stays `["io:SNDK", "io:ANTH"]`.
- `enabled: true`, `armed: false`: resolve/subscribe both pair coins, update the
  dashboard, log TRADEABLE edges. **No orders.** xyz is not maker-quoted.
- `enabled: true`, `armed: true`: live fire is possible **only when the loop is
  not paused**. `trading.start_paused` and the dashboard pause switch are still
  honored. Opening a hedge while paused is impossible; residual flatten is
  risk-reducing and may still run.

The process never auto-flips `armed` to true. Do not deploy an armed config
without an explicit operator change.

## Fees (do not use raw `userFees`)

Hyperliquid `userFees` still prints raw 1.5 / 4.5 bps. Entropy Tier 4 claims a
200% self-rebate on Entropy's HIP-3 fee share (plus referred-user benefit), so
effective net taker cost is ≈0 to slightly negative. This path does **not**
treat the API print as the operator's true cost.

| `fee_preset` | Round-trip taker default | When to use |
|---|---|---|
| `tier4` (default) | `0.2` bps safety buffer | Operator T4 + Entropy rebate |
| `growth` | `HIP3_GROWTH_TAKER_FEE_BPS * 2` (~1.72) | Conservative / unknown rebate |

`round_trip_taker_bps` and `min_net_bps` (default `1.0`) override the preset
when set. A crossed book is tradeable only when net = gross − round-trip >
`min_net_bps`.

## How to enable (paper, then live)

1. Keep `start_paused` / dashboard pause as you already run them.
2. Set `enabled: true`, leave `armed: false`. Restart.
3. Confirm Settings → Cross-dex basis shows last net bps / side and
   `paper / not armed`. Confirm xyz BBO appears in last prices and that maker
   is **not** quoting xyz.
4. Size caps: `max_notional` (per hedge), `max_abs_delta` (share units of
   `szi_a + szi_b`), `max_gross_notional`.
5. Only then set `armed: true`, restart, and unpause if you intend to fire.
   Simultaneous IOC legs buy the cheap ask / sell the rich bid. If one leg
   fails or fills short, the leftover is flatten/unwound on a timeout.

Turn `armed` back to `false` (or pause) to stop new hedges. Do not commit
secrets or a live `.env`.

## Dashboard fields

- `last_cross_dex_net_bps` / `last_cross_dex_side`
- `last_cross_dex_enabled` / `last_cross_dex_armed`
- `last_cross_dex_position`: `flat` (null), `hedged`, or `unwinding`
