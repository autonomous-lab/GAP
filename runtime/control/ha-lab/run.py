#!/usr/bin/env python3
"""Disposable real PostgreSQL/Patroni/etcd partition test. Synthetic credits only."""
import concurrent.futures
import json
from pathlib import Path
import subprocess
import time

PREFIX='gap-ha-lab-'; NETWORK=PREFIX+'net'
ROOT=Path('/tmp/gap-ha-lab-config'); ROOT.mkdir(exist_ok=True); ROOT.chmod(0o755)
NODES=[PREFIX+'pg'+str(i) for i in range(1,4)]
CREATED=[]; CONFIGS=[]; report={'scope':'real GAP Authority on PostgreSQL, synthetic credits and isolated replicas','checks':[]}

def cmd(args,check=True,timeout=30,input=None):
    r=subprocess.run(args,input=input,text=True,capture_output=True,timeout=timeout)
    if check and r.returncode:raise RuntimeError(' '.join(args[:4])+': '+r.stderr[-1200:])
    return r

def docker(*args,**kw):return cmd(['docker',*args],**kw)
def sql(node,query,check=True):
    return docker('exec','-e','PGOPTIONS=-c statement_timeout=5000',node,'psql','-h','127.0.0.1','-U','postgres','-At','-v','ON_ERROR_STOP=1','-c',query,check=check,timeout=12)
def authority(node,*args,check=True):
    return docker('exec',node,'python3','/control/ha-lab/authority_client.py',*args,check=check,timeout=25)
def role(node):
    r=docker('exec',node,'python3','-c',"import urllib.request,json;print(json.load(urllib.request.urlopen('http://127.0.0.1:8008/patroni',timeout=1))['role'])",check=False,timeout=5)
    return r.stdout.strip() if r.returncode==0 else 'unavailable'
def wait(fn,label,seconds=100):
    end=time.monotonic()+seconds
    while time.monotonic()<end:
        try:
            value=fn()
            if value:return value
        except (RuntimeError,subprocess.TimeoutExpired):pass
        time.sleep(1)
    raise RuntimeError('timeout: '+label)
def record(name,**values):
    report['checks'].append(dict(check=name,**values));print(json.dumps(report['checks'][-1]),flush=True)
def primary(exclude=()):
    leaders=[n for n in NODES if n not in exclude and role(n)=='primary']
    return leaders[0] if len(leaders)==1 else None

