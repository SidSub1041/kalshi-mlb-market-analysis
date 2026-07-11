"""In-game win-probability model + model-vs-market discrepancy backtest.

Model: analytic WP from game state (score, inning, half, outs, runners) with
team-strength priors (season RS/RA), calibrated with a walk-forward logistic
layer. Compared against Kalshi ML mid-prices at every play of the collected
season to test whether model/market gaps predict future prices and outcomes
net of fees.

Usage:
  python3 wp_model.py --data kalshi-pipeline/data_full
"""
import argparse, glob, os, re
from collections import defaultdict

import numpy as np
import pandas as pd

# Standard RE24 run-expectancy table: (outs) x (on1,on2,on3) -> expected runs
# rest of half-inning. League-average values.
RE24 = {
    (0,0,0,0):0.481,(0,1,0,0):0.859,(0,0,1,0):1.100,(0,0,0,1):1.350,
    (0,1,1,0):1.437,(0,1,0,1):1.784,(0,0,1,1):1.964,(0,1,1,1):2.292,
    (1,0,0,0):0.254,(1,1,0,0):0.509,(1,0,1,0):0.664,(1,0,0,1):0.950,
    (1,1,1,0):0.884,(1,1,0,1):1.130,(1,0,1,1):1.376,(1,1,1,1):1.541,
    (2,0,0,0):0.098,(2,1,0,0):0.224,(2,0,1,0):0.319,(2,0,0,1):0.353,
    (2,1,1,0):0.429,(2,1,0,1):0.478,(2,0,1,1):0.580,(2,1,1,1):0.752,
}
LEAGUE_HALF_MU = 0.481      # E[runs] per half-inning from clean state
HALF_VAR = 1.12             # Var[runs] per half-inning
HOME_EDGE = 0.035           # home advantage folded into strength diff

def team_ratings(games, finals):
    """Season off/def ratings: runs scored/allowed per game vs league avg."""
    rs, ra, gp = defaultdict(float), defaultdict(float), defaultdict(int)
    for ec, g in games.iterrows():
        f = finals.get(ec)
        if not f: continue
        aw, hm = g['away'], g['home']
        a_r, h_r = f
        rs[aw] += a_r; ra[aw] += h_r; gp[aw] += 1
        rs[hm] += h_r; ra[hm] += a_r; gp[hm] += 1
    lg = sum(rs.values()) / max(1, sum(gp.values()))
    off = {t: (rs[t]/gp[t])/lg for t in gp}
    dfn = {t: (ra[t]/gp[t])/lg for t in gp}
    return off, dfn

def wp_features(row, off, dfn, away, home):
    """z-score of home team's expected final-score margin."""
    inning, half = row['inning'], row['half']
    outs = int(row['outs_after']); lead = row['home_score'] - row['away_score']
    on = (int(row['on1']), int(row['on2']), int(row['on3']))
    if outs >= 3:
        outs, on, mid_inning_done = 0, (0,0,0), True
    else:
        mid_inning_done = False

    o_h, o_a = off.get(home,1.0), off.get(away,1.0)
    d_h, d_a = dfn.get(home,1.0), dfn.get(away,1.0)
    mu_h = LEAGUE_HALF_MU * o_h * d_a    # home runs per half-inning batted
    mu_a = LEAGUE_HALF_MU * o_a * d_h

    # half-innings still to be played from scratch (9-inning regulation)
    inn = min(int(inning), 9)
    if half == 'top':
        if mid_inning_done:   # top just ended -> bottom of this inning pending
            rem_a = 9 - inn
            rem_h = 9 - inn + 1
            partial = 0.0; partial_side = 0
        else:                 # mid top half
            rem_a = 9 - inn
            rem_h = 9 - inn + 1
            partial = RE24.get((outs,)+on, LEAGUE_HALF_MU) * (o_a*d_h)
            partial_side = -1
    else:
        if mid_inning_done:
            rem_a = 9 - inn; rem_h = 9 - inn
            partial = 0.0; partial_side = 0
        else:
            rem_a = 9 - inn; rem_h = 9 - inn
            partial = RE24.get((outs,)+on, LEAGUE_HALF_MU) * (o_h*d_a)
            partial_side = 1

    exp_diff = lead + rem_h*mu_h - rem_a*mu_a + partial_side*partial + HOME_EDGE
    n_half = max(rem_a + rem_h + (0 if partial_side == 0 else 1), 1)
    sigma = np.sqrt(n_half * HALF_VAR)
    return exp_diff / sigma

def load_season(data_dir):
    games = pd.read_csv(os.path.join(data_dir, 'games.csv')).set_index('event_code')
    finals, rows = {}, []
    for f in glob.glob(os.path.join(data_dir, 'plays_*.csv')):
        ec = re.match(r'plays_(.+)\.csv', os.path.basename(f)).group(1)
        if ec not in games.index: continue
        p = pd.read_csv(f)
        if p.empty: continue
        finals[ec] = (p['away_score'].iloc[-1], p['home_score'].iloc[-1])
        p['event_code'] = ec
        rows.append(p)
    plays = pd.concat(rows, ignore_index=True)
    return games, plays, finals

def candle_frames(data_dir, games):
    """event_code -> ML candle df indexed by minute ts, with ticker side team."""
    out = {}
    for ec, g in games.iterrows():
        f = os.path.join(data_dir, f'candles_{ec}_ML.csv')
        if not os.path.exists(f): continue
        c = pd.read_csv(f)
        if c.empty: continue
        c['mid'] = np.where((c['ask_close']>0)&(c['bid_close']>0),
                            (c['ask_close']+c['bid_close'])/2, c['price_mean'])
        out[ec] = c.set_index('ts')
        side = g['ml_ticker'].rsplit('-',1)[-1]
        out[ec].attrs['side_is_home'] = (side == g['home'])
    return out

