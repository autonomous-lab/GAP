"""Public active tariffs from configured nodes, cached for at most 30 seconds."""
import concurrent.futures
import json
import threading
import time
import urllib.parse
import urllib.request
from finance import NoRedirect

FIELDS={'vcpu_hour','gib_ram_hour','gb_disk_month','gb_in','gb_out'}

def validate(value):
    tariff=value.get('tariff')
    if (value.get('available') is not True or value.get('mode')!='enforced'
        or value.get('currency')!='USD' or value.get('microcredits_per_credit')!=1000000
        or not isinstance(tariff,dict) or set(tariff)!={'version',*FIELDS}
        or not isinstance(tariff['version'],str) or not 1<=len(tariff['version'])<=64
        or any(type(tariff[k]) is not int or not 0<=tariff[k]<=10**12 for k in FIELDS)):
        raise ValueError('pricing_unavailable_or_invalid')
    return dict(available=True,mode='enforced',currency='USD',microcredits_per_credit=1000000,tariff=tariff)

class Prices:
    def __init__(self,sources,clock=time.monotonic,fetch=None):
        self.sources=sources;self.clock=clock;self.fetch=fetch or self.read
        self.lock=threading.Lock();self.until=0;self.values={}

    @staticmethod
    def read(source):
        url=urllib.parse.urlsplit(source['url'])
        request=urllib.request.Request(urllib.parse.urlunsplit((url.scheme,url.netloc,'/v1/pricing','','')),
                                      headers={'User-Agent':'GAP-Pricing/1.0'})
        with urllib.request.build_opener(NoRedirect()).open(request,timeout=3) as response:raw=response.read(8193)
        if len(raw)>8192:raise ValueError()
        return json.loads(raw)

    def get(self):
        with self.lock:
            if self.clock()<self.until:return self.values
            def one(source):
                try:result=validate(self.fetch(source))
                except Exception:result={'available':False}
                return source['node_id'],dict(result,checked_at=int(time.time()),max_age_seconds=30)
            with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:self.values=dict(pool.map(one,self.sources))
            self.until=self.clock()+30
            return self.values
