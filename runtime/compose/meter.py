"""Host-side IP byte counters from QEMU's bounded packet capture filter.

Only a 96-byte header is captured per packet, never the complete application
payload. The FIFO is drained continuously; no packet capture file is retained.
"""
import json
import os
from pathlib import Path
import select
import struct
import threading
import time


def packet_bytes(packet, original=None):
    if len(packet)<34: return 0
    kind=packet[12:14]
    if kind==b'\x08\x00': size=int.from_bytes(packet[16:18],'big')
    elif kind==b'\x86\xdd' and len(packet)>=54: size=40+int.from_bytes(packet[18:20],'big')
    else: return 0
    return min(size,max(0,original-14)) if original is not None else size


class PacketMeter:
    def __init__(self,folder,mac):
        self.folder=Path(folder)
        # Direction comes from the host backend queues, never guest MAC/IP.
        # netdev tx sends towards the guest; netdev rx receives from it.
        self.inputs={}
        for direction in ('in','out'):
            path=self.folder/('meter-'+direction+'.fifo')
            path.unlink(missing_ok=True); os.mkfifo(path,0o600)
            fd=os.open(path,os.O_RDWR|os.O_NONBLOCK)
            self.inputs[fd]={'direction':direction,'path':path,'buffer':bytearray(),'endian':None}
        saved=self.folder/'network-counts.json'
        self.counts=json.loads(saved.read_text()) if saved.exists() else {'in':0,'out':0}
        self.lock=threading.Lock(); self.stopped=False; self.error=None
        self.thread=threading.Thread(target=self.run,daemon=True); self.thread.start()

    def values(self):
        with self.lock:
            if self.error: raise RuntimeError('network_meter_failed')
            return self.counts['in'],self.counts['out']

    def flush(self):
        from microvm import atomic_json
        with self.lock: atomic_json(self.folder/'network-counts.json',dict(self.counts))

    def run(self):
        last=time.monotonic()
        try:
            while not self.stopped:
                for fd in select.select(list(self.inputs),[],[],.1)[0]:
                    stream=self.inputs[fd]; buffer=stream['buffer']
                    buffer.extend(os.read(fd,65536))
                    if stream['endian'] is None and len(buffer)>=24:
                        if buffer[:4]==b'\xd4\xc3\xb2\xa1': stream['endian']='<'
                        elif buffer[:4]==b'\xa1\xb2\xc3\xd4': stream['endian']='>'
                        else: raise ValueError('invalid pcap header')
                        del buffer[:24]
                    while stream['endian'] and len(buffer)>=16:
                        _,_,captured,original=struct.unpack(stream['endian']+'IIII',buffer[:16])
                        if captured>96 or captured>original: raise ValueError('invalid pcap packet')
                        if len(buffer)<16+captured: break
                        size=packet_bytes(buffer[16:16+captured],original)
                        with self.lock: self.counts[stream['direction']]+=size
                        del buffer[:16+captured]
                if time.monotonic()-last>=1: self.flush(); last=time.monotonic()
            self.flush()
        except Exception as error: self.error=str(error)

    def close(self):
        # QEMU must have exited before closing its capture pipe.
        time.sleep(.12)
        self.stopped=True; self.thread.join(timeout=3)
        for fd,stream in self.inputs.items():
            os.close(fd); stream['path'].unlink(missing_ok=True)
