#!/usr/bin/env python3
"""
Feature/model pass: predict post-event price drift on Kalshi MLB moneyline
markets from microevent + leverage + price features, with weekly walk-forward
validation.

Input: a collector output dir (kalshi-pipeline data_full/) or the pilot data/
dir — the script detects which schema it has and degrades gracefully
(pilot data lacks decisive_time / base-out state).

Usage:
  python3 train.py --data data_full --horizons 1 3 5 10
"""
import argparse, math, sys
from pathlib import Path
from datetime import datetime, timezone

import numpy as np
import pandas as pd
from sklearn.ensemble import HistGradientBoostingRegressor
from sklearn.metrics import r2_score

EVENT_GROUPS = {
    "home_run": "home_run", "triple": "xbh", "double": "xbh", "single": "single",
    "walk": "walk_hbp", "hit_by_pitch": "walk_hbp", "intent_walk": "walk_hbp",
    "strikeout": "strikeout", "strikeout_double_play": "strikeout",
    "field_out": "out", "force_out": "out", "grounded_into_double_play": "out",
    "double_play": "out", "sac_fly": "out", "sac_bunt": "out",
    "fielders_choice": "out", "fielders_choice_out": "out",
    "field_error": "error", "error": "error",
}

def iso_to_epoch(s):
    try:
        return datetime.fromisoformat(str(s).replace("Z", "+00:00")).timestamp()
    except Exception:
        return np.nan

def load_manifest(data: Path):
    g = pd.read_csv(data / "games.csv")
    # pilot schema -> collector schema
    if "event_code" not in g.columns:
        g["event_code"] = g["game_code"]
        g["game_start_utc"] = g["game_start_utc"]
    return g

def ml_side_is_home(ml_ticker: str, event_code: str) -> bool:
    """Does the ML ticker track the home team? Ticker suffix vs event teams."""
    side = ml_ticker.rsplit("-", 1)[-1]
    # event_code like DETNYY or DETNYY_26JUN30 -> home = trailing team
    teams = event_code.split("_")[0]
    return teams.endswith(side)

