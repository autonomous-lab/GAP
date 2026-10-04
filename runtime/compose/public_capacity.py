"""Aggregate host headroom only: no tenant identifiers or credentials."""
import json
import os
from pathlib import Path
import shutil
import time
import host_capacity


def snapshot(manager, runtime):
    result = dict(available=False, checked_at=int(time.time()), max_age_seconds=30)
    if not manager or not runtime:
        return result
    try:
        from admission import check
        ready = check(runtime.ledger)['ready']
        pricing = runtime.ledger.pricing()
        memory = host_capacity.memory_mib()
        rows = [json.loads(p.read_text()) for p in (manager.root / 'catalog').glob('*.json')]
        active = [r for r in rows if r['state'] not in ('destroyed', 'migrated') and manager.alive(r)]
        # A completed anonymous cleanup has already destroyed the disk and
        # released the central reservation. Old workers could leave its
        # catalog state as `stopped`; never count that ghost allocation.
        retained = [r for r in rows if not r.get('free_vm_cleanup_complete')
                    and r['state'] != 'migrated' and (r['state'] != 'destroyed' or r.get('retained'))]
        cpu = host_capacity.logical_cpus()
        # Execution headroom counts only verified live guests. Retained disks
        # stay committed; a stopped/hibernated guest must pass admission on wake.
        reserve_cpu = getattr(runtime,'reserve_vcpus',0)
        ratio = getattr(runtime,'cpu_overcommit_ratio',8)
        vms_per_cpu = getattr(runtime,'vms_per_cpu',8)
        reserve_memory = getattr(runtime,'reserve_memory_mib',2048)
        reserve_swap = getattr(runtime,'reserve_swap_mib',0)
        min_available_memory = getattr(runtime,'min_available_memory_mib',512)
        cpu_free = max(0, host_capacity.cpu_limit(cpu,reserve_cpu,ratio) - sum(float(r['vcpus']) for r in active))
        vm_free = max(0, host_capacity.vm_limit(cpu,vms_per_cpu) - len(active))
        ram_free = min(host_capacity.startable_memory(memory,reserve_memory,reserve_swap,min_available_memory),
                       host_capacity.commitment_memory(memory,sum(r['memory_mib'] for r in active),reserve_memory,reserve_swap))
        disk = shutil.disk_usage(manager.root)
        disk_free = max(0, (disk.free - sum(r['disk_gib'] * 1024**3 for r in retained)) // 1024**3 - 5)
        model = next((line.split(':', 1)[1].strip() for line in Path('/proc/cpuinfo').read_text().splitlines() if line.startswith('model name')), 'Unknown')
        return dict(result, available=True, admission_ready=ready,
                    hardware=dict(cpu_model=model, logical_cpus=cpu, memory_mib=memory['MemTotal'], swap_mib=memory['SwapTotal']),
                    headroom=dict(vcpus=int(cpu_free*4)/4, vm_slots=vm_free, memory_mib=(ram_free//256)*256, disk_gib=disk_free),
                    pricing=dict(pricing, available=pricing['mode']=='enforced' and pricing['tariff'] is not None, currency='USD'),
                    basis='CPU, VM-slot and RAM+swap headroom after live guests and host overhead; retained disks remain committed, and wake admission and account quotas still apply.')
    except Exception:
        return result
