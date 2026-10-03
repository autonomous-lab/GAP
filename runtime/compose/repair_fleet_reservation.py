"""Offline, explicitly fenced repair after a worker ledger rollback.

The authority's cumulative consumption has already debited the customer. This
tool only makes the worker mirror that paid consumption; it never contacts or
changes the authority. A human must independently verify the authority row.
"""
import argparse
import fcntl
import hashlib
import json
from pathlib import Path
import re
import time

import finance
from sqlite_crypto import connect, sqlite3


def reconcile_record(db,project,reservation,expected_local_allocated,expected_local_spent,
                     central_allocated,central_consumed):
    if not re.fullmatch(r'prj_[0-9a-f]{24}', project) or not re.fullmatch(r'rsv_[0-9a-f]{32}', reservation):
        raise ValueError('invalid_repair_identity')
    if not 0 <= expected_local_spent <= expected_local_allocated <= central_allocated or not 0 <= central_consumed <= central_allocated:
        raise ValueError('invalid_repair_counters')
    row=db.execute('SELECT owner,reservation,allocated,pending FROM fleet_bindings WHERE project=?',(project,)).fetchone()
    account=db.execute('SELECT owner,balance,spent,estimated,budget_spent FROM accounts WHERE project=?',(project,)).fetchone()
    if not row or not account or row[0]!=account[0] or row[1]!=reservation or row[2]!=expected_local_allocated or account[2]!=expected_local_spent:
        raise RuntimeError('local_repair_precondition_changed')
    balance,spent,estimated=account[1:4]
    if balance+spent!=expected_local_allocated or estimated!=spent or not row[3]:
        raise RuntimeError('local_ledger_requires_manual_audit')
    pending=json.loads(row[3])
    if (pending.get('reservation_id')!=reservation or pending.get('project_id')!=project
            or pending.get('owner_did')!=row[0] or pending.get('consumed_microcredits',-1)>spent
            or pending.get('consumed_microcredits',-1)>central_consumed):
        raise RuntimeError('pending_checkpoint_mismatch')
    adjustment=max(0,central_consumed-spent)
    new_spent=spent+adjustment
    extra=central_allocated-expected_local_allocated
    new_balance=balance+extra-adjustment
    if new_balance<0 or new_balance!=central_allocated-new_spent:
        raise RuntimeError('reservation_balance_mismatch')
    operation='fleet-authority-repair:'+reservation+':'+str(central_consumed)
    if db.execute('SELECT 1 FROM entries WHERE project=? AND operation=?',(project,operation)).fetchone():
        raise RuntimeError('repair_already_recorded')
    payload={'kind':'authority_reconciliation','project_id':project,'reservation_id':reservation,
             'previous_allocated_microcredits':expected_local_allocated,
             'previous_spent_microcredits':spent,'previous_balance_microcredits':balance,
             'central_allocated_microcredits':central_allocated,'central_consumed_microcredits':central_consumed,
             'added_allocation_microcredits':extra,'already_paid_consumption_microcredits':adjustment,
             'previous_pending_checkpoint':pending,'reconciled_at':time.time()}
    encoded=json.dumps(payload,sort_keys=True,separators=(',',':'))
    db.execute('INSERT INTO entries(project,operation,digest,payload,created) VALUES(?,?,?,?,?)',
               (project,operation,hashlib.sha256(encoded.encode()).hexdigest(),encoded,payload['reconciled_at']))
    db.execute('UPDATE accounts SET balance=?,spent=?,estimated=?,budget_spent=budget_spent+? WHERE project=?',
               (new_balance,new_spent,new_spent,adjustment,project))
    db.execute('UPDATE fleet_bindings SET allocated=?,pending=NULL WHERE project=?',(central_allocated,project))
    if adjustment:
        finance.record(db,project,{'estimated_microcredits':adjustment,
            'debited_microcredits':adjustment,'unpaid_microcredits':0,'usage':{}},
            payload['reconciled_at'],payload['reconciled_at'],historical=True)
        db.execute('INSERT INTO finance_meta(key,value) VALUES(?,?)',('authority-repair-finance:'+operation,'1'))
    return {'project_id':project,'already_paid_adjustment_microcredits':adjustment,
            'new_allocation_microcredits':extra,'remaining_reservation_microcredits':new_balance}


