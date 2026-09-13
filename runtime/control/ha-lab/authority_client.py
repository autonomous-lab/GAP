"""Synthetic requests through the real Authority implementation; lab use only."""
import json
import sys
sys.path.insert(0,'/control')
from authority import Failure
from postgres_authority import PostgresAuthority

try:
    a=PostgresAuthority('host=127.0.0.1 dbname=postgres user=postgres','ha-lab')
    project='prj_'+'a'*24
    customer=a.create_customer('operator','customer','HA lab')['customer_id']
    action=sys.argv[1]
    if action=='setup':
        owner='did:gap:'+'a'*64
        a.attach_principal('operator','owner',customer,'agent',owner)
        a.attach_project('operator','project',customer,project,'node-one',owner)
        result=a.topup('operator','fund',customer,100,'promotional')
    elif action=='debit':result=a.debit('node-one',sys.argv[2],project,int(sys.argv[3]))
    elif action=='wallet':result=a.wallet(customer)
    else:raise ValueError('unknown lab action')
    print(json.dumps(result))
except Failure as error:
    print(json.dumps({'error':error.code}))
    raise SystemExit(2)
