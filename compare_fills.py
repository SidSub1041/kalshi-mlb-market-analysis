"""Gate 5 slippage report: shadow simulator vs real (demo or live) executor.

Both inputs use the paper-trader v2 CSV schema:
  ts,ticker,action,price_cents,size,cost_cents,fair_model_cents,
  fair_blend_cents,detail,pnl_cents,fees

Usage:
  python3 compare_fills.py shadow_trades.csv demo_trades.csv
"""
import csv, sys
from collections import defaultdict
from datetime import datetime


def load(path):
    """-> closed trades [{ticker, entry_ts, avg_entry, qty, pnl, cost}], entries count"""
    open_clips, closed, n_entries = {}, [], 0
    for r in csv.DictReader(open(path)):
        act = r.get('action')
        if not act:
            continue
        t = r['ticker']
        if act in ('entry', 'add'):
            n_entries += 1
            open_clips.setdefault(t, []).append(r)
        elif act in ('exit_maker', 'exit_taker', 'settle'):
            clips = open_clips.pop(t, [])
            if not clips:
                continue
            cost = sum(float(c['price_cents']) * float(c['size']) for c in clips)
            qty = sum(float(c['size']) for c in clips)
            closed.append({
                'ticker': t,
                'entry_ts': datetime.fromisoformat(clips[0]['ts'].replace('Z', '+00:00')),
                'avg_entry': cost / qty if qty else 0.0,
                'qty': qty,
                'cost': cost,
                'pnl': float(r['pnl_cents'] or 0),
                'exit_mode': act,
            })
    return closed, n_entries


def summarize(label, closed, n_entries):
    pnl = sum(c['pnl'] for c in closed)
    cost = sum(c['cost'] for c in closed)
    w = sum(c['pnl'] > 0 for c in closed)
    l = sum(c['pnl'] < 0 for c in closed)
    print(f"{label:>8}: {n_entries} entries -> {len(closed)} closed ({w}W/{l}L)  "
          f"pnl {pnl:+.0f}c on {cost:.0f}c deployed "
          f"({pnl / cost * 100 if cost else 0:+.1f}%)")
    return pnl, cost


def main(shadow_path, real_path):
    shadow, s_entries = load(shadow_path)
    real, r_entries = load(real_path)
    print("=== totals ===")
    s_pnl, s_cost = summarize('shadow', shadow, s_entries)
    r_pnl, r_cost = summarize('real', real, r_entries)

    print("\n=== fill rate ===")
    if s_entries:
        print(f"real entries per shadow entry: {r_entries / s_entries:.2f} "
              f"(<1.0 means the sim over-promises fills)")

    # Match closed trades by ticker + nearest entry time (within 10 min).
    by_ticker = defaultdict(list)
    for c in real:
        by_ticker[c['ticker']].append(c)
    matched, price_delta, pnl_delta = 0, 0.0, 0.0
    for s in shadow:
        cands = [r for r in by_ticker[s['ticker']]
                 if abs((r['entry_ts'] - s['entry_ts']).total_seconds()) < 600]
        if not cands:
            continue
        r = min(cands, key=lambda r: abs((r['entry_ts'] - s['entry_ts']).total_seconds()))
        matched += 1
        price_delta += r['avg_entry'] - s['avg_entry']
        pnl_delta += (r['pnl'] / r['qty'] if r['qty'] else 0) - \
                     (s['pnl'] / s['qty'] if s['qty'] else 0)
    print(f"\n=== matched round trips (same ticker, entries within 10 min): {matched} ===")
    if matched:
        print(f"avg entry price slippage: {price_delta / matched:+.2f}c/contract "
              f"(positive = real fills at worse prices)")
        print(f"avg per-contract P&L gap:  {pnl_delta / matched:+.2f}c/contract "
              f"(negative = reality underperforms the sim)")
        print("\nGate 5 read: apply the P&L gap to the sim's historical edge — "
              "if edge minus gap <= 0, the strategy does not survive real fills.")
    else:
        print("no matches — need overlapping sessions of both logs")


if __name__ == '__main__':
    if len(sys.argv) != 3:
        print(__doc__)
        sys.exit(1)
    main(sys.argv[1], sys.argv[2])
