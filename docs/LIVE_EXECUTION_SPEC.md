# Live Execution Spec

The contract for the order-execution layer. The pieces below the line are
implemented and tested in `common` — the executor itself is deliberately left
to the human operator. Wire it exactly to these interfaces and invariants.

## What already exists (do not reimplement)

| Component | Where | What it gives you |
|---|---|---|
| Request signing | `common::auth::Signer::headers(method, path)` | RSA-PSS headers for ANY method — works for POST/DELETE unchanged |
| Read-only account views | `common::portfolio::PortfolioClient` | `balance_cents()`, `positions()`, `resting_orders()` — signed GETs |
| Fill parsing | `common::portfolio::parse_fill` | Tolerant cents / dollars_fp decoding of `fills` ws messages |
| Risk layer | `common::risk` | `RiskBook::approve` (all caps), lockout files, `kill_requested`, `DeadMan` |
| Fair values + strategy | `paper-trader` | Emits the same signals the executor will act on |
| Host config | `config.toml` `api_base` / `ws_base` | Demo vs prod is a config switch |

## The two calls you write

Sign exactly like the reads: the path portion only, e.g.
`{ts}POST/trade-api/v2/portfolio/orders`.

```
POST /trade-api/v2/portfolio/orders
{
  "ticker": "...",
  "client_order_id": "<uuid you generate>",
  "side": "yes",
  "action": "buy",        // "sell" to reduce/exit
  "count": 1,
  "type": "limit",
  "yes_price": 42
}

DELETE /trade-api/v2/portfolio/orders/{order_id}
```

**Idempotency invariant:** one `client_order_id` per intent, reused verbatim
on every retry of that intent. A timeout is NOT a failure — treat the order as
possibly-live until `resting_orders()` / the `fills` channel says otherwise.

## Executor architecture (intent reconciliation)

The strategy never calls the API. It emits desired state; the executor owns
all transmission:

```
strategy  ──intents──▶  executor loop  ──POST/DELETE──▶  exchange
                             ▲                              │
                             └────── fills ws + REST ───────┘
```

Loop (every tick):
1. `risk::kill_requested(dir)`? → cancel all, flatten, exit.
2. `DeadMan::is_stale`? (book or game feed) → cancel all resting, stop quoting
   until fresh.
3. Diff desired vs actual per market:
   - want position, none resting → build order → `RiskBook::approve` →
     on `Ok` send; on `Err(veto)` log and skip (never retry around a veto).
   - resting order no longer wanted (edge gone, price moved) → cancel.
4. Apply fills from the ws channel (`parse_fill`): position and cost basis
   change ONLY on fill events — never on send.
   Call `RiskBook::on_exposure_change` / `on_realized` accordingly.
5. Daily stop: `RiskBook::daily_loss_hit()` → cancel all, flatten (sell at
   bid), `risk::write_lockout(dir, now, reason)`, exit process.

## Startup sequence (before anything trades)

1. `risk::is_locked_out(dir, now)` → refuse to run. This is what makes a
   KeepAlive supervisor safe.
2. `PortfolioClient::resting_orders()` → cancel every one.
3. `PortfolioClient::positions()` → adopt into state or flatten
   (`adopt_positions_on_boot` config flag; default flatten).
4. `balance_cents()` → log it; refuse to start if below a floor you set.

## Logging (so existing tooling works)

Write actions to a CSV in the exact paper-trader v2 schema —
`ts,ticker,action,price_cents,size,cost_cents,fair_model_cents,fair_blend_cents,detail,pnl_cents,fees`
with `action` ∈ entry/add/exit_maker/exit_taker/settle. Then:
- `tracker_server.py` can display live results by pointing at the live clone
- `compare_fills.py <shadow.csv> <live.csv>` produces the Gate 5 slippage report

Run the shadow simulator in-process (keep the `SimOrder` path, logging to
`shadow_trades.csv`) for the whole of Phase 5.

## Config additions

`live_enabled` (bool, default false — the binary refuses to send orders
unless explicitly true), `max_exposure_cents`, `per_market_cap_cents`,
`daily_loss_stop_cents`, `max_orders_per_min`, `max_price_distance_cents`,
`adopt_positions_on_boot`, `balance_floor_cents`.

## Gates (from the cutover plan — measurements, not feelings)

- **G2**: place/query/cancel/fill observed on demo from a test binary; retried
  `client_order_id` provably does not double-fill.
- **G3**: `kill -9` mid-position ×5 → clean reconciliation every time.
- **G5**: ≥200 demo trades, positive P&L, shadow-vs-live slippage quantified
  via `compare_fills.py`. Demo books are thinner than prod — read the result
  as a lower bound on real slippage.
- **G6**: 1-contract sizing, supervised week, penny-perfect ledger vs the
  Kalshi UI, scale only by a rule written down in advance.
