"""Durable, capacity-aware placement reservations for new microVMs."""
import concurrent.futures
import json
import math
import re
import secrets
import time
import urllib.parse
import urllib.request

from authority import Failure, identifier


PLACEMENT = re.compile(r'plc_[0-9a-f]{32}\Z')


def schema(db):
    db.execute('''CREATE TABLE IF NOT EXISTS placement_reservations(
        id TEXT PRIMARY KEY, project TEXT NOT NULL REFERENCES projects(id),
        customer TEXT NOT NULL REFERENCES customers(id), owner TEXT NOT NULL,
        node TEXT NOT NULL, cpu INTEGER NOT NULL, memory INTEGER NOT NULL,
        disk INTEGER NOT NULL, region TEXT NOT NULL, tariff TEXT NOT NULL,
        state TEXT NOT NULL, generation INTEGER NOT NULL, vm TEXT,
        created INTEGER NOT NULL, expires INTEGER NOT NULL, committed INTEGER NOT NULL DEFAULT 0)''')
    db.execute('CREATE INDEX IF NOT EXISTS placement_node_state ON placement_reservations(node,state,expires)')
    db.execute('CREATE INDEX IF NOT EXISTS placement_project ON placement_reservations(project,generation)')


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_):
        return None


class Directory:
    def __init__(self, sources, clock=time.time, fetch=None):
        self.clock = clock
        self.fetch = fetch or self.read
        self.sources = []
        seen = set()
        for source in sources:
            if not isinstance(source, dict) or set(source) != {'node_id', 'url'}:
                raise ValueError('invalid placement source')
            node = identifier(source['node_id'])
            url = urllib.parse.urlsplit(source['url'])
            if (node in seen or url.scheme != 'https' or not url.hostname or url.username or url.password
                    or url.query or url.fragment or url.path not in ('', '/')):
                raise ValueError('invalid placement source')
            seen.add(node)
            self.sources.append({'node_id': node, 'url': source['url'].rstrip('/')})
        if not self.sources:
            raise ValueError('placement sources required')

    @staticmethod
    def read(source):
        request = urllib.request.Request(source['url'] + '/v1/public-node',
                                         headers={'User-Agent': 'GAP-Placement/1.0'})
        with urllib.request.build_opener(NoRedirect()).open(request, timeout=3) as response:
            raw = response.read(16385)
            if response.status != 200 or len(raw) > 16384:
                raise ValueError('invalid node response')
        value = json.loads(raw)
        if not isinstance(value, dict):
            raise ValueError('invalid node response')
        return value

    def snapshots(self):
        def one(source):
            try:
                return source['node_id'], self.fetch(source)
            except Exception:
                return source['node_id'], None
        with concurrent.futures.ThreadPoolExecutor(max_workers=min(4, len(self.sources))) as pool:
            return dict(pool.map(one, self.sources))


def resource(value, maximum, minimum=1):
    if type(value) is not int or not minimum <= value <= maximum:
        raise Failure('invalid_placement_resources')
    return value


def view(row):
    return dict(placement_id=row['id'], project_id=row['project'], customer_id=row['customer'],
                owner_did=row['owner'], node_id=row['node'], state=row['state'],
                generation=row['generation'], vm_id=row['vm'], region=row['region'],
                tariff_version=row['tariff'], resources=dict(cpu_quarters=row['cpu'],
                    memory_mib=row['memory'], disk_gib=row['disk']),
                expires_at=row['expires'] if row['state'] == 'reserved' else 0)


