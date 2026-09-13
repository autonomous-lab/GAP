"""Bounded HTTPS requests to explicitly configured migration peers."""
import json
from pathlib import Path
import re
import urllib.request
from urllib.parse import urlsplit

from microvm import VMError


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self,*args,**kwargs):return None


def request(config,node,body):
    peer=config.get('migration_peers',{}).get(node)
    if not isinstance(peer,dict):raise VMError('migration_peer_not_configured')
    origin=peer.get('origin','');url=urlsplit(origin)
    if (url.scheme!='https' or not url.hostname or url.username or url.password
            or url.path not in ('','/') or url.query or url.fragment):
        raise VMError('invalid_migration_peer_origin')
    try:
        token=Path(peer['token_file']).read_text().strip()
        if not re.fullmatch('[A-Za-z0-9_-]{43,128}',token):raise ValueError()
        req=urllib.request.Request(origin.rstrip('/')+'/v1/fleet/migration-worker',
            data=json.dumps(body).encode(),headers={'Authorization':'Bearer '+token,
                'Content-Type':'application/json','User-Agent':'GAP-Migration/1.0'})
        with urllib.request.build_opener(NoRedirect).open(req,timeout=5) as response:
            raw=response.read(2*1024*1024+1)
            if response.status!=200 or len(raw)>2*1024*1024:raise ValueError()
            value=json.loads(raw)
            if not isinstance(value,dict):raise ValueError()
            return value
    except Exception:
        raise VMError('migration_peer_unavailable') from None
