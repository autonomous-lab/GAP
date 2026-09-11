"""Operator reporting projections. Never alter wallet balances or debit carry."""
import json
import re
from fractions import Fraction

MONTH_SECONDS = 730 * 3600
FIELDS = ('vcpu_ms','ram_byte_ms','disk_byte_ms','bytes_in','bytes_out',
          'estimated_microcredits','debited_microcredits','unpaid_microcredits')
COST_FIELDS = ('node_month_microdollars','extra_disk_month_microdollars',
               'network_in_gb_microdollars','network_out_gb_microdollars')


def schema(db):
    db.executescript('''
    CREATE TABLE IF NOT EXISTS finance_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS finance_hours(project TEXT NOT NULL,hour INTEGER NOT NULL,
        data TEXT NOT NULL,historical INTEGER NOT NULL,PRIMARY KEY(project,hour));
    CREATE TABLE IF NOT EXISTS finance_costs(version TEXT PRIMARY KEY,effective INTEGER NOT NULL,payload TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS finance_funding(project TEXT NOT NULL,operation TEXT NOT NULL,
        source TEXT NOT NULL,cash_microdollars INTEGER NOT NULL,note TEXT NOT NULL,created INTEGER NOT NULL,
        PRIMARY KEY(project,operation));
    ''')