def valid_snapshot(node, value, now):
    if not isinstance(value, dict) or value.get('node_id') != node:
        return None
    checked = value.get('checked_at')
    maximum_age = value.get('max_age_seconds')
    microvm = value.get('microvm')
    if (type(checked) is not int or type(maximum_age) is not int or maximum_age < 1
            or checked > now + 5 or now - checked > min(maximum_age, 60)
            or not isinstance(microvm, dict) or microvm.get('available') is not True
            or microvm.get('admission_ready') is not True):
        return None
    headroom, pricing = microvm.get('headroom'), microvm.get('pricing')
    tariff = pricing.get('tariff') if isinstance(pricing, dict) else None
    if (not isinstance(headroom, dict) or not isinstance(tariff, dict)
            or pricing.get('available') is not True or pricing.get('mode') != 'enforced'
            or pricing.get('currency') != 'USD' or not isinstance(tariff.get('version'), str)):
        return None
    vcpus = headroom.get('vcpus')
    memory, disk = headroom.get('memory_mib'), headroom.get('disk_gib')
    if (isinstance(vcpus, bool) or not isinstance(vcpus, (int, float)) or not math.isfinite(vcpus)
            or type(memory) is not int or type(disk) is not int or min(vcpus, memory, disk) < 0):
        return None
    return dict(node_id=node, checked_at=checked, region=value.get('region') if isinstance(value.get('region'), str) else '',
                tariff=tariff['version'], cpu=math.floor(vcpus * 4), memory=memory, disk=disk)


def actor_key(actor):
    import hashlib
    return 'placement:' + hashlib.sha256(json.dumps(actor, sort_keys=True).encode()).hexdigest()


def reserve(authority, actor, request, project, cpu, memory, disk, region, snapshots, ttl=120):
    identifier(request)
    if not isinstance(project, str) or not re.fullmatch(r'prj_[0-9a-f]{24}', project):
        raise Failure('invalid_project')
    resource(cpu, 16)
    resource(memory, 8192, 256)
    resource(disk, 100)
    if memory % 256:
        raise Failure('invalid_placement_resources')
    if not isinstance(region, str) or len(region) > 100 or any(ord(c) < 32 for c in region):
        raise Failure('invalid_placement_region')
    if type(ttl) is not int or not 30 <= ttl <= 300:
        raise Failure('invalid_placement_ttl')
    body = dict(action='placement-reserve', project=project, cpu=cpu, memory=memory,
                disk=disk, region=region, ttl=ttl)

    def apply(db):
        now = int(authority.clock())
        bound = db.execute('SELECT * FROM projects WHERE id=?', (project,)).fetchone()
        if not bound or bound['customer'] != actor['customer']:
            raise Failure('project_membership_required', 403)
        if actor['agent'] is not None:
            grant = db.execute('SELECT role FROM grants WHERE project=? AND agent=?',
                               (project, actor['agent'])).fetchone()
            if not grant or grant['role'] not in ('owner', 'operator'):
                raise Failure('project_operator_required', 403)
        quota = authority.quota_state(db, actor['customer'])
        reserved = db.execute('''SELECT count(*) max_vms,coalesce(sum(cpu),0) cpu_quarters,
                coalesce(sum(memory),0) memory_mib FROM placement_reservations
            WHERE customer=? AND state='reserved' AND expires>?''', (actor['customer'], now)).fetchone()
        requested = dict(max_vms=1, cpu_quarters=cpu, memory_mib=memory)
        for key, value in requested.items():
            if quota['allocated'][key] + reserved[key] + value > quota['limits'][key]:
                raise Failure('customer_quota_exceeded_' + key, 409)
        candidates = []
        for node, raw in snapshots.items():
            snap = valid_snapshot(node, raw, now)
            if not snap or (region and snap['region'] != region):
                continue
            held = db.execute('''SELECT coalesce(sum(cpu),0) cpu,coalesce(sum(memory),0) memory,
                    coalesce(sum(disk),0) disk FROM placement_reservations
                WHERE node=? AND ((state='reserved' AND expires>?) OR state='claimed'
                    OR (state='committed' AND committed>=?))''', (node, now, snap['checked_at'])).fetchone()
            free = dict(cpu=snap['cpu']-held['cpu'], memory=snap['memory']-held['memory'], disk=snap['disk']-held['disk'])
            if free['cpu'] >= cpu and free['memory'] >= memory and free['disk'] >= disk:
                score = (1 if region and snap['region'] == region else 0,
                         free['cpu']-cpu, free['memory']-memory, free['disk']-disk, node)
                candidates.append((score, snap))
        if not candidates:
            raise Failure('no_eligible_placement', 409)
        selected = max(candidates, key=lambda item: item[0])[1]
        generation = db.execute('SELECT coalesce(max(generation),0)+1 FROM placement_reservations WHERE project=?',
                                (project,)).fetchone()[0]
        placement = 'plc_' + secrets.token_hex(16)
        db.execute('''INSERT INTO placement_reservations VALUES(
            ?,?,?,?,?,?,?,?,?,?,'reserved',?,NULL,?,?,0)''',
            (placement, project, actor['customer'], bound['owner'], selected['node_id'], cpu,
             memory, disk, selected['region'], selected['tariff'], generation, now, now+ttl))
        db.execute('INSERT OR IGNORE INTO vm_host_projects VALUES(?,?)', (project, selected['node_id']))
        row = db.execute('SELECT * FROM placement_reservations WHERE id=?', (placement,)).fetchone()
        return dict(operator_id=authority.operator, **view(row))
    return authority.mutation(actor_key(actor), request, body, apply)


