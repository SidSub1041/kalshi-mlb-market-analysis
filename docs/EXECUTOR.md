# Demo-first executor

`kalshi-executor` is a separate process from `paper-trader`. It owns all order
transmission; the strategy supplies a fresh `desired_positions.json` snapshot.
It starts disabled, has no production default, and fails closed on a stale
strategy file, a `KILL` file, a daily lockout, or unverified cancellation.

## Files and configuration

Copy `config.example.toml` to the ignored `config.toml`. It pins strategy size
to one contract. For the first run use only the demo REST and WebSocket
endpoints shown in the example. Add the demo
`key_id` and the local filesystem path to its PEM key. Never commit that file
or send the key through chat.

The executor will not create an `OrderClient` until `live_enabled = true`.
Production also needs `confirm_prod = true`; keep that false until all demo
gates have passed.

The strategy adapter writes this atomically (write a temporary file then
rename it) at `intent_path` every few seconds:

```json
{
  "book_updated_at_ms": 1756830000000,
  "game_updated_at_ms": 1756830000000,
  "intents": [{
    "ticker": "KX...",
    "client_order_id": "4b7bced8-21d4-4d75-a485-09260fec7964",
    "target_contracts": 1,
    "limit_price_cents": 42,
    "mid_price_cents": 42.0
  }]
}
```

`client_order_id` is a UUID created once for the intent and retained if the
request is retried. A new ID means a new desired order. `time_in_force` and
`post_only` let the strategy distinguish a resting maker order from an explicit
non-post-only `fill_or_kill` exit. The initial executor
supports whole-contract long-YES targets only; it rejects fractional and short
targets rather than guessing how to hedge them.

## Commands

Build and inspect without connecting to Kalshi:

```sh
cd kalshi-pipeline
cargo run -p kalshi-executor -- --help
```

With a demo config and a deliberately unmarketable one-contract price:

```sh
cargo run --release -p kalshi-executor -- --config config.toml smoke \
  --ticker YOUR_DEMO_TICKER --price-cents 1 --side bid
```

The smoke command is demo-only. It creates one post-only order, queries it,
cancels it, and verifies it is no longer resting. It never creates a marketable
order.

After that gate, the continuous executor can be started with:

```sh
cargo run --release -p kalshi-executor -- --config config.toml run
```

On startup it checks the balance floor, cancels and verifies all resting orders,
and refuses to start with pre-existing positions unless
`adopt_positions_on_boot = true`. It listens to authenticated `fill` events and
updates position state only from those events. Each order is checked by the risk
module before it is submitted.

`touch KILL` in `run_dir` triggers cancellation and a reduce-only,
fill-or-kill flatten using a fresh quote. The same guarded flatten path is
available manually:

```sh
cargo run --release -p kalshi-executor -- --config config.toml flatten
```

Do not use the flatten command in production until you have seen it work on
demo; it intentionally sends orders when invoked.

## Before production

Run the smoke test, idempotency/restart drills, and extended demo validation
before creating or using production credentials. Production must retain the
small caps in `config.toml`, have an explicit written scaling rule, and begin
only with supervised one-contract sessions.
