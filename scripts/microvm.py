#!/usr/bin/env python3
"""Agent CLI: manage public microVM ports and SSH keys with the normal GAP token."""
import argparse
import json
import os
from pathlib import Path
import re
import sys
import urllib.error
import urllib.request
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--node', default=os.environ.get('GAP_NODE', 'https://gap.geta.team'))
    parser.add_argument('--project', required=True)
    parser.add_argument('--vm', help='Required VM generation for writes')
    parser.add_argument('--request-id', help='Reuse the same ID and body after a lost response')
    sub = parser.add_subparsers(dest='action', required=True)
    sub.add_parser('ports', help='Read the five allocated port numbers')
    set_ports = sub.add_parser('set-ports', help='Replace all mappings; omit --map to disable all')
    set_ports.add_argument('--map', action='append', default=[], metavar='SLOT:GUEST_PORT:tcp|udp|both')
    sub.add_parser('ssh', help='Read SSH command, approved keys and host fingerprint')
    set_keys = sub.add_parser('set-ssh-keys', help='Replace keys; omit --key to revoke all managed keys')
    set_keys.add_argument('--key', action='append', default=[], help='Ed25519 public key file')
    job = sub.add_parser('job', help='Inspect an asynchronous operation')
    job.add_argument('job_id')
    args = parser.parse_args()
    token = os.environ.get('GAP_TOKEN')
    if not token:
        parser.error('set GAP_TOKEN to your agent bearer')
    if not re.fullmatch(r'prj_[0-9a-f]{24}', args.project):
        parser.error('invalid project identity')
    resource, body, method = args.action, None, 'GET'
    if args.action.startswith('set-'):
        if not args.vm or not re.fullmatch(r'vm_[0-9a-f]{32}', args.vm):
            parser.error('--vm is required for writes')
        method = 'PUT'
        resource = 'ports' if args.action == 'set-ports' else 'ssh'
        body = {'vm_id': args.vm, 'request_id': args.request_id or uuid.uuid4().hex}
        if resource == 'ports':
            try:
                body['mappings'] = [dict(slot=int(s), guest_port=int(p), protocol=t)
                                    for s, p, t in (item.split(':') for item in args.map)]
            except ValueError:
                parser.error('use --map SLOT:GUEST_PORT:tcp|udp|both')
        else:
            body['authorized_keys'] = [Path(p).read_text().strip() for p in args.key]
        # Keep this ID if the HTTP response is lost; never print the bearer.
        print('request_id=' + body['request_id'], file=sys.stderr)
    elif args.action == 'job':
        if not re.fullmatch(r'job_[0-9a-f]{32}', args.job_id):
            parser.error('invalid job identity')
        resource = 'jobs/' + args.job_id
    url = args.node.rstrip('/') + '/v1/cloud/projects/' + args.project + '/stack/' + resource
    req = urllib.request.Request(url, method=method, headers={
        'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'},
        data=json.dumps(body).encode() if body else None)
    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *a, **kw):
            return None
    try:
        with urllib.request.build_opener(NoRedirect).open(req, timeout=20) as response:
            result = json.load(response)
    except urllib.error.HTTPError as error:
        print(error.read(4096).decode(errors='replace'), file=sys.stderr)
        return 1
    print(json.dumps(result, indent=2))
    return 0


if __name__ == '__main__':
    sys.exit(main())