def record(db, project, result, start, end, historical=False):
    """Split an interval into UTC hours; conserve every integer by cumulative division.

    Meter totals do not identify packet timestamps. Within an interval, network
    bytes and corresponding debit are apportioned by elapsed time, not claimed
    as exact individual request times. Historical periods are explicitly marked.
    """
    start, end = round(start * 1000), round(end * 1000)
    end = max(start + 1, end)
    duration = end - start
    amounts = {k:int(result.get('usage',{}).get(k,result.get(k,0))) for k in FIELDS}
    cursor = start
    while cursor < end:
        hour = cursor // 3600000
        until = min(end, (hour+1)*3600000)
        part = {k:v*(until-start)//duration-v*(cursor-start)//duration for k,v in amounts.items()}
        old = db.execute('SELECT data,historical FROM finance_hours WHERE project=? AND hour=?',(project,hour)).fetchone()
        if old:
            previous=json.loads(old['data'])
            part={k:previous.get(k,0)+v for k,v in part.items()}
        db.execute('INSERT OR REPLACE INTO finance_hours VALUES(?,?,?,?)',
                   (project,hour,json.dumps(part),int(bool(historical or (old and old['historical'])))))
        cursor=until


def initialize(db, now):
    if db.execute("SELECT 1 FROM finance_meta WHERE key='initialized_at'").fetchone(): return
    for row in db.execute('SELECT project,payload,created FROM entries'):
        item=json.loads(row['payload'])
        if item.get('kind')!='usage': continue
        end=item.get('ended_at',row['created'])
        record(db,row['project'],item,item.get('started_at',end),end,True)
    db.execute("INSERT INTO finance_meta VALUES('initialized_at',?)",(str(int(now)),))


def set_costs(db, values, expected, now):
    if not isinstance(values,dict) or set(values)!={'version',*COST_FIELDS}: raise ValueError('invalid_cost_configuration')
    version=values['version']
    if not isinstance(version,str) or not re.fullmatch(r'[A-Za-z0-9_.-]{1,64}',version): raise ValueError('invalid_cost_version')
    for field in COST_FIELDS:
        amount=values[field]
        if amount is not None and (type(amount) is not int or not 0<=amount<=10**12): raise ValueError('invalid_cost_amount')
    encoded=json.dumps(values,sort_keys=True)
    latest=db.execute('SELECT version FROM finance_costs ORDER BY effective DESC,rowid DESC LIMIT 1').fetchone()
    if (latest['version'] if latest else None)!=expected: raise ValueError('cost_version_changed_refresh_before_retry')
    old=db.execute('SELECT payload FROM finance_costs WHERE version=?',(version,)).fetchone()
    if old:
        if old['payload']!=encoded: raise ValueError('immutable_cost_version')
        return values
    db.execute('INSERT INTO finance_costs VALUES(?,?,?)',(version,int(now),encoded))
    return values


def funding(db,project,operation,source,cash,note,now):
    if source not in ('paid','promotional') or type(cash) is not int or not 0<=cash<=10**12: raise ValueError('invalid_funding_classification')
    if (source=='promotional' and cash!=0) or (source=='paid' and cash==0): raise ValueError('invalid_cash_amount')
    if not isinstance(note,str) or not note.strip() or len(note)>1000: raise ValueError('funding_note_required')
    row=db.execute('SELECT payload FROM entries WHERE project=? AND operation=?',(project,operation)).fetchone()
    if not row or json.loads(row['payload']).get('kind')!='topup': raise ValueError('credit_addition_not_found')
    old=db.execute('SELECT source,cash_microdollars,note FROM finance_funding WHERE project=? AND operation=?',(project,operation)).fetchone()
    if old:
        if tuple(old)!=(source,cash,note): raise ValueError('immutable_funding_classification')
        return
    db.execute('INSERT INTO finance_funding VALUES(?,?,?,?,?,?)',(project,operation,source,cash,note,int(now)))


def report(db,start,end,project=None):
    if type(start) is not int or type(end) is not int or start<0 or end<=start or end-start>24*366*3600: raise ValueError('invalid_finance_window')
    if start%3600 or end%3600: raise ValueError('finance_window_requires_utc_hours')
    if project is not None and (not isinstance(project,str) or not re.fullmatch(r'prj_[0-9a-f]{24}',project)):raise ValueError('invalid_finance_project')
    hours=[];totals={k:0 for k in FIELDS};historical=False
    rows=db.execute('SELECT hour,data,historical FROM finance_hours WHERE hour>=? AND hour<? AND (? IS NULL OR project=?) ORDER BY hour',(start//3600,end//3600,project,project))
    combined={}
    for row in rows:
        values=combined.setdefault(row['hour'],{k:0 for k in FIELDS})
        data=json.loads(row['data'])
        for k in FIELDS:values[k]+=data[k];totals[k]+=data[k]
        historical=historical or bool(row['historical'])
    versions=[dict(r) for r in db.execute('SELECT version,effective,payload FROM finance_costs ORDER BY effective,rowid')]
    cost=Fraction(0);complete=True
    for hour in range(start//3600,end//3600):
        amounts=combined.get(hour,{k:0 for k in FIELDS});hour_cost=Fraction(0);known=True
        a,b=hour*3600,(hour+1)*3600
        boundaries=sorted({a,b,*[r['effective'] for r in versions if a<r['effective']<b]})
        for left,right in zip(boundaries,boundaries[1:]):
            effective=[r for r in versions if r['effective']<=left]
            if not effective:known=False;continue
            prices=json.loads(effective[-1]['payload']);span=right-left
            if any(prices[k] is None for k in COST_FIELDS):known=False
            for key in COST_FIELDS[:2]:
                if prices[key] is not None:hour_cost+=Fraction(prices[key]*span,MONTH_SECONDS)
            for key,usage in [('network_in_gb_microdollars','bytes_in'),('network_out_gb_microdollars','bytes_out')]:
                if prices[key] is not None:hour_cost+=Fraction(prices[key]*amounts[usage]*span,10**9*3600)
        allocated_cost=int(cost+hour_cost)-int(cost)
        cost+=hour_cost;complete=complete and known
        hours.append({'hour':a,**amounts,'known_infra_cost_microdollars':allocated_cost if project is None else None,
                      'cost_complete':known if project is None else False,'usage_margin_microdollars':amounts['debited_microcredits']-allocated_cost if known and project is None else None})
    receipts={'paid_credits_microcredits':0,'promotional_credits_microcredits':0,'unclassified_credits_microcredits':0,'recorded_cash_microdollars':0}
    rows=db.execute('SELECT e.payload,f.source,f.cash_microdollars FROM entries e LEFT JOIN finance_funding f ON f.project=e.project AND f.operation=e.operation WHERE e.created>=? AND e.created<? AND (? IS NULL OR e.project=?)',(start,end,project,project))
    for row in rows:
        entry=json.loads(row['payload'])
        if entry.get('kind')!='topup':continue
        source=row['source'] or 'unclassified'
        receipts[source+'_credits_microcredits']+=entry['amount_microcredits']
        receipts['recorded_cash_microdollars']+=row['cash_microdollars'] or 0
    return {'start':start,'end':end,'project_id':project,'hours':hours,'usage':totals,'funding':receipts,
            'known_infra_cost_microdollars':int(cost) if project is None else None,'cost_complete':complete if project is None else False,
            'usage_margin_microdollars':totals['debited_microcredits']-int(cost) if complete and project is None else None,
            'historical_apportionment':historical,'cost_versions':[json.loads(v['payload'])|{'effective_at':v['effective']} for v in versions],
            'notes':['Usage charges may consume promotional credits; usage margin is not cash profit.',
                     'Costs start when configured. Missing provider costs remain unknown, never zero.',
                     'Monthly fixed costs use 730 hours. Hourly usage is apportioned within each metering interval.',
                     'Project reports do not allocate shared node infrastructure costs.']}
