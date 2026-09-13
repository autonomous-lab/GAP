"""Durable cold-migration preparation, before routing/billing handoff.

Trusted workers attest durable local facts; receipts are audit evidence, not
cryptographic host proofs. No state here grants destination execution. In
particular, a retry response is historical evidence, never a start lease.
"""
import re
import secrets

from authority import Failure, identifier


def schema(db):
    db.execute('''CREATE TABLE IF NOT EXISTS vm_migrations(
        id TEXT PRIMARY KEY, vm TEXT NOT NULL REFERENCES capacity(vm),
        project TEXT NOT NULL REFERENCES projects(id), customer TEXT NOT NULL,
        owner TEXT NOT NULL, source TEXT NOT NULL, target TEXT NOT NULL,
        capacity_revision INTEGER NOT NULL, revision INTEGER NOT NULL,
        phase TEXT NOT NULL, disk_digest TEXT, source_receipt TEXT,
        target_receipt TEXT, created INTEGER NOT NULL,
        CHECK(source<>target))''')
    db.execute('''CREATE TABLE IF NOT EXISTS vm_host_projects(
        project TEXT NOT NULL REFERENCES projects(id),node TEXT NOT NULL,
        PRIMARY KEY(project,node))''')
    db.execute('DROP INDEX IF EXISTS migration_vm')
    db.execute('DROP INDEX IF EXISTS active_migration_vm')
    db.execute("CREATE UNIQUE INDEX IF NOT EXISTS active_migration_vm ON vm_migrations(vm) WHERE phase NOT IN ('cancelled','committed')")


def view(row):
    return dict(migration_id=row['id'], vm_id=row['vm'], project_id=row['project'],
                source_node=row['source'], target_node=row['target'],
                revision=row['revision'], phase=row['phase'],
                disk_sha256=row['disk_digest'], execution_authorized=False,
                handoff_complete=row['phase']=='committed')


def row_for(db, migration):
    identifier(migration)
    row = db.execute('SELECT * FROM vm_migrations WHERE id=?', (migration,)).fetchone()
    if not row:
        raise Failure('migration_not_found', 404)
    return row


def guard_capacity(db, vm):
    if db.execute("SELECT 1 FROM vm_migrations WHERE vm=? AND phase NOT IN ('cancelled','committed')", (vm,)).fetchone():
        raise Failure('vm_migration_in_progress', 409)


def prepare(a, request, customer, project, vm, source, target, capacity_revision):
    """Operator-only entry point; transport validates the configured node set."""
    identifier(source)
    identifier(target)
    if source == target:
        raise Failure('migration_same_node')
    a.capacity_number(capacity_revision, 2**53-1, 1)
    body = dict(action='migration-prepare', customer=customer, project=project,
                vm=vm, source=source, target=target, capacity_revision=capacity_revision)
    def apply(db):
        placement = a.node_project(db, source, project)
        if placement['customer'] != customer:
            raise Failure('migration_customer_mismatch', 403)
        allocation = a.capacity_row(db, source, project, vm)
        if not allocation or allocation['state'] != 'active':
            raise Failure('migration_requires_active_allocation', 409)
        if allocation['revision'] != capacity_revision:
            raise Failure('capacity_revision_conflict', 409)
        guard_capacity(db, vm)
        migration = 'move_' + secrets.token_hex(16)
        db.execute('''INSERT INTO vm_migrations VALUES(
            ?,?,?,?,?,?,?,?,1,'prepared',NULL,NULL,NULL,?)''',
            (migration, vm, project, customer, placement['owner'], source, target,
             capacity_revision, int(a.clock())))
        return view(row_for(db, migration))
    return a.mutation('operator', request, body, apply)


def get(a, migration, node=None):
    with a.db() as db:
        row = row_for(db, migration)
        if node is not None and node not in (row['source'], row['target']):
            raise Failure('migration_node_mismatch', 403)
        return view(row)


