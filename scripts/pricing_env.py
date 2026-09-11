"""Read explicit USD tariffs without evaluating a deployment .env as shell code."""
from decimal import Decimal
from pathlib import Path
import re

FIELDS = {
    'GAP_PRICE_VCPU_HOUR_USD': 'vcpu_hour',
    'GAP_PRICE_RAM_GIB_HOUR_USD': 'gib_ram_hour',
    'GAP_PRICE_DISK_GB_MONTH_USD': 'gb_disk_month',
    'GAP_PRICE_NETWORK_IN_GB_USD': 'gb_in',
    'GAP_PRICE_NETWORK_OUT_GB_USD': 'gb_out',
}
VERSION = 'GAP_PRICING_VERSION'


def tariff_from_env(path):
    values = {}
    for line in Path(path).read_text().splitlines():
        line = line.strip()
        if not line or line.startswith('#') or '=' not in line:
            continue
        name, value = line.split('=', 1)
        name = name.strip().removeprefix('export ').strip()
        if name not in FIELDS and name != VERSION:
            continue  # Never print, interpolate or evaluate unrelated secrets.
        if name in values:
            raise ValueError('duplicate pricing setting: ' + name)
        value = value.strip()
        if value[:1] in ('"', "'"):
            quote = value[0]
            end = value.find(quote, 1)
            if end < 0 or (value[end+1:].strip() and not value[end+1:].strip().startswith('#')):
                raise ValueError('invalid pricing setting: ' + name)
            value = value[1:end]
        else:
            value = re.split(r'\s+#', value, maxsplit=1)[0].strip()
        values[name] = value
    missing = ({VERSION} | set(FIELDS)) - values.keys()
    if missing:
        raise ValueError('missing pricing settings: ' + ', '.join(sorted(missing)))
    if not re.fullmatch(r'[A-Za-z0-9_.-]{1,64}', values[VERSION]):
        raise ValueError('invalid pricing version')
    tariff = {'version': values[VERSION]}
    for name, field in FIELDS.items():
        # At most six decimal places: all accepted values are exact microcredits.
        if not re.fullmatch(r'[0-9]{1,7}(?:\.[0-9]{1,6})?', values[name]):
            raise ValueError('invalid USD amount: ' + name)
        amount = int(Decimal(values[name]) * 1_000_000)
        if amount > 10**12:
            raise ValueError('USD amount exceeds limit: ' + name)
        tariff[field] = amount
    if not any(tariff[field] for field in FIELDS.values()):
        raise ValueError('a production tariff cannot be entirely zero')
    return tariff