try:
    # Never reuse or clean up a namespace belonging to a previous run implicitly.
    if docker('network','inspect',NETWORK,check=False).returncode==0:raise RuntimeError('lab network already exists')
    docker('network','create','--internal',NETWORK)
    cluster=','.join(f'e{i}=http://e{i}:2380' for i in range(1,4))
    for i in range(1,4):
        name=PREFIX+'e'+str(i)
        docker('run','-d','--name',name,'--label','gap.ha-lab=true','--network',NETWORK,'--network-alias','e'+str(i),
            '--memory','256m','--cpus','.5','quay.io/coreos/etcd:v3.6.4','/usr/local/bin/etcd',
            '--name','e'+str(i),'--data-dir','/etcd-data','--listen-client-urls','http://0.0.0.0:2379',
            '--advertise-client-urls',f'http://e{i}:2379','--listen-peer-urls','http://0.0.0.0:2380',
            '--initial-advertise-peer-urls',f'http://e{i}:2380','--initial-cluster',cluster,'--initial-cluster-token','gap-isolated-lab')
        CREATED.append(name)
    for i,name in enumerate(NODES,1):
        config=dict(scope='gap-isolated',name=name,namespace='/gap-lab/',
          restapi=dict(listen='0.0.0.0:8008',connect_address=name+':8008'),
          etcd3=dict(hosts=['e1:2379','e2:2379','e3:2379']),
          bootstrap=dict(dcs=dict(ttl=10,loop_wait=2,retry_timeout=2,maximum_lag_on_failover=0,
              synchronous_mode=True,synchronous_mode_strict=True,synchronous_node_count=1,
              check_timeline=True,postgresql=dict(use_pg_rewind=True,parameters=dict(wal_log_hints='on',synchronous_commit='on'))),
              initdb=[{'encoding':'UTF8'},'data-checksums'],pg_hba=['host all all 0.0.0.0/0 trust','host replication all 0.0.0.0/0 trust']),
          postgresql=dict(listen='0.0.0.0:5432',connect_address=name+':5432',data_dir='/var/lib/postgresql/data/patroni',bin_dir='/usr/lib/postgresql/17/bin',
              authentication=dict(superuser=dict(username='postgres',password='synthetic-lab-only'),replication=dict(username='replicator',password='synthetic-lab-only'))),
          watchdog=dict(mode='off'))
        p=ROOT/(name+'.json');p.write_text(json.dumps(config));p.chmod(0o644); CONFIGS.append(p)
        docker('run','-d','--name',name,'--label','gap.ha-lab=true','--network',NETWORK,'--network-alias',name,
               '--memory','512m','--cpus','1','-v',str(p)+':/lab.json:ro','-v',str(Path(__file__).resolve().parent.parent)+':/control:ro','gap-ha-lab-patroni','/lab.json')
        CREATED.append(name)
    leader=wait(primary,'initial leader')
    wait(lambda:sql(leader,"select count(*) from pg_stat_replication where state='streaming'").stdout.strip()=='2','two replicas')
    wait(lambda:'sync' in sql(leader,'select sync_state from pg_stat_replication').stdout.split(),'synchronous standby')
    wait(lambda: 'sync_standby' in docker('exec',PREFIX+'e1','/usr/local/bin/etcdctl','get','/gap-lab/gap-isolated/sync','--print-value-only').stdout and json.loads(docker('exec',PREFIX+'e1','/usr/local/bin/etcdctl','get','/gap-lab/gap-isolated/sync','--print-value-only').stdout).get('sync_standby'), 'published synchronous standby')
    record('cluster_ready',leader=leader,data_nodes=3,consensus_members=3)
    authority(leader,'setup')
    authority(leader,'debit','confirmed-before-partition','25')
    began=time.monotonic();docker('network','disconnect',NETWORK,leader)
    # Docker exec still reaches the old SQL process locally despite its network isolation.
    denied=authority(leader,'debit','lost-response','10',check=False)
    assert denied.returncode!=0,'isolated primary confirmed a write'
    record('isolated_primary_did_not_acknowledge',exit_code=denied.returncode)
    successor=wait(lambda:primary((leader,)),'successor election')
    wait(lambda:'sync' in sql(successor,'select sync_state from pg_stat_replication').stdout.split(),'successor synchronous partner')
    assert sql(successor,"select count(*) from wallet_entries where operation='confirmed-before-partition'").stdout.strip()=='1'
    authority(successor,'debit','lost-response','10')
    authority(successor,'debit','lost-response','10')
    assert json.loads(authority(successor,'wallet').stdout)['balance_microcredits']==65
    record('failover_preserved_acknowledged_debit_and_idempotent_retry',successor=successor,elapsed_seconds=round(time.monotonic()-began,2))
    with concurrent.futures.ThreadPoolExecutor(2) as pool:
        results=list(pool.map(lambda r:authority(successor,'debit',r,'65',check=False).returncode,['race-a','race-b']))
    assert sorted(results)==[0,2],results
    assert json.loads(authority(successor,'wallet').stdout)['balance_microcredits']==0
    assert sql(successor,"select sum(-delta) from wallet_entries where kind='usage'").stdout.strip()=='100'
    record('concurrent_final_balance_spend',outcomes=results,final_balance=0,total_debits=100)
    # Restore network and let Patroni rewind the previous primary to the new timeline.
    docker('network','connect','--alias',leader,NETWORK,leader)
    wait(lambda:role(leader)=='replica','old primary rejoins as replica')
    wait(lambda:sql(leader,"select sum(-delta) from wallet_entries where kind='usage'").stdout.strip()=='100','replica converges')
    assert authority(leader,'debit','forbidden-replica','1',check=False).returncode!=0
    assert len([n for n in NODES if role(n)=='primary'])==1
    record('old_primary_rejoined_read_only',one_primary=True)
    report['passed']=True
except Exception as e:
    report['passed']=False;report['error']=str(e);print(str(e),flush=True)
    for name in CREATED:
        logs=docker('logs','--tail','12',name,check=False).stderr
        print(name,logs[-1500:],flush=True)
finally:
    for name in reversed(CREATED):docker('rm','-f','-v',name,check=False)
    if CREATED:docker('network','rm',NETWORK,check=False)
    for p in CONFIGS:p.unlink(missing_ok=True)
    Path('/tmp/gap-ha-lab-result.json').write_text(json.dumps(report,indent=2))
print(json.dumps(report),flush=True)
raise SystemExit(0 if report.get('passed') else 1)