def attest(a, node, request, migration, revision, stage, evidence, disk_digest):
    """Source: disk exported AFTER stop+fence. Target: imported, still stopped.

    Disk integrity and shutdown must be checked by the worker before reporting.
    No timeout transition releases the source fence or destination execution.
    """
    a.capacity_number(revision, 2**53-1, 1)
    identifier(evidence)
    if not isinstance(disk_digest, str) or not re.fullmatch('[0-9a-f]{64}', disk_digest):
        raise Failure('invalid_migration_disk_digest')
    if stage not in ('source-fenced', 'target-staged'):
        raise Failure('invalid_migration_stage')
    body = dict(action='migration-attest', migration=migration, revision=revision,
                stage=stage, evidence=evidence, disk_digest=disk_digest)
    def apply(db):
        row = row_for(db, migration)
        expected_node = row['source'] if stage == 'source-fenced' else row['target']
        if node != expected_node:
            raise Failure('migration_node_mismatch', 403)
        if row['revision'] != revision:
            raise Failure('migration_revision_conflict', 409)
        expected_phase = 'prepared' if stage == 'source-fenced' else 'source_fenced'
        if row['phase'] != expected_phase:
            raise Failure('migration_phase_conflict', 409)
        allocation = a.capacity_row(db, row['source'], row['project'], row['vm'])
        if not allocation or allocation['state'] != 'active' or allocation['revision'] != row['capacity_revision']:
            raise Failure('migration_allocation_changed', 409)
        if stage == 'source-fenced':
            db.execute("UPDATE vm_migrations SET phase='source_fenced',revision=revision+1,disk_digest=?,source_receipt=? WHERE id=?",
                       (disk_digest, evidence, migration))
        else:
            if disk_digest != row['disk_digest']:
                raise Failure('migration_disk_digest_mismatch', 409)
            db.execute("UPDATE vm_migrations SET phase='target_staged',revision=revision+1,target_receipt=? WHERE id=?",
                       (evidence, migration))
        return view(row_for(db, migration))
    return a.mutation('node:'+node, request, body, apply)


def cancel_request(a, request, migration, revision):
    """Fence progression, then wait for BOTH hosts to acknowledge no execution.

    Cancellation is not a timeout-based unlock. Target must durably discard its
    copy before the source may be unfenced; delayed source/target work remains
    blocked by its local migration marker.
    """
    a.capacity_number(revision, 2**53-1, 1)
    body = dict(action='migration-cancel', migration=migration, revision=revision)
    def apply(db):
        row = row_for(db, migration)
        if row['revision'] != revision:
            raise Failure('migration_revision_conflict', 409)
        if row['phase'] not in ('prepared', 'source_fenced', 'target_staged', 'source_settled', 'routing_ready'):
            raise Failure('migration_phase_conflict', 409)
        db.execute("UPDATE vm_migrations SET phase='cancelling',revision=revision+1 WHERE id=?", (migration,))
        return view(row_for(db, migration))
    return a.mutation('operator', request, body, apply)


def cancel_attest(a, node, request, migration, revision, stage, evidence):
    a.capacity_number(revision, 2**53-1, 1)
    identifier(evidence)
    if stage not in ('target-discarded', 'source-restored'):
        raise Failure('invalid_migration_stage')
    body = dict(action='migration-cancel-attest', migration=migration,
                revision=revision, stage=stage, evidence=evidence)
    def apply(db):
        row = row_for(db, migration)
        target = stage == 'target-discarded'
        if node != row['target' if target else 'source']:
            raise Failure('migration_node_mismatch', 403)
        if row['revision'] != revision:
            raise Failure('migration_revision_conflict', 409)
        if row['phase'] != ('cancelling' if target else 'target_discarded'):
            raise Failure('migration_phase_conflict', 409)
        # Store new receipts separately in the operation log, preserving export
        # and import receipts in the migration record for postmortem inspection.
        db.execute('UPDATE vm_migrations SET phase=?,revision=revision+1 WHERE id=?',
                   ('target_discarded' if target else 'cancelled', migration))
        return view(row_for(db, migration))
    return a.mutation('node:'+node, request, body, apply)


