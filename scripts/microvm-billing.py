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
from pricing_env import tariff_from_env


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--token-file',default='data/gap-compose/config/billing-admin.token')
    p.add_argument('--endpoint',default='http://172.17.0.1:8092/operator')
    sub=p.add_subparsers(dest='action',required=True)
    sub.add_parser('pricing')
    preview=sub.add_parser('preview-pricing-env',help='Validate and print only the tariff; no network request')
    preview.add_argument('--env-file',default='.env')
    apply=sub.add_parser('set-pricing-env',help='Apply explicit environment prices without restarting')
    apply.add_argument('--env-file',default='.env')
    apply.add_argument('--expect-version',required=True,help='Current live version, or none for a fresh ledger')
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
    if args.action in ('preview-pricing-env','set-pricing-env'):
        try: tariff=tariff_from_env(args.env_file)
        except (ValueError,OSError):
            # Do not echo a malformed file, exception containing input or secrets.
            print('Invalid pricing configuration. All six pricing settings must be explicit, unique and valid.',file=sys.stderr)
            raise SystemExit(2)
        if args.action=='preview-pricing-env':
            print(json.dumps(tariff,indent=2));return
    body={'action':args.action}
    if args.action=='set-pricing-env':
        body={'action':'set-pricing','mode':'enforced','tariff':tariff,
              'expected_version':None if args.expect_version=='none' else args.expect_version}
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
