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
    """-> closed trades, entries count, orphan exit count, (min_ts, max_ts).

    Partial-fill aware: an exit row closes only its own `size`, consuming
    open clips FIFO, so executors that exit in slices are accounted
    correctly instead of the first exit swallowing the whole position.
    """
    open_clips, closed, n_entries, orphans = {}, [], 0, 0
    lo = hi = None
    for r in csv.DictReader(open(path)):
        act = r.get('action')
        if not act:
            continue
        ts = datetime.fromisoformat(r['ts'].replace('Z', '+00:00'))
        lo = ts if lo is None or ts < lo else lo
        hi = ts if hi is None or ts > hi else hi
        t = r['ticker']
        if act in ('entry', 'add'):
            n_entries += 1
            open_clips.setdefault(t, []).append(
                {'ts': ts, 'price': float(r['price_cents']), 'size': float(r['size'])})
        elif act in ('exit_maker', 'exit_taker', 'settle'):
            clips = open_clips.get(t)
            if not clips:
                orphans += 1
                continue
            want = float(r['size'] or 0) or sum(c['size'] for c in clips)
            take, cost, first_ts = 0.0, 0.0, clips[0]['ts']
            while clips and take < want - 1e-9:
                c = clips[0]
                use = min(c['size'], want - take)
                take += use
                cost += use * c['price']
                c['size'] -= use
                if c['size'] <= 1e-9:
                    clips.pop(0)
            if not clips:
                open_clips.pop(t, None)
            closed.append({
                'ticker': t,
                'entry_ts': first_ts,
                'avg_entry': cost / take if take else 0.0,
                'qty': take,
                'cost': cost,
                'pnl': float(r['pnl_cents'] or 0),
                'exit_mode': act,
            })
    return closed, n_entries, orphans, (lo, hi)


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
    shadow, s_entries, s_orph, s_span = load(shadow_path)
    real, r_entries, r_orph, r_span = load(real_path)
    print("=== totals (entire files) ===")
    summarize('shadow', shadow, s_entries)
    summarize('real', real, r_entries)
    if s_orph or r_orph:
        print(f"orphan exit rows (no open clips; excluded): "
              f"shadow {s_orph}, real {r_orph}")

    # Restrict comparisons to the window covered by BOTH logs.
    if None in (*s_span, *r_span):
        print("\none of the logs is empty — nothing to compare")
        return
    lo, hi = max(s_span[0], r_span[0]), min(s_span[1], r_span[1])
    if lo >= hi:
        print("\nno overlapping time window between the two logs")
        return
    sh = [c for c in shadow if lo <= c['entry_ts'] <= hi]
    re = [c for c in real if lo <= c['entry_ts'] <= hi]
    print(f"\n=== overlap window {lo:%Y-%m-%d %H:%M} -> {hi:%Y-%m-%d %H:%M} UTC ===")
    print(f"closed in window: shadow {len(sh)}, real {len(re)}")
    if sh:
        print(f"fill ratio (real/shadow closed in window): {len(re)/len(sh):.2f} "
              f"(<1.0 means the sim over-promises fills)")

    # One-to-one greedy matching by ticker + nearest entry time (10-min cap),
    # smallest time gaps first, each real trade consumed at most once.
    pairs = []
    for si, s_t in enumerate(sh):
        for ri, r_t in enumerate(re):
            if r_t['ticker'] != s_t['ticker']:
                continue
            gap = abs((r_t['entry_ts'] - s_t['entry_ts']).total_seconds())
            if gap < 600:
                pairs.append((gap, si, ri))
    pairs.sort()
    used_s, used_r, matches = set(), set(), []
    for gap, si, ri in pairs:
        if si in used_s or ri in used_r:
            continue
        used_s.add(si); used_r.add(ri)
        matches.append((sh[si], re[ri]))
    price_delta = sum(r['avg_entry'] - s['avg_entry'] for s, r in matches)
    pnl_delta = sum((r['pnl'] / r['qty'] if r['qty'] else 0)
                    - (s['pnl'] / s['qty'] if s['qty'] else 0)
                    for s, r in matches)
    m = len(matches)
    print(f"\n=== one-to-one matches (same ticker, entries within 10 min): {m} ===")
    print(f"unmatched: shadow {len(sh) - m} (sim filled, reality didn't), "
          f"real {len(re) - m} (reality filled, sim didn't)")
    if m:
        print(f"avg entry price slippage: {price_delta / m:+.2f}c/contract "
              f"(positive = real fills at worse prices; assumes yes-buy entries)")
        print(f"avg per-contract P&L gap:  {pnl_delta / m:+.2f}c/contract "
              f"(negative = reality underperforms the sim)")
        print("\nGate 5 read: the gap is measured only on trades BOTH tracks "
              "filled, so treat it as a lower bound on true slippage — "
              "shadow-only fills above are pure sim optimism on top of it. "
              "If sim edge minus this gap <= 0, the strategy does not survive "
              "real fills.")
    else:
        print("no matches — need overlapping sessions with shared tickers")


if __name__ == '__main__':
    if len(sys.argv) != 3:
        print(__doc__)
        sys.exit(1)
    main(sys.argv[1], sys.argv[2])
