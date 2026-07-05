#!/usr/bin/env python3
"""
Kalshi MLB pilot event study — June 30, 2026 slate (6 games).

Measures how Kalshi moneyline (KXMLBGAME) and totals (KXMLBTOTAL) prices react
to in-game microevents (hits, HRs, walks, strikeouts, outs, scoring plays):
  - immediate reaction (0-2 min after play end)
  - drift/reversion (2-7 min after)
  - overreaction test: corr(immediate move, subsequent drift)
  - volume response and spreads around events

Run: python3 analyze.py [data_dir]
"""
import sys, math
from pathlib import Path
from datetime import datetime, timezone

import numpy as np
import pandas as pd
from scipy import stats

DATA = Path(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).parent / "data")

# market side of the ML ticker: which team's win the YES price tracks (+ home/away)
ML_SIDE = {  # game_code: (team, 'home'|'away')
    "DETNYY": ("DET", "away"), "PITPHI": ("PHI", "home"), "TBKC": ("TB", "away"),
    "MINHOU": ("HOU", "home"), "SDCHC": ("CHC", "home"), "LAASEA": ("SEA", "home"),
}

EVENT_GROUPS = {
    "home_run": "home_run",
    "triple": "xbh", "double": "xbh",
    "single": "single",
    "walk": "walk_hbp", "hit_by_pitch": "walk_hbp", "intent_walk": "walk_hbp",
    "strikeout": "strikeout", "strikeout_double_play": "strikeout",
    "field_out": "out_in_play", "force_out": "out_in_play", "grounded_into_double_play": "out_in_play",
    "double_play": "out_in_play", "sac_fly": "out_in_play", "sac_bunt": "out_in_play",
    "fielders_choice": "out_in_play", "fielders_choice_out": "out_in_play", "field_error": "error",
    "error": "error",
}

def iso_to_epoch(s):
    return datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()

def load_candles(path):
    df = pd.read_csv(path)
    df = df.sort_values("ts").reset_index(drop=True)
    # continuous 1-min grid, forward-fill gaps
    grid = pd.DataFrame({"ts": np.arange(df.ts.min(), df.ts.max() + 60, 60, dtype=int)})
    df = grid.merge(df, on="ts", how="left")
    df["gap"] = df.price_close.isna()
    for c in ["price_close", "price_mean", "ask_close", "bid_close"]:
        df[c] = df[c].ffill()
    df["volume"] = df["volume"].fillna(0.0)
    df["spread"] = df.ask_close - df.bid_close
    return df.set_index("ts")

def price_at(cnd, ts):
    """close of the last candle ending at or before ts (ts need not be on grid)"""
    idx = cnd.index[cnd.index <= ts]
    return cnd.loc[idx[-1], "price_close"] if len(idx) else np.nan

rows = []
vol_rows = []
games = pd.read_csv(DATA / "games.csv")

for g in games.itertuples():
    code = g.game_code
    ml = load_candles(DATA / f"candles_{code}_ML.csv")
    tot = load_candles(DATA / f"candles_{code}_TOT.csv")
    plays = pd.read_csv(DATA / f"plays_{code}.csv")
    plays["t"] = plays.end_time.map(iso_to_epoch)
    plays["run_delta"] = (plays.away_score + plays.home_score).diff().fillna(
        plays.away_score + plays.home_score)
    side_team, side_ha = ML_SIDE[code]
    base_vol = ml.volume.median()

    for p in plays.itertuples():
        grp = EVENT_GROUPS.get(p.event_type)
        if grp is None:
            continue
        batting = "away" if p.half == "top" else "home"
        sign = 1.0 if batting == side_ha else -1.0  # +ΔP(YES) good for batting team?

        t0 = math.ceil(p.t / 60) * 60           # first candle boundary at/after play end
        pre = price_at(ml, t0 - 60)             # baseline: candle closing before play end
        p2, p7 = price_at(ml, t0 + 120), price_at(ml, t0 + 420)
        if np.isnan(pre) or np.isnan(p2) or np.isnan(p7):
            continue
        # ΔP from the batting team's perspective, in cents
        imm = sign * (p2 - pre) * 100
        drift = sign * (p7 - p2) * 100

        tpre = price_at(tot, t0 - 60); tp2 = price_at(tot, t0 + 120); tp7 = price_at(tot, t0 + 420)
        timm = (tp2 - tpre) * 100 if not np.isnan(tpre) and not np.isnan(tp2) else np.nan
        tdrift = (tp7 - tp2) * 100 if not np.isnan(tp7) and not np.isnan(tp2) else np.nan

        v_evt = ml.volume.reindex(np.arange(t0, t0 + 180, 60)).sum()
        spr = ml.spread.reindex(np.arange(t0, t0 + 180, 60)).mean()

        rows.append(dict(game=code, t=p.t, inning=p.inning, half=p.half, group=grp,
                         event=p.event_type, rbi=p.rbi, scoring=(str(p.is_scoring).lower() == "true"),
                         run_delta=p.run_delta, pre=pre, imm_c=imm, drift_c=drift,
                         tot_imm_c=timm, tot_drift_c=tdrift,
                         vol_3min=v_evt, vol_ratio=v_evt / (3 * base_vol) if base_vol else np.nan,
                         spread_c=spr * 100))

df = pd.DataFrame(rows)
df.to_csv(DATA / "event_study.csv", index=False)

