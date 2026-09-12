"""Operator-only consolidation of node reports and the authoritative wallet.

Imports and node reservations are transfers, never receipts or new revenue.
Unavailable costs and incomplete node reports never produce a margin.
"""
import concurrent.futures
import json
import math
from pathlib import Path
import re
import urllib.parse
import urllib.request
from authority import Failure

FIELDS=('vcpu_ms','ram_byte_ms','disk_byte_ms','bytes_in','bytes_out','estimated_microcredits','debited_microcredits','unpaid_microcredits')
FUNDING=('paid_credits_microcredits','promotional_credits_microcredits','unclassified_credits_microcredits','recorded_cash_microdollars')


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self,*a,**kw):return None


class Finance:
    def __init__(self,authority,sources,nodes,transport=None):
        self.a=authority;self.sources=[];self.transport=transport or self.fetch
        for source in sources:
            source=dict(source);url=urllib.parse.urlsplit(source['url'])
            if source['node_id'] not in nodes or any(s['node_id']==source['node_id'] for s in self.sources):raise ValueError('invalid finance node')
            if not isinstance(source['provider'],str) or not 1<=len(source['provider'])<=100:raise ValueError('invalid finance provider')
            local=url.scheme=='http' and url.hostname in ('127.0.0.1','localhost','172.17.0.1')
            if not url.hostname or url.username or url.password or url.query or url.fragment or url.path!='/v1/fleet/node-finance' or not (local or url.scheme=='https'):raise ValueError('invalid finance source')
            source['token']=Path(source['token_file']).read_text().strip()
            if not re.fullmatch(r'[A-Za-z0-9_-]{43,128}',source['token']):raise ValueError('invalid finance credential')
            self.sources.append(source)

    @staticmethod
    def fetch(source,start,end,project):
        query={'start':start,'end':end}
        if project:query['project_id']=project
        request=urllib.request.Request(source['url']+'?'+urllib.parse.urlencode(query),headers={'Authorization':'Bearer '+source['token'],'User-Agent':'GAP-Finance/1.0'})
        with urllib.request.build_opener(NoRedirect()).open(request,timeout=8) as response:raw=response.read(2*1024*1024+1)
        if len(raw)>2*1024*1024:raise ValueError('oversized report')
        return json.loads(raw)

    def report(self,start,end,project=None,customer=None,node=None):
        if type(start) is not int or type(end) is not int or start<0 or end<=start or start%3600 or end%3600 or end-start>31*86400 or end>int(self.a.clock())//3600*3600:raise Failure('invalid_finance_window')
        if project is not None and not re.fullmatch(r'prj_[0-9a-f]{24}',project):raise Failure('invalid_finance_project')
        if customer is not None and not re.fullmatch(r'cus_[0-9a-f]{32}',customer):raise Failure('invalid_finance_customer')
        sources=[s for s in self.sources if node is None or s['node_id']==node]
        if node is not None and not sources:raise Failure('unknown_finance_node')
        with self.a.db() as db:
            mappings={r['id']:r['customer'] for r in db.execute('SELECT id,customer FROM projects')}
            customers=[dict(r) for r in db.execute('SELECT id,label FROM customers ORDER BY id LIMIT 1001')]
            if customer and not any(c['id']==customer for c in customers):self.a.customer(db,customer)
            if customer and project and mappings.get(project)!=customer:raise Failure('project_customer_mismatch')
            central=list(db.execute("SELECT customer,source,delta FROM wallet_entries WHERE kind='funding' AND created>=? AND created<?",(start,end)))
        def read(source):
            try:
                result=self.transport(source,start,end,project)
                if result.get('available') is not True or result.get('start')!=start or result.get('end')!=end:raise ValueError()
                def quantities(values):
                    return all(type(values[k]) in (int,float) and math.isfinite(values[k]) for k in FIELDS[:5]) and all(type(values[k]) is int for k in FIELDS[5:])
                if not quantities(result['usage']) or any(type(result['funding'][k]) is not int for k in FUNDING):raise ValueError()
                if result.get('project_id')!=project or type(result['cost_complete']) is not bool:raise ValueError()
                if result['known_infra_cost_microdollars'] is not None and type(result['known_infra_cost_microdollars']) is not int:raise ValueError()
                if len(result['hours'])!=(end-start)//3600 or {h['hour'] for h in result['hours']}!=set(range(start,end,3600)):raise ValueError()
                for h in result['hours']:
                    if not quantities(h) or type(h['cost_complete']) is not bool:raise ValueError()
                if not isinstance(result.get('projects'),list):raise ValueError()
                for p in result['projects']:
                    if not re.fullmatch(r'prj_[0-9a-f]{24}',p['project_id']) or not quantities(p['usage']) or type(p['lifetime_spent_microcredits']) is not int or any(type(p['funding'][k]) is not int for k in FUNDING):raise ValueError()
                return source,result
            except Exception:return source,None
        with concurrent.futures.ThreadPoolExecutor(max_workers=min(4,max(1,len(sources)))) as pool:
            reports=list(pool.map(read,sources))
        usage={k:0 for k in FIELDS};funding={k:0 for k in FUNDING};hours={};by_customer={};bound_spent={};details=[];providers={}
        unassigned_usage=0;unassigned_projects=set()
        complete=bool(sources);attribution=True;known_cost=0;cost_complete=bool(sources) and project is None and customer is None;historical=False;versions=[]
        for source,result in reports:
            entry={'node_id':source['node_id'],'provider':source['provider'],'available':result is not None}
            provider=providers.setdefault(source['provider'],dict(provider=source['provider'],known_infra_cost_microdollars=0,cost_complete=True,nodes=0))
            provider['nodes']+=1
            if result is None:
                complete=False;cost_complete=False;attribution=False;entry['reason']='node_report_unavailable';provider['cost_complete']=False;details.append(entry);continue
            attribution=attribution and result.get('projects_complete') is True
            project_rows=result['projects']
            if customer:
                matched=[p for p in project_rows if mappings.get(p['project_id'])==customer]
                for p in matched:
                    for k in FIELDS:usage[k]+=p['usage'][k]
                    for k in FUNDING:funding[k]+=p['funding'][k]
            else:
                for k in FIELDS:usage[k]+=result['usage'][k]
                for k in FUNDING:funding[k]+=result['funding'][k]
                for h in result['hours']:
                    if type(h.get('hour')) is not int or h['hour']<start or h['hour']>=end or h['hour']%3600:raise Failure('invalid_node_hour')
                    target=hours.setdefault(h['hour'],dict(hour=h['hour'],**{k:0 for k in FIELDS},known_infra_cost_microdollars=0,cost_complete=True))
                    for k in FIELDS:target[k]+=h[k]
                    target['known_infra_cost_microdollars']+=h['known_infra_cost_microdollars'] or 0
                    target['cost_complete']=target['cost_complete'] and h['cost_complete'] is True
            for p in project_rows:
                cid=mappings.get(p['project_id'])
                if cid:
                    by_customer[cid]=by_customer.get(cid,0)+p['usage']['debited_microcredits']
                    if p.get('fleet_bound'):bound_spent[cid]=bound_spent.get(cid,0)+p['lifetime_spent_microcredits']
                else:
                    unassigned_usage+=p['usage']['debited_microcredits'];unassigned_projects.add(p['project_id'])
            cost=result['known_infra_cost_microdollars'];valid_cost=result['cost_complete'] is True
            if cost is not None:known_cost+=cost;provider['known_infra_cost_microdollars']+=cost
            provider['cost_complete']=provider['cost_complete'] and valid_cost
            cost_complete=cost_complete and valid_cost
            historical=historical or result['historical_apportionment']
            versions.extend(dict(v,node_id=source['node_id'],provider=source['provider']) for v in result.get('cost_versions',[]))
            entry.update(usage_microcredits=result['usage']['debited_microcredits'],known_infra_cost_microdollars=cost,cost_complete=valid_cost,projects_complete=result.get('projects_complete') is True)
            details.append(entry)
        selected_customer=customer or mappings.get(project)
        central_paid=0
        # A node filter cannot allocate shared-account funding to a single host.
        if not node and not project:
            for row in central:
                if selected_customer and row['customer']!=selected_customer:continue
                key=row['source']+'_credits_microcredits'
                if key in funding:funding[key]+=row['delta']
                if row['source']=='paid':central_paid+=row['delta']
        wallets=[];unknown_project=project is not None and project not in mappings
        wallet_customers=customers[:1000]
        if selected_customer and not any(c['id']==selected_customer for c in wallet_customers):
            with self.a.db() as db:wallet_customers=[dict(db.execute('SELECT id,label FROM customers WHERE id=?',(selected_customer,)).fetchone())]
        for c in wallet_customers:
            if unknown_project:continue
            if selected_customer and c['id']!=selected_customer:continue
            w=self.a.wallet(c['id']);consistent=complete and attribution and node is None and project is None
            wallets.append(dict(customer_id=c['id'],label=c['label'],usage_microcredits=by_customer.get(c['id'],0),**{k:w[k] for k in ('balance_microcredits','reserved_microcredits','total_remaining_microcredits','spent_microcredits')},
                unsettled_usage_microcredits=bound_spent.get(c['id'],0)-w['spent_microcredits'] if consistent else None))
        if customer and not attribution:complete=False
        for h in hours.values():
            h['cost_complete']=h['cost_complete'] and complete
            h['usage_margin_microdollars']=h['debited_microcredits']-h['known_infra_cost_microdollars'] if h['cost_complete'] else None
        return dict(available=any(r is not None for _,r in reports),scope='fleet',start=start,end=end,project_id=project,customer_id=customer,node_id=node,
            usage=usage,funding=funding,hours=sorted(hours.values(),key=lambda h:h['hour']),coverage_complete=complete,customer_attribution_complete=attribution,
            known_infra_cost_microdollars=known_cost if not project and not customer else None,cost_complete=cost_complete and complete,
            usage_margin_microdollars=usage['debited_microcredits']-known_cost if cost_complete and complete else None,
            historical_apportionment=historical,cost_versions=versions,sources=details,providers=list(providers.values()),customers=wallets,
            customer_options=customers[:1000],wallets_complete=(len(customers)<=1000 or selected_customer is not None) and not unknown_project,node_options=[{'node_id':s['node_id'],'provider':s['provider']} for s in self.sources],
            wallet_totals={k:None if unknown_project else sum(w[k] for w in wallets) for k in ('balance_microcredits','reserved_microcredits','total_remaining_microcredits','spent_microcredits')},
            unassigned={'usage_microcredits':unassigned_usage,'projects':len(unassigned_projects)},
            cash_receipts_complete=complete and central_paid==0 and funding['unclassified_credits_microcredits']==0,central_paid_without_cash_receipt_microcredits=central_paid,
            notes=[str(len(unassigned_projects))+' project(s) have no customer binding; their reported usage is '+str(unassigned_usage)+' microcredits.',
                   'Node reservations and wallet imports are transfers, not funding or receipts.',
                   'Wallet values are current account totals, including node reservations exactly once; usage covers the selected complete UTC hours.',
                   'Unsettled usage compares cumulative node charges with central checkpoints; reports are not simultaneous accounting snapshots.',
                   'Usage margin may include promotional credits and is not cash profit. Unrecorded cash receipts remain unknown.',
                   'Provider costs start when configured. Missing or unavailable costs never become zero; shared costs are not allocated to individual customers or projects.',
                   'Customer-filtered reports show period totals without an hourly allocation. A node filter does not allocate common-wallet funding to that host.'])
