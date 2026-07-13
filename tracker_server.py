"""Live paper-trading tracker. Serves a self-refreshing dashboard on :8787.

Reads every v2-schema paper_trades*.csv in kalshi-pipeline/ (live file +
archives), reconstructs positions, and renders the xlsx-style layout with
period tabs (Today / 1W / 1M / YTD / All) and a per-day performance table.
The page polls /data.json every 5 s; all filtering happens client-side.

Run: python3 tracker_server.py
"""
import csv, glob, json, os, re
from datetime import datetime, timedelta
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PIPE = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'kalshi-pipeline')
PORT = 8787
ET_OFFSET_H = 4   # UTC -> ET (EDT)

NAMES = {'ATH':'Athletics','ATL':'Braves','AZ':'D-backs','BAL':'Orioles','BOS':'Red Sox',
         'CHC':'Cubs','CIN':'Reds','CLE':'Guardians','COL':'Rockies','CWS':'White Sox',
         'DET':'Tigers','HOU':'Astros','KC':'Royals','LAA':'Angels','LAD':'Dodgers',
         'MIA':'Marlins','MIL':'Brewers','MIN':'Twins','NYM':'Mets','NYY':'Yankees',
         'PHI':'Phillies','PIT':'Pirates','SD':'Padres','SEA':'Mariners','SF':'Giants',
         'STL':'Cardinals','TB':'Rays','TEX':'Rangers','TOR':'Blue Jays','WSH':'Nationals'}

def parse_ticker(t):
    m = re.match(r'KXMLBGAME-(\d{2}[A-Z]{3}\d{2})\d{4}([A-Z]+)-([A-Z]+)$', t)
    if not m:
        return t, ''
    _, teams, side = m.groups()
    for s in (2, 3):
        a, h = teams[:s], teams[s:]
        if a in NAMES and h in NAMES:
            return f"{NAMES[a]} @ {NAMES[h]}", NAMES.get(side, side)
    return t, NAMES.get(side, side)

def load_rows():
    rows = []
    for f in sorted(glob.glob(os.path.join(PIPE, 'paper_trades*.csv'))):
        try:
            with open(f) as fh:
                for r in csv.DictReader(fh):
                    if r.get('action') and r.get('ts'):   # v2 schema only
                        rows.append(r)
        except OSError:
            continue
    rows.sort(key=lambda r: r['ts'])
    return rows

def build():
    open_clips, closed, opens = {}, [], []
    for r in load_rows():
        t = r['ticker']
        act = r['action']
        price = float(r['price_cents'] or 0)
        size = float(r['size'] or 0)
        if act in ('entry', 'add'):
            open_clips.setdefault(t, []).append((r['ts'], price, size, r.get('detail', '')))
        elif act in ('exit_maker', 'exit_taker', 'settle'):
            clips = open_clips.pop(t, [])
            if not clips:
                continue
            cost = sum(p * s for _, p, s, _ in clips)
            qty = sum(s for _, _, s, _ in clips)
            pnl = float(r['pnl_cents'] or 0)
            game, team = parse_ticker(t)
            et = (datetime.fromisoformat(clips[0][0].replace('Z', '+00:00'))
                  - timedelta(hours=ET_OFFSET_H))
            closed.append({
                'date_iso': et.strftime('%Y-%m-%d'),
                'date': et.strftime('%b %d'), 'time': et.strftime('%I:%M %p'),
                'game': game, 'team': team, 'state': clips[0][3],
                'clips': len(clips), 'qty': qty,
                'avg_entry': round(cost / qty, 1) if qty else 0,
                'exit': price, 'mode': act.replace('exit_', ''),
                'invested': round(cost / 100, 2), 'pnl': pnl,
                'ret': round(pnl / cost * 100, 1) if cost else 0,
                'fees': float(r['fees'] or 0),
            })
    now = datetime.now().astimezone()
    for t, clips in open_clips.items():
        game, team = parse_ticker(t)
        cost = sum(p * s for _, p, s, _ in clips)
        qty = sum(s for _, _, s, _ in clips)
        last_ts = datetime.fromisoformat(clips[-1][0].replace('Z', '+00:00'))
        stale = (now - last_ts).total_seconds() > 4 * 3600
        opens.append({'game': game, 'team': team, 'clips': len(clips), 'qty': qty,
                      'avg_entry': round(cost / qty, 1) if qty else 0,
                      'invested': round(cost / 100, 2),
                      'state': clips[-1][3] + (' — orphaned (restart)' if stale else '')})
    today_iso = (datetime.utcnow() - timedelta(hours=ET_OFFSET_H)).strftime('%Y-%m-%d')
    return {'closed': closed, 'open': opens, 'today_iso': today_iso,
            'updated': datetime.now().strftime('%I:%M:%S %p')}

