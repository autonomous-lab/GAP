#!/usr/bin/env python3
"""Private operator-authority CLI. Credentials stay in files, never arguments.

Legacy export is a read-only inventory, not a balance transfer or source fence.
"""
import argparse
import json
import os
from pathlib import Path
import sqlite3
import sys
import urllib.error
import urllib.parse
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_):
        return None


def write_private(path, value):
    # Refuse clobbering a prior response or following a symlink.
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'w') as stream:
        json.dump(value, stream, indent=2, allow_nan=False)
        stream.write('\n')


def export_legacy(path, node):
    uri = Path(path).resolve().as_uri() + '?mode=ro'
    with sqlite3.connect(uri, uri=True) as db:
        db.row_factory = sqlite3.Row
        db.execute('PRAGMA query_only=ON')
        db.execute('BEGIN')
        columns = ['balance', 'spent', 'remainder', 'shadow_remainder', 'budget',
                   'budget_spent', 'budget_epoch', 'exhausted_at', 'retention_claim']
        rows = db.execute('SELECT project,owner,' + ','.join(columns) + ' FROM accounts ORDER BY project')
        snapshots = [dict(project_id=row['project'], snapshot=dict(owner_did=row['owner'], **{k: row[k] for k in columns})) for row in rows]
    return dict(version=1, node_id=node, state='inventory_only', source_fenced=False,
                spendable=False, accounts=snapshots)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    export = sub.add_parser('export-legacy')
    export.add_argument('--ledger', required=True)
    export.add_argument('--node', required=True)
    export.add_argument('--output', required=True)
    call = sub.add_parser('call')
    call.add_argument('--endpoint', default='http://172.17.0.1:8096')
    call.add_argument('--token-file', default='data/gap-control/config/operator.token')
    call.add_argument('--path', choices=('/operator', '/node', '/v1/account', '/v1/wallet', '/v1/projects', '/v1/logout'), default='/operator')
    call.add_argument('--request-file', help='JSON body; omitted for a GET')
    call.add_argument('--output', help='Required for issue-token. New private file, never overwritten.')
    args = parser.parse_args()
    try:
        if args.command == 'export-legacy':
            value = export_legacy(args.ledger, args.node)
            write_private(args.output, value)
            print(json.dumps(dict(path=str(Path(args.output).resolve()), accounts=len(value['accounts']), source_fenced=False)))
            return
        endpoint = urllib.parse.urlsplit(args.endpoint)
        # A private bridge or local authenticated tunnel is the only cleartext
        # exception. Remote fleet traffic requires a verified HTTPS certificate.
        if endpoint.username or endpoint.password or endpoint.query or endpoint.fragment or endpoint.path not in ('', '/'):
            raise ValueError()
        if endpoint.scheme != 'https' and not (endpoint.scheme == 'http' and endpoint.hostname in ('127.0.0.1', 'localhost', '172.17.0.1')):
            raise ValueError()
        body = json.loads(Path(args.request_file).read_text()) if args.request_file else None
        if body is not None and not isinstance(body, dict):
            raise ValueError()
        if body and body.get('action') == 'issue-token' and not args.output:
            raise ValueError('issue-token requires --output')
        if args.output and Path(args.output).exists():
            raise ValueError('output already exists')
        token = Path(args.token_file).read_text().strip()
        request = urllib.request.Request(args.endpoint.rstrip('/') + args.path,
                    data=json.dumps(body, allow_nan=False).encode() if body is not None else None,
                    headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json', 'User-Agent': 'GAP-Control/1.0'})
        try:
            response = urllib.request.build_opener(NoRedirect()).open(request, timeout=15)
        except urllib.error.HTTPError as error:
            with error:
                result = json.loads(error.read(65537))
            # Only an identifier, never an upstream HTML page/request echo.
            import re
            code = result.get('error', {}).get('code', '')
            if not isinstance(code, str) or not re.fullmatch(r'[a-z_]{1,80}', code):
                code = 'control_request_failed'
            print(code, file=sys.stderr)
            raise SystemExit(1)
        with response:
            raw = response.read(1024 * 1024 + 1)
        if len(raw) > 1024 * 1024:
            raise ValueError()
        value = json.loads(raw)
        if args.output:
            write_private(args.output, value)
            print(json.dumps({'path': str(Path(args.output).resolve())}))
        else:
            if 'token' in value:
                raise ValueError()
            print(json.dumps(value, indent=2))
    except (OSError, ValueError, TypeError, sqlite3.Error):
        print('Control operation failed; inspect paths, private credentials and request locally. No secrets printed.', file=sys.stderr)
        raise SystemExit(1) from None


if __name__ == '__main__':
    main()
