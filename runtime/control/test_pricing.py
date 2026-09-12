import unittest
from pricing import Prices,validate

class PricingTests(unittest.TestCase):
    def sample(self):return dict(available=True,mode='enforced',currency='USD',microcredits_per_credit=1000000,
        tariff=dict(version='v1',vcpu_hour=10,gib_ram_hour=20,gb_disk_month=30,gb_in=40,gb_out=50))
    def test_registry_change_and_outage_expire_cached_price(self):
        clock=[0];reply=[self.sample()];calls=[]
        def fetch(source):
            calls.append(source['node_id'])
            if reply[0] is None:raise OSError()
            return reply[0]
        prices=Prices([dict(node_id='node')],clock=lambda:clock[0],fetch=fetch)
        self.assertEqual(prices.get()['node']['tariff']['version'],'v1')
        reply[0]=None;self.assertTrue(prices.get()['node']['available']);self.assertEqual(len(calls),1)
        clock[0]=31;self.assertEqual(prices.get()['node']['available'],False)
        clock[0]=62;reply[0]=self.sample();reply[0]['tariff']['version']='v2'
        self.assertEqual(prices.get()['node']['tariff']['version'],'v2')
    def test_shadow_unknown_units_or_invalid_rates_never_publish(self):
        for key,value in [('mode','shadow'),('currency','EUR'),('microcredits_per_credit',1),('available',False)]:
            report=self.sample();report[key]=value
            with self.assertRaises(ValueError):validate(report)
        report=self.sample();report['tariff']['vcpu_hour']=-1
        with self.assertRaises(ValueError):validate(report)
    def test_only_public_fields_are_returned(self):
        report=self.sample();report['secret']='must not leak'
        self.assertNotIn('secret',validate(report))

if __name__=='__main__':unittest.main()