PAGE = """<!doctype html><html><head><meta charset="utf-8">
<title>Kalshi Paper-Trading Tracker</title>
<style>
 body{font-family:Arial,sans-serif;margin:0;background:#fff;color:#000}
 .title{background:#1F3864;color:#fff;text-align:center;font-size:22px;
        font-weight:bold;padding:14px}
 .sub{color:#404040;font-style:italic;text-align:center;font-size:12px;margin:8px 20px}
 .tabs{display:flex;justify-content:center;gap:4px;margin:12px}
 .tab{padding:7px 20px;border:1px solid #1F3864;color:#1F3864;cursor:pointer;
      font-weight:bold;font-size:13px;border-radius:3px}
 .tab.on{background:#1F3864;color:#fff}
 .kpis{display:flex;flex-wrap:wrap;gap:8px;justify-content:center;margin:14px}
 .kpi{min-width:96px;text-align:center;border:1px solid #BFBFBF}
 .kpi .h{background:#44546A;color:#fff;font-size:11px;font-weight:bold;padding:5px}
 .kpi .v{background:#D9E2F3;font-size:16px;font-weight:bold;padding:8px}
 .good{color:#006100}.bad{color:#9C0006}
 table{border-collapse:collapse;margin:10px auto;font-size:12px}
 th{background:#1F3864;color:#fff;padding:6px 9px}
 td{border:1px solid #BFBFBF;padding:5px 9px;text-align:center}
 tr:nth-child(even) td{background:#F2F2F2}
 td.win{background:#C6EFCE;color:#006100;font-weight:bold}
 td.loss{background:#FFC7CE;color:#9C0006;font-weight:bold}
 .sec{color:#1F3864;font-weight:bold;font-size:15px;margin:18px 0 4px;text-align:center}
 .upd{color:#888;font-size:11px;text-align:center;margin:10px}
 .note{color:#888;font-size:11px;text-align:center}
 svg{display:block;margin:6px auto}
</style></head><body>
<div class="title">KALSHI MLB PAPER-TRADING RESULTS — LIVE</div>
<div class="sub">Fair-value model (v2). Simulated money. Auto-refreshes every 5 seconds
 from the trader's live CSV logs.</div>
<div class="tabs" id="tabs"></div>
<div class="kpis" id="kpis"></div>
<div class="sec">Cumulative P&amp;L ($) — <span id="chartlbl"></span></div>
<svg id="chart" width="640" height="150"></svg>
<div class="sec">Performance by Day (all history)</div><table id="days"></table>
<div class="note">Judge the model day by day — early days ran older builds
 (see the Jul 11 Rockies outlier).</div>
<div class="sec">Open Positions</div><table id="open"></table>
<div class="sec">Closed Trades — <span id="loglbl"></span></div><table id="log"></table>
<div class="upd" id="upd"></div>
<script>
const $=id=>document.getElementById(id);
const money=v=>(v<0?'-$':'$')+Math.abs(v).toFixed(2);
const PERIODS=['Today','1W','1M','YTD','All'];
let period=localStorage.getItem('kt_period')||'Today';
let last=null;

function inPeriod(d,today){
 if(period==='All')return true;
 if(period==='Today')return d===today;
 const days={'1W':7,'1M':30}[period];
 if(days){
  const lim=new Date(today);lim.setDate(lim.getDate()-days+1);
  return d>=lim.toISOString().slice(0,10);
 }
 return d.slice(0,4)===today.slice(0,4);   // YTD
}
function stats(rows){
 const pnls=rows.map(c=>c.pnl),wins=pnls.filter(p=>p>0),losses=pnls.filter(p=>p<0);
 const cost=rows.reduce((a,c)=>a+c.invested,0),tot=pnls.reduce((a,b)=>a+b,0);
 return {pnl:tot/100,cost,ret:cost?100*tot/100/cost:0,n:rows.length,
  w:wins.length,l:losses.length,
  wr:rows.length?100*wins.length/rows.length:0,
  aw:wins.length?wins.reduce((a,b)=>a+b,0)/wins.length/100:0,
  al:losses.length?losses.reduce((a,b)=>a+b,0)/losses.length/100:0,
  pf:losses.length?wins.reduce((a,b)=>a+b,0)/Math.abs(losses.reduce((a,b)=>a+b,0)):0,
  fees:rows.reduce((a,c)=>a+c.fees,0),
  best:pnls.length?Math.max(...pnls)/100:0,worst:pnls.length?Math.min(...pnls)/100:0};
}
function kpi(h,v,cls){return `<div class="kpi"><div class="h">${h}</div>`+
 `<div class="v ${cls||''}">${v}</div></div>`}
function render(){
 if(!last)return;
 const d=last,today=d.today_iso;
 $('tabs').innerHTML=PERIODS.map(p=>
  `<div class="tab ${p===period?'on':''}" onclick="setP('${p}')">${p}</div>`).join('');
 const rows=d.closed.filter(c=>inPeriod(c.date_iso,today));
 const k=stats(rows);
 $('kpis').innerHTML=
  kpi('Total P&L',money(k.pnl),k.pnl>=0?'good':'bad')+
  kpi('Deployed',money(k.cost))+
  kpi('Return',k.ret.toFixed(1)+'%',k.ret>=0?'good':'bad')+
  kpi('Trades',`${k.n} (${k.w}W/${k.l}L)`)+
  kpi('Win Rate',k.wr.toFixed(0)+'%')+
  kpi('Avg Win',money(k.aw),'good')+
  kpi('Avg Loss',money(k.al),'bad')+
  kpi('Profit Factor',k.pf.toFixed(2)+'x')+
  kpi('Fees',money(k.fees))+
  kpi('Best',money(k.best),'good')+
  kpi('Worst',money(k.worst),'bad');
 $('chartlbl').textContent=period;$('loglbl').textContent=period;
 const svg=$('chart');let acc=0;const cum=rows.map(c=>(acc+=c.pnl/100,acc));
 if(cum.length>1){
  const W=620,H=130,mn=Math.min(0,...cum),mx=Math.max(0,...cum);
  const x=i=>10+i*(W-20)/(cum.length-1),y=v=>10+(mx-v)*(H-20)/((mx-mn)||1);
  svg.innerHTML=`<line x1="10" y1="${y(0)}" x2="${W-10}" y2="${y(0)}"
   stroke="#BFBFBF"/><polyline fill="none" stroke="#1F3864" stroke-width="2"
   points="${cum.map((v,i)=>x(i)+','+y(v)).join(' ')}"/>`;
 } else svg.innerHTML='';
 const byday={};
 d.closed.forEach(c=>{(byday[c.date_iso]=byday[c.date_iso]||[]).push(c)});
 $('days').innerHTML=
  '<tr><th>Day</th><th>Trades</th><th>W/L</th><th>Deployed</th><th>P&L</th>'+
  '<th>Return</th><th>Win Rate</th><th>Worst Trade</th></tr>'+
  Object.keys(byday).sort().reverse().map(day=>{
   const s=stats(byday[day]);
   const cls=s.pnl>0?'win':(s.pnl<0?'loss':'');
   return `<tr><td>${day}${day===today?' (today)':''}</td><td>${s.n}</td>`+
    `<td>${s.w}/${s.l}</td><td>${money(s.cost)}</td>`+
    `<td class="${cls}">${money(s.pnl)}</td><td class="${cls}">${s.ret.toFixed(1)}%</td>`+
    `<td>${s.wr.toFixed(0)}%</td><td>${money(s.worst)}</td></tr>`;
  }).join('');
 $('open').innerHTML=d.open.length?
  '<tr><th>Game</th><th>Team</th><th>Clips</th><th>Contracts</th>'+
  '<th>Avg Entry ¢</th><th>Invested</th><th>Last State</th></tr>'+
  d.open.map(o=>`<tr><td>${o.game}</td><td>${o.team}</td><td>${o.clips}</td>`+
   `<td>${o.qty}</td><td>${o.avg_entry}</td><td>${money(o.invested)}</td>`+
   `<td>${o.state}</td></tr>`).join('')
  :'<tr><td>none</td></tr>';
 $('log').innerHTML=rows.length?
  '<tr><th>#</th><th>Date</th><th>Time (ET)</th><th>Game</th><th>Team Bought</th>'+
  '<th>State at Entry</th><th>Clips</th><th>Avg Entry ¢</th><th>Exit ¢</th>'+
  '<th>Exit Type</th><th>Invested</th><th>P&L ¢</th><th>Return</th><th>Result</th></tr>'+
  rows.map((c,i)=>{
   const cls=c.pnl>0?'win':(c.pnl<0?'loss':'');
   return `<tr><td>${i+1}</td><td>${c.date}</td><td>${c.time}</td><td>${c.game}</td>`+
    `<td>${c.team}</td><td>${c.state}</td><td>${c.clips}</td><td>${c.avg_entry}</td>`+
    `<td>${c.exit}</td><td>${c.mode}</td><td>${money(c.invested)}</td>`+
    `<td class="${cls}">${c.pnl>0?'+':''}${c.pnl}</td>`+
    `<td class="${cls}">${c.ret>0?'+':''}${c.ret}%</td>`+
    `<td class="${cls}">${c.pnl>0?'WIN':(c.pnl<0?'LOSS':'FLAT')}</td></tr>`;
  }).join('')
  :'<tr><td>no closed trades in this period</td></tr>';
 $('upd').textContent='Last updated '+d.updated;
}
function setP(p){period=p;localStorage.setItem('kt_period',p);render()}
async function tick(){
 try{last=await (await fetch('/data.json',{cache:'no-store'})).json()}catch(e){return}
 render();
}
tick();setInterval(tick,5000);
</script></body></html>"""

class H(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith('/data.json'):
            body = json.dumps(build()).encode()
            ctype = 'application/json'
        else:
            body = PAGE.encode()
            ctype = 'text/html; charset=utf-8'
        self.send_response(200)
        self.send_header('Content-Type', ctype)
        self.send_header('Cache-Control', 'no-store')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass

if __name__ == '__main__':
    print(f'tracker on http://localhost:{PORT}')
    ThreadingHTTPServer(('127.0.0.1', PORT), H).serve_forever()