def build_events(data: Path) -> pd.DataFrame:
    games = load_manifest(data)
    rows = []
    for g in games.itertuples():
        code = g.event_code
        cf = data / f"candles_{code}_ML.csv"
        pf = data / f"plays_{code}.csv"
        if not cf.exists() or not pf.exists():
            continue
        cnd = pd.read_csv(cf).sort_values("ts")
        grid = pd.DataFrame({"ts": np.arange(cnd.ts.min(), cnd.ts.max() + 60, 60, dtype=int)})
        cnd = grid.merge(cnd, on="ts", how="left")
        for c in ("price_close", "ask_close", "bid_close"):
            cnd[c] = cnd[c].ffill()
        cnd["volume"] = cnd["volume"].fillna(0)
        cnd = cnd.set_index("ts")
        base_vol = max(cnd.volume.median(), 1e-9)

        plays = pd.read_csv(pf)
        full_schema = "decisive_time" in plays.columns
        tcol = "decisive_time" if full_schema else "end_time"
        plays["t"] = plays[tcol].map(iso_to_epoch)
        plays = plays.dropna(subset=["t"]).sort_values("t")
        side_home = ml_side_is_home(g.ml_ticker, code)

        def px(ts):
            i = cnd.index[cnd.index <= ts]
            return cnd.loc[i[-1], "price_close"] if len(i) else np.nan

        prev = None
        for p in plays.itertuples():
            grp = EVENT_GROUPS.get(p.event_type)
            if grp is None:
                prev = p; continue
            bat_home = p.half != "top"
            sign = 1.0 if bat_home == side_home else -1.0
            t0 = math.ceil(p.t / 60) * 60
            pre = px(t0 - 60)
            p1 = px(t0 + 60)
            if np.isnan(pre) or np.isnan(p1):
                prev = p; continue
            row = dict(
                game=code, t=p.t, week=int(t0 // (7 * 86400)),
                group=grp, inning=min(p.inning, 10), bat_home=int(bat_home),
                # score diff from batting team's perspective, AFTER the play
                score_diff=(p.home_score - p.away_score) * (1 if bat_home else -1),
                rbi=p.rbi,
                pre_price=pre if sign > 0 else 1 - pre,   # batting-team price
                imm_1m=sign * (p1 - pre) * 100,
                vol_ratio=cnd.volume.reindex([t0, t0 + 60]).sum() / (2 * base_vol),
                spread=(cnd.loc[cnd.index <= t0].iloc[-1][["ask_close", "bid_close"]]
                        .diff().abs().iloc[-1]) * 100,
            )
            if full_schema:
                row["outs"] = p.outs_after
                row["runners"] = int(p.on1) + int(p.on2) + int(p.on3)
                # pre-play base state from previous play
                row["runners_pre"] = (int(prev.on1) + int(prev.on2) + int(prev.on3)
                                      if prev is not None and hasattr(prev, "on1") else 0)
            for h in (1, 3, 5, 10):
                ph = px(t0 + 60 * (1 + h))
                row[f"drift_{h}m"] = (sign * (ph - p1) * 100) if not np.isnan(ph) else np.nan
            rows.append(row)
            prev = p
    return pd.DataFrame(rows)

def walk_forward(df: pd.DataFrame, horizon: int, min_train_weeks: int = 3):
    ycol = f"drift_{horizon}m"
    d = df.dropna(subset=[ycol]).copy()
    d = pd.get_dummies(d, columns=["group"], prefix="ev")
    feat = [c for c in d.columns if c.startswith("ev_")] + [
        "inning", "bat_home", "score_diff", "rbi", "pre_price", "imm_1m",
        "vol_ratio", "spread",
    ] + [c for c in ("outs", "runners", "runners_pre") if c in d.columns]
    weeks = sorted(d.week.unique())
    if len(weeks) < min_train_weeks + 1:
        print(f"  [h={horizon}m] only {len(weeks)} week(s) of data — "
              f"walk-forward needs {min_train_weeks + 1}; fitting in-sample diagnostics only")
        m = HistGradientBoostingRegressor(max_depth=3, learning_rate=0.05,
                                          max_iter=300, random_state=0)
        m.fit(d[feat], d[ycol])
        pred = m.predict(d[feat])
        print(f"  in-sample R2={r2_score(d[ycol], pred):.3f} n={len(d)}")
        return None
    preds = []
    for k in range(min_train_weeks, len(weeks)):
        tr = d[d.week.isin(weeks[:k])]
        te = d[d.week == weeks[k]].copy()
        if len(te) < 10:
            continue
        m = HistGradientBoostingRegressor(max_depth=3, learning_rate=0.05,
                                          max_iter=300, random_state=0)
        m.fit(tr[feat], tr[ycol])
        te["pred"] = m.predict(te[feat])
        preds.append(te)
    if not preds:
        return None
    out = pd.concat(preds)
    r2 = r2_score(out[ycol], out.pred)
    # trading metric: act when |pred| clears the half-spread; maker entry
    for thr in (0.5, 1.0, 2.0):
        sel = out[out.pred.abs() >= thr]
        if len(sel) == 0:
            continue
        pnl = (np.sign(sel.pred) * sel[ycol])          # gross cents/contract
        hit = (np.sign(sel.pred) == np.sign(sel[ycol])).mean()
        print(f"  [h={horizon}m] thr={thr:.1f}c n={len(sel):4d} hit={hit:.0%} "
              f"gross={pnl.mean():+.2f}c/evt net(half-spread)="
              f"{(pnl - sel.spread / 2).mean():+.2f}c/evt")
    print(f"  [h={horizon}m] walk-forward R2={r2:.3f} on n={len(out)} oos events")
    return out

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", default="data")
    ap.add_argument("--horizons", nargs="+", type=int, default=[1, 3, 5, 10])
    ap.add_argument("--out", default=None, help="write per-event oos predictions csv")
    args = ap.parse_args()

    data = Path(args.data)
    print(f"building events from {data} ...")
    df = build_events(data)
    if df.empty:
        sys.exit("no events built — check --data dir")
    print(f"{len(df)} events, {df.game.nunique()} games, weeks: {sorted(df.week.unique())}")
    outs = []
    for h in args.horizons:
        res = walk_forward(df, h)
        if res is not None:
            res["horizon"] = h
            outs.append(res)
    if outs and args.out:
        pd.concat(outs).to_csv(args.out, index=False)
        print(f"wrote {args.out}")

if __name__ == "__main__":
    main()