def kalshi_taker_fee(p):
    """fee in price units (dollars/contract) for taking at price p (0-1)."""
    return np.ceil(7.0 * p * (1.0 - p)) / 100.0

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--data', default='kalshi-pipeline/data_full')
    args = ap.parse_args()

    games, plays, finals = load_season(args.data)
    print(f"{len(finals)} games, {len(plays)} plays")

    # chronological split for ratings + calibration vs evaluation
    games_sorted = games.assign(
        start=pd.to_datetime(games['game_start_utc'])).sort_values('start')
    cut = int(len(games_sorted) * 0.55)
    train_ec = set(games_sorted.index[:cut]); test_ec = set(games_sorted.index[cut:])
    off, dfn = team_ratings(games_sorted.loc[list(train_ec)], finals)

    plays = plays[plays['event_code'].isin(finals)]
    feats, homewin, ecs, ts = [], [], [], []
    for ec, grp in plays.groupby('event_code'):
        g = games.loc[ec]
        hw = 1 if finals[ec][1] > finals[ec][0] else 0
        for _, row in grp.iterrows():
            feats.append(wp_features(row, off, dfn, g['away'], g['home']))
            homewin.append(hw); ecs.append(ec)
            ts.append(pd.Timestamp(row['decisive_time']).timestamp())
    df = pd.DataFrame({'z': feats, 'home_win': homewin, 'event_code': ecs, 'ts': ts})
    df['is_test'] = df['event_code'].isin(test_ec)

    # logistic calibration z -> P(home win), fit on train games only
    from sklearn.linear_model import LogisticRegression
    lr = LogisticRegression(C=1e6)
    tr = df[~df['is_test']]
    lr.fit(tr[['z']], tr['home_win'])
    df['wp_home'] = lr.predict_proba(df[['z']])[:,1]

    te = df[df['is_test']].copy()
    brier_model = ((te['wp_home'] - te['home_win'])**2).mean()
    print(f"calibration a={lr.intercept_[0]:.3f} b={lr.coef_[0][0]:.3f}")
    print(f"model Brier (test, at plays): {brier_model:.4f}  "
          f"[baseline 0.25 = coin flip]")

    # ---- market comparison on test games
    candles = candle_frames(args.data, games)
    recs = []
    for ec, grp in te.groupby('event_code'):
        c = candles.get(ec)
        if c is None: continue
        is_home = c.attrs['side_is_home']
        won = (finals[ec][1] > finals[ec][0]) == is_home
        idx = c.index.values
        for _, r in grp.iterrows():
            t0 = int(r['ts'] // 60 * 60)
            pos = np.searchsorted(idx, t0)
            if pos >= len(idx): continue
            m0 = c['mid'].iloc[pos]
            if not (0.03 < m0 < 0.97): continue
            wp = r['wp_home'] if is_home else 1 - r['wp_home']
            row = {'ec': ec, 'wp': wp, 'mid': m0, 'won': int(won)}
            for h, label in [(10, 'p10'), (30, 'p30')]:
                pos_h = np.searchsorted(idx, t0 + h*60)
                row[label] = c['mid'].iloc[pos_h] if pos_h < len(idx) else (1.0 if won else 0.0)
            recs.append(row)
    mk = pd.DataFrame(recs)
    # The collector kept only the winner-side market (result == "yes"), so the
    # raw panel is 99.9% won=1 — pure survivorship. Symmetrize with the
    # complement side (prices of a binary market mirror around 1) so the panel
    # is unbiased: every snapshot appears once per side, outcomes 50/50.
    comp = mk.copy()
    for col in ['wp', 'mid', 'p10', 'p30']:
        comp[col] = 1.0 - comp[col]
    comp['won'] = 1 - comp['won']
    mk = pd.concat([mk, comp], ignore_index=True)
    print(f"\n{len(mk)} symmetrized snapshots on {mk['ec'].nunique()} test games "
          f"(win rate {mk['won'].mean():.1%})")
    brier_market = ((mk['mid'] - mk['won'])**2).mean()
    brier_model_mk = ((mk['wp'] - mk['won'])**2).mean()
    blend = 0.5*mk['wp'] + 0.5*mk['mid']
    print(f"Brier at snapshots -> market: {brier_market:.4f}   "
          f"model: {brier_model_mk:.4f}   50/50 blend: {((blend-mk['won'])**2).mean():.4f}")

    # ---- does the discrepancy predict anything?
    mk['edge'] = mk['wp'] - mk['mid']
    print("\nedge bucket -> future drift & settle P&L (per contract, $)")
    print(f"{'bucket':>14} {'n':>6} {'drift10m':>9} {'drift30m':>9} "
          f"{'settle':>8} {'settle-fee':>10}")
    for lo, hi in [(-1,-.10),(-.10,-.05),(-.05,-.02),(-.02,.02),
                   (.02,.05),(.05,.10),(.10,1)]:
        b = mk[(mk['edge']>=lo)&(mk['edge']<hi)]
        if len(b) < 30: continue
        sign = np.sign((lo+hi)/2) if abs(lo+hi)>1e-9 else 0
        d10 = (b['p10']-b['mid']); d30 = (b['p30']-b['mid'])
        st  = (b['won']-b['mid'])
        fee = b['mid'].map(kalshi_taker_fee)
        stf = st*sign - fee if sign != 0 else st*0
        print(f"[{lo:+.2f},{hi:+.2f}) {len(b):>6} {d10.mean():>9.4f} "
              f"{d30.mean():>9.4f} {st.mean():>8.4f} {stf.mean():>10.4f}")

if __name__ == '__main__':
    main()
