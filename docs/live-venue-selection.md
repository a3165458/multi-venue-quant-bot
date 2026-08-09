# Live venue selection

The bot can run the same strategy and risk checks against four live venue profiles:

- `lighter-mainnet`
- `lighter-robinhood`
- `arcus-mainnet`
- `arcus-testnet`

## Command line

Copy `.env.example` to `.env`, fill only the credentials for the venue you intend to use, then run:

```bash
./scripts/run_live.sh arcus-testnet
```

Replace `arcus-testnet` with any profile above. Legacy `mainnet` and `robinhood` arguments remain
accepted and map to the corresponding Lighter profiles.

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

## Disconnect behavior

Arcus does not provide cancel-on-disconnect. If its WebSocket closes, the bot sends a REST
`cancelAllOrders` request and stops the live loop. Run the process under a supervisor so an operator
can inspect the disconnect before restarting it.