def settle(a, node, request, migration, revision, checkpoint_request):
    """Source confirms its final VM sample is in a durable central checkpoint.

    The worker must stop metering the exported VM BEFORE submitting this fact.
    Its reservation may remain open for other VMs in the same project.
    """
    a.capacity_number(revision, 2**53-1, 1)
    identifier(checkpoint_request)
    body=dict(action='migration-settle',migration=migration,revision=revision,checkpoint=checkpoint_request)
    def apply(db):
        import json
        row=row_for(db,migration)
        if node!=row['source']:raise Failure('migration_node_mismatch',403)
        if row['revision']!=revision:raise Failure('migration_revision_conflict',409)
        if row['phase']!='target_staged':raise Failure('migration_phase_conflict',409)
        operation=db.execute('SELECT result FROM operations WHERE actor=? AND id=?',('node:'+node,checkpoint_request)).fetchone()
        if not operation:raise Failure('migration_checkpoint_missing',409)
        checkpoint=json.loads(operation['result'])
        if (checkpoint.get('node_id'),checkpoint.get('project_id'),checkpoint.get('owner_did'))!=(node,row['project'],row['owner']):
            raise Failure('migration_checkpoint_mismatch',409)
        reservation=db.execute('SELECT * FROM reservations WHERE id=?',(checkpoint.get('reservation_id'),)).fetchone()
        if not reservation or (reservation['node'],reservation['project'],reservation['customer'])!=(node,row['project'],row['customer']):
            raise Failure('migration_checkpoint_mismatch',409)
        if checkpoint.get('unpaid_microcredits')!=0 or reservation['unpaid']:
            raise Failure('unpaid_usage_requires_reconciliation',409)
        db.execute("UPDATE vm_migrations SET phase='source_settled',revision=revision+1 WHERE id=?",(migration,))
        return view(row_for(db,migration))
    return a.mutation('node:'+node,request,body,apply)


def routes_ready(a, request, migration, revision, evidence):
    """Operator attests dormant routes are staged, never that the VM is live."""
    a.capacity_number(revision, 2**53-1, 1)
    identifier(evidence)
    body=dict(action='migration-routes-ready',migration=migration,revision=revision,evidence=evidence)
    def apply(db):
        row=row_for(db,migration)
        if row['revision']!=revision:raise Failure('migration_revision_conflict',409)
        if row['phase']!='source_settled':raise Failure('migration_phase_conflict',409)
        db.execute("UPDATE vm_migrations SET phase='routing_ready',revision=revision+1 WHERE id=?",(migration,))
        return view(row_for(db,migration))
    return a.mutation('operator',request,body,apply)


def commit(a, request, migration, revision):
    """Atomic placement handoff, preserving the project home and wallet history.

    This is NOT a guest start lease: target still requires fresh billing and
    policy admission. A committed move is never rolled back on a timeout.
    """
    a.capacity_number(revision, 2**53-1, 1)
    body=dict(action='migration-commit',migration=migration,revision=revision)
    def apply(db):
        row=row_for(db,migration)
        if row['revision']!=revision:raise Failure('migration_revision_conflict',409)
        if row['phase']!='routing_ready':raise Failure('migration_phase_conflict',409)
        allocation=a.capacity_row(db,row['source'],row['project'],row['vm'])
        if not allocation or allocation['state']!='active' or allocation['revision']!=row['capacity_revision']:
            raise Failure('migration_allocation_changed',409)
        if db.execute("SELECT 1 FROM retention_claims WHERE project=? AND node IN (?,?) AND state='claimed'",(row['project'],row['source'],row['target'])).fetchone():
            raise Failure('migration_retention_claimed',409)
        db.execute('INSERT OR IGNORE INTO vm_host_projects VALUES(?,?)',(row['project'],row['target']))
        db.execute('UPDATE capacity SET node=?,revision=revision+1 WHERE vm=?',(row['target'],row['vm']))
        db.execute("UPDATE vm_migrations SET phase='committed',revision=revision+1 WHERE id=?",(migration,))
        return view(row_for(db,migration))
    return a.mutation('operator',request,body,apply)
