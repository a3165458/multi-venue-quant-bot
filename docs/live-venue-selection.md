# Live venue selection

The repository defines five isolated venue profiles:

- `lighter-mainnet`
- `lighter-robinhood`
- `arcus-mainnet`
- `arcus-testnet`
- `aster-mainnet`

## Command line

Copy `.env.example` to `.env`, fill only the credentials for the venue you intend to use, then run:

```bash
./scripts/run_live.sh arcus-testnet
```

Replace `arcus-testnet` with a supported live profile above. Legacy `mainnet` and `robinhood`
arguments remain accepted and map to the corresponding Lighter profiles.

## Dashboard

Open Settings → Network, select a venue, save its isolated credentials, and click the venue button.
The selection is persisted as `TRADING_VENUE` in `.env`. Restart the bot to make the switch; the
currently running process deliberately does not hot-swap an exchange while orders may still be
open.

Each profile has its own credential namespace and runtime state directory, preventing an Arcus key
or account state from being reused by Lighter (or vice versa).

## Arcus credentials

Arcus needs three values for each environment:

- `ARCUS_<ENV>_API_KEY`: API Key issued by Arcus.
- `ARCUS_<ENV>_SIGNING_KEY`: API Signing Key, a 32-byte Ed25519 seed encoded as 64 hexadecimal characters.
- `ARCUS_<ENV>_ADDRESS`: the Ethereum master address against which the API key is registered.
- `ARCUS_<ENV>_ACCOUNT_INDEX`: subaccount index from 0 through 9.

The bot derives the public API key from the seed and never returns the seed through the Dashboard.
Use Arcus Testnet first and confirm market IDs from the Dashboard before enabling Mainnet.

## Aster V3 credentials

Aster Mainnet uses `https://fapi.asterdex.com` and `wss://fstream.asterdex.com`. Create an API
Wallet under the intended Aster Pro Futures V3 sub-account, then configure:

- `ASTER_MAINNET_SIGNER_ADDRESS`: authorized API Wallet public address (the V3 signer).
- `ASTER_MAINNET_SIGNER_PRIVATE_KEY`: API Wallet signer private key.

Never use the sub-account login key or a funding-wallet/L1 private key. The signer private key is write-only in the Dashboard;
GET returns only whether it is configured, and submitting an empty secret preserves the stored
value. Runtime state is isolated under `data/aster-mainnet/`.

`config/settings.aster.yaml` uses conservative BTCUSDT maker defaults. The published base fee
assumptions (maker 1.0 bps, taker 3.5 bps) should be checked against the actual sub-account tier.
The live path requires One-way and isolated-margin modes and only permits `maker_quote`; it
validates but never changes those account settings.

`trading.shadow_maker.enabled` runs a no-order shadow loop against live BBO data. It estimates
quote churn, virtual fills/volume, event and strategy latency, and side-adjusted 1s/5s/30s
markout. Requotes are modeled as a single `PUT /fapi/v3/order` amend so the virtual quote stays on
the book. Results are available at `/api/shadow`, in the Dashboard, and under
`data/aster-mainnet/shadow_metrics.json`.

`trading.hft_shadow` compares join-BBO and offset profiles in parallel, ranks them by 5s markout,
virtual volume, and request rate, and never submits orders. The live `maker_quote` path uses the
same amend-first replacement; unknown `orderId` still falls back to cancel-then-wait. Filter
rejects leave the original order in place. Crossed or locked books and 1s markout toxicity pull
virtual HFT quotes only.

Do not copy unbounded GitHub HFT examples that skip inventory caps, use taker/chase orders, or
enable live connectors by default.

Public historical candles do not require API Wallet credentials:

```bash
cargo run --release -- download-aster --symbols BTCUSDT --interval 1h \
  --start 2026-01-01 --end 2026-08-17
```

## Disconnect behavior

Arcus does not provide cancel-on-disconnect. If its WebSocket closes, the bot sends a REST
`cancelAllOrders` request and stops the live loop. Run the process under a supervisor so an operator
can inspect the disconnect before restarting it.

Aster refreshes `countdownCancelAll` every 30 seconds with a 120-second timeout. Market/user stream
disconnects, listenKey expiry, heartbeat failure, or an ambiguous order submission pause trading,
cancel configured symbols through REST, and stop the live loop for supervisor restart.
