"""Aggregate host headroom only: no tenant identifiers or credentials."""
import json
import os
from pathlib import Path
import shutil
import time


def snapshot(manager, runtime):
    result = dict(available=False, checked_at=int(time.time()), max_age_seconds=30)
    if not manager or not runtime:
        return result
    try:
        from admission import check
        ready = check(runtime.ledger)['ready']
        pricing = runtime.ledger.pricing()
        memory = dict((line.split(':')[0], int(line.split()[1]) // 1024)
                      for line in Path('/proc/meminfo').read_text().splitlines()
                      if line.startswith(('MemTotal:', 'MemAvailable:')))
        rows = [json.loads(p.read_text()) for p in (manager.root / 'catalog').glob('*.json')]
        active = [r for r in rows if r['state'] not in ('destroyed', 'migrated')]
        retained = [r for r in rows if r['state'] != 'migrated' and (r['state'] != 'destroyed' or r.get('retained'))]
        cpu = len(os.sched_getaffinity(0))
        # Reserve host overhead; retain stopped/hibernated VM commitments.
        cpu_free = max(0, cpu - 1 - sum(float(r['vcpus']) for r in active))
        ram_free = max(0, min(memory['MemAvailable'], memory['MemTotal'] - sum(r['memory_mib'] for r in active)) - 1024)
        disk = shutil.disk_usage(manager.root)
        disk_free = max(0, (disk.free - sum(r['disk_gib'] * 1024**3 for r in retained)) // 1024**3 - 5)
        model = next((line.split(':', 1)[1].strip() for line in Path('/proc/cpuinfo').read_text().splitlines() if line.startswith('model name')), 'Unknown')
        return dict(result, available=True, admission_ready=ready,
                    hardware=dict(cpu_model=model, logical_cpus=cpu, memory_mib=memory['MemTotal']),
                    headroom=dict(vcpus=int(cpu_free*4)/4, memory_mib=(ram_free//256)*256, disk_gib=disk_free),
                    pricing=dict(pricing, available=pricing['mode']=='enforced' and pricing['tariff'] is not None, currency='USD'),
                    basis='Host headroom after retained VM commitments and overhead; admission and account quotas still apply.')
    except Exception:
        return result
