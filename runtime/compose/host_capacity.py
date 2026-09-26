"""Shared host admission arithmetic for the worker and public headroom."""
import os
from pathlib import Path


def memory_mib():
    keys = {'MemTotal', 'MemAvailable', 'SwapTotal', 'SwapFree'}
    return {key: int(value.split()[0]) // 1024
            for line in Path('/proc/meminfo').read_text().splitlines()
            for key, separator, value in (line.partition(':'),)
            if separator and key in keys}


def logical_cpus():
    return len(os.sched_getaffinity(0))


def cpu_limit(cpus, reserve_vcpus, ratio):
    return max(1, (cpus - reserve_vcpus) * ratio)


def startable_memory(memory, reserve_memory_mib, reserve_swap_mib, min_available_memory_mib=512):
    """Keep a physical safety floor while allowing resident pages to swap."""
    if memory['MemAvailable'] < min_available_memory_mib:
        return 0
    usable_swap = max(0, memory['SwapFree'] - reserve_swap_mib)
    return max(0, memory['MemAvailable'] - reserve_memory_mib + usable_swap)


def commitment_memory(memory, committed_mib, reserve_memory_mib, reserve_swap_mib):
    usable_swap = max(0, memory['SwapTotal'] - reserve_swap_mib)
    return max(0, memory['MemTotal'] - reserve_memory_mib + usable_swap - committed_mib)
