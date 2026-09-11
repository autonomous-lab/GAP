import json
import os
from pathlib import Path
import struct
import tempfile
import time
import unittest
from meter import PacketMeter, packet_bytes

class MeterTests(unittest.TestCase):
    def test_host_queue_direction_cannot_be_spoofed_with_guest_mac(self):
        with tempfile.TemporaryDirectory() as root:
            meter=PacketMeter(root,'52:54:00:00:00:01')
            self.addCleanup(meter.close)
            # Arbitrary Ethernet addresses: neither equals the configured guest.
            packet=b'x'*12+b'\x08\x00'+b'\x45\x00'+struct.pack('!H',1234)+b'\x00'*78
            header=struct.pack('<IHHIIII',0xa1b2c3d4,2,4,0,0,96,1)
            record=struct.pack('<IIII',0,0,96,1248)+packet
            for direction,count in [('in',1),('out',2)]:
                fd=os.open(str(Path(root)/('meter-'+direction+'.fifo')),os.O_WRONLY)
                os.write(fd,header+record*count);os.close(fd)
            deadline=time.monotonic()+2
            while time.monotonic()<deadline and meter.values()!=(1234,2468):time.sleep(.01)
            self.assertEqual(meter.values(),(1234,2468))
            meter.flush()
            self.assertEqual(json.loads((Path(root)/'network-counts.json').read_text()),{'in':1234,'out':2468})
    def test_ipv6_and_non_ip(self):
        packet=b'x'*12+b'\x86\xdd'+b'\x60\x00\x00\x00'+b'\x00\x64'+b'\x00'*34
        self.assertEqual(packet_bytes(packet),140)
        self.assertEqual(packet_bytes(b'x'*12+b'\x08\x06'+b'x'*60),0)
