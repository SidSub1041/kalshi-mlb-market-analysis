# Kalshi MLB Market Microevent Analysis — Pilot Study

**Data:** June 30, 2026 slate. 6 games, 1-minute candlesticks for moneyline (KXMLBGAME) and totals (KXMLBTOTAL) markets, aligned against MLB Stats API play-by-play timestamps. 455 plate-appearance events analyzed. All data in `data/`, methodology in `analyze.py`, per-event results in `data/event_study.csv`.

## How prices react (moneyline, batting team's perspective, cents)

| Event | n | Immediate (0–2 min) | Drift (2–7 min) | Volume spike |
|---|---|---|---|---|
| Home run | 22 | +7.4 | +2.4 | 4.5× |
| Walk / HBP | 45 | +3.8 | +1.5 | 2.5× |
| Single | 76 | +2.8 | +1.6 | 2.3× |
| Double/triple | 10 | +1.9 | −0.7 | 1.6× |
| Strikeout | 98 | −1.5 | +0.2 | 1.4× |
| Out in play | 203 | −1.5 | +0.3 | 1.5× |
| Any scoring play | 46 | +7.8 | +1.5 | 4.4× |

Totals markets react even harder to runs: scoring plays move the over +12.1¢ immediately. Outs apply steady scoreless-pressure, −2.2¢ per out.

## Key findings

**1. The market underreacts — momentum, not overreaction.** On big moves (|ΔP| ≥ 3¢), immediate move and subsequent drift are *positively* correlated (ρ = +0.17, p = 0.033). Fading big moves loses (−0.96¢/event gross); riding them wins (+0.96¢ gross, +0.37¢ net of half-spread). The classic "overreaction fade" does not exist here.

**2. The drift is asymmetric — good news continues, bad news doesn't.** After a positive immediate move, drift averages +2.3¢; after a negative one, ~0. The market fully prices outs instantly but underprices rallies. The cleanest entry: buy the batting team (or the over) in the first 1–2 minutes after a scoring play; the position gains another ~2–3¢ over the next 5–6 minutes on average (scoring-play path: +10.3¢ at min +2 → +11.6¢ at min +8).

**3. Reaction speed leaves a narrow window.** For home runs, ~87% of the 7-minute move completes within 2 minutes, and much of it lands in the same minute as the play. Any live strategy must react in seconds, from pitch-level feeds — not minute candles.

**4. Early innings move more than late.** Mean |immediate| reaction: 4.1¢ in innings 1–3, 3.2¢ in 4–6, 1.2¢ in 7+ (late-game prices are often already near 0/100). Liquidity is deep enough to trade: median spread at event time is 1¢, median 3-minute event volume ≈ $12.5k, ~2× baseline.

## Caveats

Single slate (one day, 6 games) — results need the full-season pass before trusting them. MLB `endTime` stamps lag the decisive moment (a HR is known ~30–60s before the play officially ends), so part of the reaction leaks into the "pre" candle and measured magnitudes *understate* true reactions; one 8th-inning HR even sign-flipped because of this. Event windows overlap (plays every ~2–3 min), so drift estimates carry neighbor contamination. Kalshi taker fees (quadratic, ≈1.75¢ at 50¢) would eat the +0.37¢ ride edge — execution must be maker-side (resting limit orders), which Kalshi doesn't charge fees on.

## Next steps

1. **Scale collection** — Rust collector (async, ~10 req/s public limit) pulling full-season KXMLBGAME/KXMLBTOTAL candles + raw trade ticks, joined to pitch-level GUMBO timestamps to fix the timing leakage. Run locally (Kalshi is unreachable from this sandbox's shell; only single fetches work here).
2. **Feature/model pass** — event type × leverage (inning, score diff, base-out state) × pre-event price; target = drift over 1–10 min horizons; walk-forward validation by week.
3. **Live paper test** — following week's games via Kalshi websocket feed, maker-side entries after scoring plays, measure fill-adjusted PnL.
