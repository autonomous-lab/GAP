import json
from pathlib import Path
import sqlite3
import tempfile
import time
import unittest

import finance
from repair_fleet_reservation import repair


PROJECT='prj_'+'a'*24
RESERVATION='rsv_'+'b'*32
OWNER='did:gap:'+'c'*64


class RepairTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.path=Path(self.temp.name)/'microvm-credits.sqlite'
        db=sqlite3.connect(self.path)
        db.executescript('''
            CREATE TABLE fleet_bindings(project TEXT PRIMARY KEY,owner TEXT,reservation TEXT,allocated INTEGER,pending TEXT);
            CREATE TABLE accounts(project TEXT PRIMARY KEY,owner TEXT,balance INTEGER,spent INTEGER,estimated INTEGER,budget_spent INTEGER);
            CREATE TABLE entries(project TEXT,operation TEXT,digest TEXT,payload TEXT,created REAL,UNIQUE(project,operation));
        ''')
        finance.schema(db)
        pending={'project_id':PROJECT,'owner_did':OWNER,'reservation_id':RESERVATION,'consumed_microcredits':100}
        db.execute('INSERT INTO fleet_bindings VALUES(?,?,?,?,?)',(PROJECT,OWNER,RESERVATION,200,json.dumps(pending)))
        db.execute('INSERT INTO accounts VALUES(?,?,?,?,?,?)',(PROJECT,OWNER,90,110,110,110))
        db.commit();db.close()

    def test_reconciles_without_creating_central_credit(self):
        result=repair(self.path,PROJECT,RESERVATION,200,110,240,130)
        self.assertEqual(result['already_paid_adjustment_microcredits'],20)
        self.assertEqual(result['remaining_reservation_microcredits'],110)
        with sqlite3.connect(self.path) as db:
            self.assertEqual(db.execute('SELECT balance,spent,estimated,budget_spent FROM accounts').fetchone(),(110,130,130,130))
            self.assertEqual(db.execute('SELECT allocated,pending FROM fleet_bindings').fetchone(),(240,None))
            entry=json.loads(db.execute('SELECT payload FROM entries').fetchone()[0])
            self.assertEqual(entry['already_paid_consumption_microcredits'],20)
        with self.assertRaisesRegex(RuntimeError,'local_repair_precondition_changed'):
            repair(self.path,PROJECT,RESERVATION,200,110,240,130)

    def test_fails_closed_on_mismatched_authority_or_local_state(self):
        with self.assertRaisesRegex(ValueError,'invalid_repair_counters'):
            repair(self.path,PROJECT,RESERVATION,200,110,220,230)
        with self.assertRaisesRegex(RuntimeError,'local_repair_precondition_changed'):
            repair(self.path,PROJECT,RESERVATION,201,110,240,130)
        with sqlite3.connect(self.path) as db:
            self.assertEqual(db.execute('SELECT count(*) FROM entries').fetchone()[0],0)

    def test_repair_updates_an_existing_finance_hour(self):
        with sqlite3.connect(self.path) as db:
            db.row_factory=sqlite3.Row
            now=time.time()
            finance.record(db,PROJECT,{'debited_microcredits':110,'estimated_microcredits':110},now,now)
        repair(self.path,PROJECT,RESERVATION,200,110,240,130)
        with sqlite3.connect(self.path) as db:
            amount=db.execute("SELECT sum(json_extract(data,'$.debited_microcredits')) FROM finance_hours WHERE project=?",(PROJECT,)).fetchone()[0]
            self.assertEqual(amount,130)


if __name__=='__main__':unittest.main()