def backfill_finance(path,project,reservation,central_consumed):
    """One-time projection fix for a repair applied before finance integration."""
    operation='fleet-authority-repair:'+reservation+':'+str(central_consumed)
    db=connect(path,'microvm-credits',timeout=10,isolation_level=None)
    db.row_factory=sqlite3.Row
    try:
        db.execute('BEGIN IMMEDIATE')
        marker='authority-repair-finance:'+operation
        if db.execute('SELECT 1 FROM finance_meta WHERE key=?',(marker,)).fetchone():
            db.execute('COMMIT');return {'already_recorded':True}
        row=db.execute('SELECT payload FROM entries WHERE project=? AND operation=?',(project,operation)).fetchone()
        if not row:raise RuntimeError('repair_entry_not_found')
        payload=json.loads(row[0])
        if payload.get('kind')!='authority_reconciliation' or payload.get('reservation_id')!=reservation or payload.get('central_consumed_microcredits')!=central_consumed:
            raise RuntimeError('repair_entry_mismatch')
        adjustment=payload['already_paid_consumption_microcredits']
        if type(adjustment) is not int or adjustment<=0:raise RuntimeError('no_adjustment_to_backfill')
        finance.record(db,project,{'estimated_microcredits':adjustment,
            'debited_microcredits':adjustment,'unpaid_microcredits':0,'usage':{}},
            payload['reconciled_at'],payload['reconciled_at'],historical=True)
        db.execute('INSERT INTO finance_meta(key,value) VALUES(?,?)',(marker,'1'))
        db.execute('COMMIT')
        return {'backfilled_microcredits':adjustment}
    except BaseException:
        if db.in_transaction:db.execute('ROLLBACK')
        raise
    finally:db.close()


def repair(path, project, reservation, expected_local_allocated, expected_local_spent,
           central_allocated, central_consumed):
    if central_consumed<expected_local_spent:
        raise ValueError('authority_behind_worker_requires_separate_audit')
    path=Path(path).resolve()
    with (path.parent/'runner.lock').open('a') as lock:
        try:fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        except BlockingIOError:raise RuntimeError('runner_must_be_stopped') from None
        db=connect(path,'microvm-credits',timeout=10,isolation_level=None)
        db.row_factory=sqlite3.Row
        try:
            db.execute('BEGIN IMMEDIATE')
            result=reconcile_record(db,project,reservation,expected_local_allocated,expected_local_spent,
                                    central_allocated,central_consumed)
            db.execute('COMMIT')
            if db.execute('PRAGMA integrity_check').fetchone()[0]!='ok':raise RuntimeError('repaired_database_integrity_failed')
            return result
        except BaseException:
            if db.in_transaction:db.execute('ROLLBACK')
            raise
        finally:db.close()


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--backfill-finance',action='store_true')
    parser.add_argument('--database',required=True)
    parser.add_argument('--project',required=True)
    parser.add_argument('--reservation',required=True)
    parser.add_argument('--local-allocated',type=int,required=True)
    parser.add_argument('--local-spent',type=int,required=True)
    parser.add_argument('--central-allocated',type=int,required=True)
    parser.add_argument('--central-consumed',type=int,required=True)
    args=parser.parse_args()
    if args.backfill_finance:
        print(json.dumps(backfill_finance(args.database,args.project,args.reservation,args.central_consumed)))
    else:
        print(json.dumps(repair(args.database,args.project,args.reservation,args.local_allocated,args.local_spent,
                                args.central_allocated,args.central_consumed)))
