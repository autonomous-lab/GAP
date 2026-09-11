#!/usr/bin/env python3
"""Operator-only microVM pricing and credit top-ups, without restarting GAP.

Run on the deployment host. Never pass the operator token in argv or print it.
Prices and balances use microcredits (one credit = 1,000,000 microcredits).
"""
import argparse
import json
from pathlib import Path
import sys
import urllib.request
import uuid


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--token-file',default='data/gap-compose/config/billing-admin.token')
    p.add_argument('--endpoint',default='http://172.17.0.1:8092/operator')
    sub=p.add_subparsers(dest='action',required=True)
    sub.add_parser('pricing')
    price=sub.add_parser('set-pricing')
    price.add_argument('--mode',choices=('shadow','enforced'),required=True)
    price.add_argument('--tariff-file',help='JSON: version and five microcredit unit prices (legacy GiB or commercial GB/730-hour month)')
    for action in ('account','topup'):
        command=sub.add_parser(action)
        command.add_argument('--project',required=True)
        command.add_argument('--owner',required=True)
        if action=='topup':
            command.add_argument('--amount-microcredits',type=int,required=True)
            command.add_argument('--request-id',required=True,help='Reuse after a lost response; never change the amount')
    args=p.parse_args()
    body={'action':args.action}
    if args.action=='set-pricing':
        body['mode']=args.mode
        if args.tariff_file: body['tariff']=json.loads(Path(args.tariff_file).read_text())
    if args.action in ('account','topup'): body.update(project_id=args.project,owner_did=args.owner)
    if args.action=='topup': body.update(amount_microcredits=args.amount_microcredits,request_id=args.request_id)
    token=Path(args.token_file).read_text().strip()
    req=urllib.request.Request(args.endpoint,data=json.dumps(body).encode(),
        headers={'Authorization':'Bearer '+token,'Content-Type':'application/json'})
    try:
        with urllib.request.urlopen(req,timeout=60) as response: print(response.read().decode())
    except urllib.error.HTTPError as error:
        print(error.read().decode(),file=sys.stderr);raise SystemExit(1)


if __name__=='__main__': main()
