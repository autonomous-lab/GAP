#!/usr/bin/env python3
"""Remove a retired node from live configuration, preserving historical state.

Dry-run by default. --apply saves private backups before atomic replacements.
Restart the control service, GAP API and SSH worker after applying changes.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import sys
import tempfile
from urllib.parse import urlsplit


def plan(root, node, origin):
    changes=[]
    for relative in ('data/gap-control/config/control.json',
                     'data/gap-compose/config/runner.json'):
        path=root/relative
        if not path.exists():continue
        before=path.read_bytes()
        config=json.loads(before)
        if relative.endswith('control.json'):
            for key in ('node_token_files','identity_token_files','console_paths','migration_peers'):
                config.get(key,{}).pop(node,None)
            for key in ('finance_sources','placement_sources'):
                if key in config:
                    config[key]=[entry for entry in config[key] if entry.get('node_id')!=node]
            if not config.get('node_token_files'):
                raise ValueError('cannot retire the last control node')
        else:
            if config.get('free_vm',{}).get('node_id')==node:
                raise ValueError('cannot retire the local worker with this tool')
            config.get('free_vm',{}).get('peers',{}).pop(node,None)
            config.get('migration_peers',{}).pop(node,None)
        after=(json.dumps(config,indent=2,sort_keys=True)+'\n').encode()
        if json.loads(before)!=config:changes.append((path,before,after))
    path=root/'.env'
    before=path.read_bytes()
    lines=before.decode().splitlines(keepends=True)
    hostname=urlsplit(origin).hostname
    edge_host_key='GAP_FLEET_NODE'+node.removeprefix('node-')+'_HOST'
    for i,line in enumerate(lines):
        name,separator,value=line.partition('=')
        if separator and name==edge_host_key:
            lines[i]=''
            continue
        if separator and name in ('GAP_PUBLIC_EXPLORER_NODES','GAP_PUBLIC_ELESTIO_NODES'):
            value=value.strip()
            quote=value[:1] if value[:1] in ('"',"'") and value[-1:]==value[:1] else ''
            if quote:value=value[1:-1]
            peers=[item for item in value.split(',') if urlsplit(item.strip()).hostname!=hostname]
            lines[i]=name+'='+quote+','.join(peers)+quote+('\n' if line.endswith('\n') else '')
    after=''.join(lines).encode()
    if after!=before:changes.append((path,before,after))
    return changes


def apply(root,node,changes):
    # Private backups stay beside runtime data, outside version control.
    if not changes:return None
    backup=Path(tempfile.mkdtemp(prefix='retire-'+node+'-',dir=root/'data'))
    for path,before,_ in changes:
        if path.read_bytes()!=before:raise ValueError('configuration changed since planning')
        target=backup/path.relative_to(root)
        target.parent.mkdir(parents=True,exist_ok=True)
        shutil.copy2(path,target)
    for path,before,after in changes:
        if path.read_bytes()!=before:raise ValueError('configuration changed since backup')
        stat=path.stat()
        descriptor,pending=tempfile.mkstemp(prefix='.retire-',dir=path.parent)
        try:
            os.fchmod(descriptor,stat.st_mode & 0o777)
            os.fchown(descriptor,stat.st_uid,stat.st_gid)
            with os.fdopen(descriptor,'wb') as stream:
                stream.write(after);stream.flush();os.fsync(stream.fileno())
            os.replace(pending,path)
        finally:
            if os.path.exists(pending):os.unlink(pending)
    return backup


def release_orphans(authority,node):
    """Release offline VM slots after retirement, retaining unsettled credits.

    A lost node's last checkpoint cannot establish final consumption. Monetary
    reservations require explicit reconciliation and are never refunded here.
    """
    def action(db):
        for table,column in (('projects','node'),('free_vm_trials','node')):
            if db.execute('SELECT 1 FROM '+table+' WHERE '+column+'=?',(node,)).fetchone():
                raise ValueError('retired node still has projects; migrate or resolve them first')
        if db.execute("SELECT 1 FROM vm_migrations WHERE (source=? OR target=?) AND phase NOT IN ('cancelled','committed')",(node,node)).fetchone():
            raise ValueError('retired node has an incomplete migration')
        if db.execute("SELECT 1 FROM capacity WHERE node=? AND state!='released' AND (cpu!=0 OR memory!=0 OR target_cpu!=0 OR target_memory!=0 OR pending IS NOT NULL)",(node,)).fetchone():
            raise ValueError('retired node still has compute reservations requiring reconciliation')
        reservations=db.execute('SELECT * FROM reservations WHERE node=? AND closed=0',(node,)).fetchall()
        if any(row['expires']>authority.clock() for row in reservations):
            raise ValueError('execution lease is active')
        if db.execute("SELECT 1 FROM placement_reservations WHERE node=? AND state='reserved' AND expires>?",(node,int(authority.clock()))).fetchone():
            raise ValueError('retired node has a live placement reservation')
        released=db.execute("UPDATE capacity SET state='released',revision=revision+1 WHERE node=? AND state!='released'",(node,)).rowcount
        placements=db.execute("UPDATE placement_reservations SET state='released',generation=generation+1 WHERE node=? AND state IN ('reserved','committed')",(node,)).rowcount
        hosts=db.execute('DELETE FROM vm_host_projects WHERE node=?',(node,)).rowcount
        return dict(node_id=node,released_vm_slots=released,released_placements=placements,
                    removed_vm_host_bindings=hosts,
                    retained_financial_reservations=len(reservations),
                    retained_microcredits=sum(row['allocated']-row['consumed'] for row in reservations))
    return authority.mutation('operator','retire-'+node+'-orphan-reservations-v1',
                              dict(action='retire-orphan-reservations',node_id=node),action)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('root',type=Path)
    parser.add_argument('--node-id',required=True)
    parser.add_argument('--origin',required=True)
    parser.add_argument('--apply',action='store_true')
    parser.add_argument('--release-orphans',action='store_true',
                        help='After revoking credentials, release orphan VM slots; keep unsettled financial reservations')
    args=parser.parse_args()
    if not re.fullmatch(r'node-[a-zA-Z0-9-]{1,64}',args.node_id):parser.error('invalid node ID')
    origin=urlsplit(args.origin)
    if origin.scheme!='https' or not origin.hostname or origin.username or origin.password or origin.path not in ('','/') or origin.query or origin.fragment:
        parser.error('invalid public origin')
    root=args.root.resolve()
    changes=plan(root,args.node_id,args.origin)
    for path,_,_ in changes:print(('UPDATE ' if args.apply else 'WOULD UPDATE ')+str(path))
    if args.apply:
        backup=apply(root,args.node_id,changes)
        if backup:print('PRIVATE BACKUP '+str(backup))
    if not changes:print('Already retired from these configurations.')
    if args.release_orphans:
        if not args.apply:parser.error('--release-orphans requires --apply')
        config=json.loads((root/'data/gap-control/config/control.json').read_text())
        if args.node_id in config['node_token_files']:raise ValueError('revoke node credentials first')
        database=root/'data/gap-control/state/authority.sqlite'
        import sqlite3
        backup=Path(tempfile.mkdtemp(prefix='retire-'+args.node_id+'-state-',dir=root/'data'))/'authority.sqlite'
        with sqlite3.connect(database) as source,sqlite3.connect(backup) as target:source.backup(target)
        print('PRIVATE STATE BACKUP '+str(backup))
        sys.path.insert(0,str(root/'runtime/control'))
        from authority import Authority
        print(json.dumps(release_orphans(Authority(database,config['operator_id']),args.node_id),sort_keys=True))


if __name__=='__main__':main()