def row_for(db, placement):
    if not isinstance(placement, str) or not PLACEMENT.fullmatch(placement):
        raise Failure('invalid_placement_id')
    row = db.execute('SELECT * FROM placement_reservations WHERE id=?', (placement,)).fetchone()
    if not row:
        raise Failure('placement_not_found', 404)
    return row


def get(authority, node, placement):
    with authority.db() as db:
        row = row_for(db, placement)
        if row['node'] != node:
            raise Failure('placement_node_mismatch', 403)
        value = view(row)
        if row['state'] == 'reserved' and row['expires'] <= int(authority.clock()):
            value['state'], value['expires_at'] = 'expired', 0
        return dict(operator_id=authority.operator, **value)


def claim(db, authority, node, placement, project, owner, vm, cpu, memory, disk):
    row = row_for(db, placement)
    expected = (node, project, owner, cpu, memory, disk)
    actual = (row['node'], row['project'], row['owner'], row['cpu'], row['memory'], row['disk'])
    if actual != expected:
        raise Failure('placement_binding_mismatch', 403)
    if row['state'] == 'claimed' and row['vm'] == vm:
        return
    if row['state'] != 'reserved':
        raise Failure('placement_already_consumed', 409)
    if row['expires'] <= int(authority.clock()):
        raise Failure('placement_expired', 409)
    db.execute("UPDATE placement_reservations SET state='claimed',vm=?,expires=0 WHERE id=?", (vm, placement))


def finish_vm(db, authority, vm, outcome):
    row = db.execute("SELECT * FROM placement_reservations WHERE vm=? AND state IN ('claimed','committed')", (vm,)).fetchone()
    if not row:
        return
    if outcome == 'commit':
        db.execute("UPDATE placement_reservations SET state='committed',committed=? WHERE id=?",
                   (int(authority.clock()), row['id']))
    elif outcome in ('abort', 'release'):
        db.execute("UPDATE placement_reservations SET state='released',expires=0 WHERE id=?", (row['id'],))


def host_approval(authority, node, project, owner):
    with authority.db() as db:
        bound = authority.node_project(db, node, project)
        if bound['owner'] != owner:
            raise Failure('project_owner_mismatch', 403)
        now = int(authority.clock())
        rows = db.execute('''SELECT * FROM placement_reservations WHERE node=? AND project=? AND owner=?
            AND ((state='reserved' AND expires>?) OR state IN ('claimed','committed'))''',
            (node, project, owner, now)).fetchall()
        if not rows:
            raise Failure('placement_not_active', 409)
        quota = dict(max_vms=len(rows), vcpus=sum(r['cpu'] for r in rows)/4,
                     memory_mib=sum(r['memory'] for r in rows), disk_gib=sum(r['disk'] for r in rows))
        tier=db.execute('SELECT tier FROM customer_tiers WHERE customer=?',(bound['customer'],)).fetchone()
        tier=tier['tier'] if tier else 'free'
        return dict(operator_id=authority.operator, id=project, node=node, owner=owner,
                    managed=True, tier=tier, network_restricted=tier=='free',
                    always_on_allowed=False, quota=quota)
