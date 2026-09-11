"""Explicit provider costs. Empty values mean unknown, not free."""
from decimal import Decimal
import re
from pricing_env import explicit_values

FIELDS={
 'GAP_COST_NODE_MONTH_USD':'node_month_microdollars',
 'GAP_COST_EXTRA_DISK_MONTH_USD':'extra_disk_month_microdollars',
 'GAP_COST_NETWORK_IN_GB_USD':'network_in_gb_microdollars',
 'GAP_COST_NETWORK_OUT_GB_USD':'network_out_gb_microdollars',
}
VERSION='GAP_COST_VERSION'


def costs_from_env(path):
    values=explicit_values(path,FIELDS,VERSION)
    if not re.fullmatch(r'[A-Za-z0-9_.-]{1,64}',values[VERSION]):raise ValueError('invalid cost version')
    result={'version':values[VERSION]}
    for name,field in FIELDS.items():
        value=values[name]
        if not value:result[field]=None;continue
        if not re.fullmatch(r'[0-9]{1,7}(?:\.[0-9]{1,6})?',value):raise ValueError('invalid cost amount')
        amount=int(Decimal(value)*1000000)
        if amount>10**12:raise ValueError('cost exceeds limit')
        result[field]=amount
    return result
