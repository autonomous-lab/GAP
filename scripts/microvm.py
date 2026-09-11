#!/usr/bin/env python3
"""Agent CLI: manage Linux microVMs, networking and SSH with the normal GAP token."""
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
    sub.add_parser('show', help='Inspect the microVM')
    create = sub.add_parser('create', help='Create a Linux VM; no Compose release is required')
    create.add_argument('--vcpus', type=int, default=1)
    create.add_argument('--memory-mib', type=int, default=1024)
    create.add_argument('--disk-gib', type=int, default=8)
    create.add_argument('--guest-port', type=int, action='append', default=[])
    create.add_argument('--key', action='append', default=[], help='Initial owner Ed25519 public key file')
    create.add_argument('--stopped', action='store_true')
    sub.add_parser('start', help='Start the entire VM')
    stop = sub.add_parser('stop', help='Stop the entire VM')
    stop.add_argument('--force', action='store_true')
    resize = sub.add_parser('resize', help='Resize a stopped VM; disks only grow')
    for name in ('vcpus', 'memory-mib', 'disk-gib'):
        resize.add_argument('--' + name, type=int)
    destroy = sub.add_parser('destroy', help='Destroy a stopped VM; retains disk by default')
    destroy.add_argument('--delete-data', action='store_true')
    destroy.add_argument('--confirm-data-loss', action='store_true')
    sub.add_parser('ingress', help='Inspect HTTP/API/WebSocket publication')
    ingress = sub.add_parser('set-ingress', help='Configure HTTP/API/WebSocket publication')
    ingress.add_argument('--guest-port', type=int)
    ingress.add_argument('--disable', action='store_true')
    sub.add_parser('ports' , help='Read the five allocated port numbers')
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
    if args.action.startswith('set-') or args.action in ('create','start','stop','resize','destroy'):
        if args.action != 'create' and (not args.vm or not re.fullmatch(r'vm_[0-9a-f]{32}', args.vm)):
            parser.error('--vm is required for writes')
        method = 'PUT'
        resource = {'set-ports':'ports', 'set-ssh-keys':'ssh', 'set-ingress':'ingress'}.get(args.action, args.action)
        body = {'vm_id': args.vm, 'request_id': args.request_id or uuid.uuid4().hex}
        if resource == 'ports':
            try:
                body['mappings'] = [dict(slot=int(s), guest_port=int(p), protocol=t)
                                    for s, p, t in (item.split(':') for item in args.map)]
            except ValueError:
                parser.error('use --map SLOT:GUEST_PORT:tcp|udp|both')
        elif resource == 'ssh':
            body['authorized_keys'] = [Path(p).read_text().strip() for p in args.key]
        elif resource == 'ingress':
            if args.disable and args.guest_port is not None or not args.disable and args.guest_port is None:
                parser.error('choose --guest-port PORT or --disable')
            body['enabled'] = not args.disable
            if not args.disable:
                body['guest_port'] = args.guest_port
        elif resource == 'create':
            method, resource = 'POST', ''
            body.pop('vm_id')
            body.update(vcpus=args.vcpus, memory_mib=args.memory_mib, disk_gib=args.disk_gib,
                        ports=args.guest_port, start=not args.stopped,
                        ssh_keys=[Path(p).read_text().strip() for p in args.key])
        elif resource in ('start','stop'):
            method = 'POST'
            if resource == 'stop':
                body['force'] = args.force
        elif resource == 'resize':
            method, resource = 'PATCH', ''
            for field in ('vcpus','memory_mib','disk_gib'):
                if getattr(args, field) is not None:
                    body[field] = getattr(args, field)
        elif resource == 'destroy':
            method, resource = 'DELETE', ''
            if args.delete_data and not args.confirm_data_loss:
                parser.error('--delete-data also requires --confirm-data-loss')
            body.update(delete_data=args.delete_data, confirm_data_loss=args.confirm_data_loss)
        # Keep this ID if the HTTP response is lost; never print the bearer.
        print('request_id=' + body['request_id'], file=sys.stderr)
    elif args.action == 'job':
        if not re.fullmatch(r'job_[0-9a-f]{32}', args.job_id):
            parser.error('invalid job identity')
        resource = 'jobs/' + args.job_id
    if args.action == 'show':
        resource = ''
    url = args.node.rstrip('/') + '/v1/cloud/projects/' + args.project + '/vm' + ('/' + resource if resource else '')
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
