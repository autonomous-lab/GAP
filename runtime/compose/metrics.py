"""Read-only host measurements. Never executes in, resumes, or touches a guest."""
import json
import os
from pathlib import Path
import socket
import threading
import time


class Metrics:
    def __init__(self, runner):
        self.runner = runner
        self.lock = threading.Lock()
        self.samples = {}
        self.dns = {'hostname':None, 'addresses':[], 'resolved_at':None}
        self.dns_pending = False
        self.dns_attempt = 0

    def addresses(self, hostname):
        # DNS never blocks an owner request. Only the operator-configured public
        # network hostname is resolved; callers cannot submit arbitrary targets.
        with self.lock:
            if not self.dns_pending and (self.dns['hostname'] != hostname or time.monotonic()-self.dns_attempt > 60):
                self.dns_pending = True
                self.dns_attempt = time.monotonic()
                threading.Thread(target=self.resolve, args=(hostname,), daemon=True).start()
            return dict(self.dns)

    def resolve(self, hostname):
        try:
            addresses=sorted({r[4][0] for r in socket.getaddrinfo(hostname,None,type=socket.SOCK_STREAM)})
            with self.lock:self.dns={'hostname':hostname,'addresses':addresses,'resolved_at':time.time()}
        except OSError:
            pass
        finally:
            with self.lock:self.dns_pending=False

    def process(self, meta):
        manager=self.runner.hypervisor
        if not manager.alive(meta):return {'identity':None,'cpu_seconds':0,'resident_bytes':0}
        pid=int((manager.folder(meta)/'qemu.pid').read_text())
        stat=Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()
        # Recheck the controller-owned identity after reading /proc. Never
        # report a recycled PID belonging to another VM or host process.
        if not manager.alive(meta) or int((manager.folder(meta)/'qemu.pid').read_text())!=pid:raise OSError('VM process changed')
        return {'identity':(pid,int(stat[19])), 'cpu_seconds':(int(stat[11])+int(stat[12]))/os.sysconf('SC_CLK_TCK'),
                'resident_bytes':max(0,int(stat[21]))*os.sysconf('SC_PAGE_SIZE')}

    def read(self, meta):
        if not meta or meta['state']=='destroyed':return {'available':False}
        manager=self.runner.hypervisor;runtime=self.runner.runtime
        result={'available':True,'vm_id':meta['vm_id'],'sampled_at':time.time(),
                'allocated_vcpus':meta['vcpus'],'allocated_memory_bytes':meta['memory_mib']*1024*1024,
                'disk_capacity_bytes':meta['disk_gib']*1024**3,'source':'host','errors':[]}
        now=time.monotonic()
        try:process=self.process(meta)
        except (OSError,ValueError,IndexError):process=None;result['errors'].append('process_metrics_unavailable')
        result['memory_resident_bytes']=process['resident_bytes'] if process else None
        result['cpu_seconds_total']=process['cpu_seconds'] if process else None
        result['cpu_percent_of_allocation']=0 if process and process['identity'] is None else None
        result['cpu_window_seconds']=None
        incoming=outgoing=None
        try:
            meter=manager.meters.get(meta['vm_id'])
            if meter:incoming,outgoing=meter.values()
            else:
                path=manager.folder(meta)/'network-counts.json'
                counts=json.loads(path.read_text()) if path.exists() else {'in':0,'out':0}
                incoming,outgoing=counts['in'],counts['out']
            if any(type(n) is not int or n<0 for n in (incoming,outgoing)):raise ValueError('invalid counters')
        except (OSError,ValueError,KeyError,RuntimeError):
            incoming=outgoing=None;result['errors'].append('network_metrics_unavailable')
        result.update(network_in_bytes=incoming,network_out_bytes=outgoing,network_in_bytes_per_second=None,network_out_bytes_per_second=None)
        try:result['storage_host_bytes']=runtime.storage_bytes(meta) if runtime else None
        except OSError:result['storage_host_bytes']=None;result['errors'].append('storage_metrics_unavailable')
        with self.lock:
            before=self.samples.get(meta['vm_id'])
            elapsed=now-before['time'] if before else 0
            if before and elapsed>=1:
                old=before['process']
                if process and old and process['identity'] is not None and process['identity']==old['identity']:
                    delta=process['cpu_seconds']-old['cpu_seconds']
                    if delta>=0:
                        result['cpu_percent_of_allocation']=100*delta/elapsed/meta['vcpus']
                        result['cpu_window_seconds']=elapsed
                for name,total in [('in',incoming),('out',outgoing)]:
                    previous=before[name]
                    if total is not None and previous is not None and total>=previous:
                        result['network_'+name+'_bytes_per_second']=(total-previous)/elapsed
            # Rapid refreshes do not destroy a useful measurement baseline.
            if not before or elapsed>=1:
                if len(self.samples)>=1024:self.samples.pop(next(iter(self.samples)))
                self.samples[meta['vm_id']]={'time':now,'process':process,'in':incoming,'out':outgoing}
        result['public_network']=self.addresses(manager.network.host) if manager.network else None
        return result
