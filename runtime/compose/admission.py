"""Read-only readiness for NEW VM admissions; never changes existing leases."""
from billing import BillingError


def check(ledger):
    config=getattr(ledger,'config',None)
    if config is None:return dict(ready=True,scope='local',errors=[])
    errors=[];pricing=ledger.pricing();expected=config.get('expected_tariff')
    if pricing['mode']!='enforced':errors.append('fleet_requires_enforced_billing')
    if not expected:errors.append('fleet_expected_tariff_not_configured')
    elif pricing['tariff']!=expected:errors.append('fleet_active_tariff_mismatch')
    try:
        result=ledger.transport({'action':'readiness'})
        if result.get('operator_id')!=config['operator_id'] or result.get('node_id')!=config['node_id']:
            errors.append('fleet_readiness_identity_mismatch')
        if result.get('protocol')!='fleet-admission-v1':errors.append('fleet_admission_protocol_incompatible')
        if result.get('reservations') is not True:errors.append('fleet_reservations_disabled')
        if result.get('capacity') is not True:errors.append('fleet_capacity_disabled')
    except BillingError as error:errors.append(str(error))
    except (ValueError,TypeError,AttributeError,OSError):errors.append('fleet_readiness_invalid_response')
    return dict(ready=not errors,scope='fleet',errors=errors,
                active_tariff_version=(pricing.get('tariff') or {}).get('version'))
