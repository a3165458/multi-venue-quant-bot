# Arcus API adapter

The bot includes an Arcus REST adapter in `src/arcus.rs`. It targets the official endpoints:

| Environment | REST | WebSocket |
| --- | --- | --- |
| Mainnet | `https://api.arcus.xyz` | `wss://api.arcus.xyz/v1/ws` |
| Testnet | `https://api.testnet.arcus.xyz` | `wss://api.testnet.arcus.xyz/v1/ws` |

Read-only clients can discover markets, fetch BBO data, and read account state. Authenticated
clients accept an Ed25519 signer callback so private keys remain in the caller's existing secret
store:

```rust,ignore
let client = ArcusClient::authenticated(
    ArcusEnvironment::Testnet,
    public_key_hex,
    move |message| secret_store.sign_ed25519_hex(message),
)?;
```

For order placement, build both `PlaceOrder` (engine-native tick/quantum values used for the typed
signature) and `PlaceOrderRequest` (human-readable decimal strings sent to REST). The adapter checks
that address, account, and market agree before signing and sends `X-API-Key`, `X-Timestamp`, and
`X-Signature` as required by Arcus.

Keep the following Arcus constraints in mind:

- Convert price and size exactly with the market's current `tickSize` and `stepSize`.
- `goodTilTime` is sent in microseconds but signed as nanoseconds, and must be at least one month in
  the future.
- A successful `200` or `202` response is only an acknowledgement. Order lifecycle state comes
  from the `orders` WebSocket channel.
- Arcus has no cancel-on-disconnect. Production strategies need an external kill switch.