def summarize(d, label):
    out = []
    for grp, sub in d.groupby("group"):
        n = len(sub)
        imm, drift = sub.imm_c, sub.drift_c
        tt = stats.ttest_1samp(imm, 0) if n > 2 else None
        # overreaction: corr(imm, drift)
        rho, rp = (stats.pearsonr(imm, drift) if n > 4 else (np.nan, np.nan))
        out.append(dict(segment=label, group=grp, n=n,
                        imm_mean=imm.mean(), imm_med=imm.median(), imm_sd=imm.std(),
                        imm_p=tt.pvalue if tt else np.nan,
                        drift_mean=drift.mean(), rev_rho=rho, rev_p=rp,
                        vol_ratio=sub.vol_ratio.median(), spread_c=sub.spread_c.mean(),
                        tot_imm=sub.tot_imm_c.mean()))
    return pd.DataFrame(out)

print("=" * 100)
print("EVENT STUDY - ML price reaction (cents, from batting team's perspective)")
print("=" * 100)
s_all = summarize(df, "all")
print(s_all.round(2).to_string(index=False))

# scoring vs non-scoring
print("\n--- scoring plays vs not ---")
for lab, sub in [("scoring", df[df.scoring]), ("non-scoring", df[~df.scoring])]:
    print(f"{lab:12s} n={len(sub):3d} imm={sub.imm_c.mean():+.2f}c drift={sub.drift_c.mean():+.2f}c "
          f"tot_imm={sub.tot_imm_c.mean():+.2f}c vol_ratio={sub.vol_ratio.median():.1f}")

# leverage: late innings vs early
print("\n--- by inning stage ---")
for lab, sub in [("inn 1-3", df[df.inning <= 3]), ("inn 4-6", df[(df.inning > 3) & (df.inning <= 6)]),
                 ("inn 7+", df[df.inning >= 7])]:
    print(f"{lab:8s} n={len(sub):3d} |imm|={sub.imm_c.abs().mean():.2f}c "
          f"imm={sub.imm_c.mean():+.2f}c drift={sub.drift_c.mean():+.2f}c")

# overreaction / momentum on big moves
print("\n--- conditional on immediate move size (all events) ---")
for lab, sub in [("big |imm|>=3c", df[df.imm_c.abs() >= 3]), ("small |imm|<3c", df[df.imm_c.abs() < 3])]:
    if len(sub) > 4:
        rho, rp = stats.pearsonr(sub.imm_c, sub.drift_c)
        # sign agreement: continuation vs reversal
        cont = ((sub.imm_c * sub.drift_c) > 0).mean()
        print(f"{lab:15s} n={len(sub):3d} corr(imm,drift)={rho:+.2f} (p={rp:.3f}) "
              f"continuation-rate={cont:.0%} mean drift given imm>0: "
              f"{sub[sub.imm_c > 0].drift_c.mean():+.2f}c, imm<0: {sub[sub.imm_c < 0].drift_c.mean():+.2f}c")

# HR reaction speed: how much of total 7-min move happens in first 2 min
hr = df[df.group == "home_run"].copy()
if len(hr):
    tot_move = hr.imm_c + hr.drift_c
    frac = (hr.imm_c / tot_move.replace(0, np.nan)).median()
    print(f"\nHR reaction: median {frac:.0%} of the 7-min move is done within 2 min "
          f"(n={len(hr)}, mean imm={hr.imm_c.mean():+.2f}c)")

# totals market reaction to run-scoring
print("\n--- TOTALS (over) reaction ---")
sc = df[df.run_delta > 0]
nsc = df[df.run_delta == 0]
print(f"runs scored:   n={len(sc):3d} tot_imm={sc.tot_imm_c.mean():+.2f}c tot_drift={sc.tot_drift_c.mean():+.2f}c")
print(f"no runs:       n={len(nsc):3d} tot_imm={nsc.tot_imm_c.mean():+.2f}c tot_drift={nsc.tot_drift_c.mean():+.2f}c")
ks = df[(df.group == "strikeout") | (df.group == "out_in_play")]
print(f"outs (K+field):n={len(ks):3d} tot_imm={ks.tot_imm_c.mean():+.2f}c  (scoreless-pressure on the over)")

# spreads & tradability
print("\n--- microstructure ---")
print(f"median ML spread at event time: {df.spread_c.median():.1f}c | "
      f"median 3-min event volume: ${df.vol_3min.median():,.0f} | "
      f"median vol_ratio vs game baseline: {df.vol_ratio.median():.1f}x")

# simple strategy backtests (fade big moves vs ride momentum), 5c cost per round trip assumed 0 first
print("\n--- toy strategies (per-event PnL in cents, ignoring fees/spread; then spread-adjusted) ---")
big = df[df.imm_c.abs() >= 3].copy()
if len(big):
    fade = (-np.sign(big.imm_c) * big.drift_c)
    ride = (np.sign(big.imm_c) * big.drift_c)
    half_spread = big.spread_c / 2
    print(f"FADE big move : n={len(big)} gross={fade.mean():+.2f}c/event net(spread)={(fade - half_spread).mean():+.2f}c")
    print(f"RIDE big move : n={len(big)} gross={ride.mean():+.2f}c/event net(spread)={(ride - half_spread).mean():+.2f}c")

s_all.round(3).to_csv(DATA / "event_summary.csv", index=False)
print(f"\nSaved: {DATA/'event_study.csv'} ({len(df)} events), {DATA/'event_summary.csv'}")
