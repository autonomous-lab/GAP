# Serverless microVM rollout

The worker adds disk hibernation after 900 seconds of inbound inactivity,
automatic protocol wake, per-agent always-on permission, prepaid resource metering
and 72-hour unpaid storage retention. Native programs and optional Compose use
the same VM, network and billing paths.

## Validation

- 52 worker unit tests: lifecycle validation, quota and port isolation, immutable
  tariffs, atomic/idempotent charges, fractional carry, budget behavior,
  recharge/deletion and budget-period boundaries, and host queue counters independent of guest MAC.
- 6 access CLI tests and 3 agent CLI tests.
- Rust private-node policy and Cloud surface tests, plus a real node build.
- Browser console checked in Chromium with simulated owner API responses:
  balance/state rendering, execution mode job polling and credit budget conversion.
- Real QEMU/KVM tests in public and private node modes: native Python application,
  in-flight HTTP protection, four cycles of eight concurrent wake requests per
  mode, preserved application identity, POST, WebSocket, TCP, first UDP datagram,
  SSH wake, outgoing-only idle, host counters, real isolated test credit debit,
  zero-balance blocking, recharge cancellation and accelerated 72-hour deletion.
- Shared-origin and optional Compose regression tests cover owner approval,
  quotas, ports/SSH keys, native applications without Docker, Compose environment,
  HTTP/POST/query/assets/redirect/WebSocket, persistence, resize and destruction.

A final stress run completed 50 successive restore cycles with eight concurrent
HTTP requests per cycle. Each batch completed in 1.252–1.582 seconds; the initial
automatic idle wake completed in 1.429 seconds. This used the small native test
application and is not a latency SLA for larger memory working sets or host load.
The final worker quarantines recently used TCP source ports for 120 seconds of
guest execution, persisting that clock and history across hibernation. Packet
header traces identified new SYNs colliding with guest-retained TCP state after
slirp restarted. Merely binding a random source port was insufficient; the
quarantine prevents that reuse. Queued requests recheck the VM generation and
active ingress before connecting to prevent forwarding to a reassigned port.
HTTP retains its response timeout after
connection establishment; initial short connect timeouts no longer truncate
slow responses. Test workers use separate catalog directories and wake ports.

## Operating limits and activation

Production now uses `enforced` billing with the approved `usd-v1` tariff.
The GAP project received its authorized initial 100-credit allocation before
enforcement. The operator endpoint verified balance 100,000,000 microcredits,
execution allowed, and no storage deletion deadline. Fresh installations still
start in `shadow` without a tariff and must fund accounts before enforcement.
No production usage was fabricated to test billing; unit tests use isolated
ledgers. The 72-hour retention policy was exercised with accelerated test time.

GAP hosted pricing uses **1 credit = USD 1** (1,000,000 microcredits):
**USD 0.010/vCPU-hour**, **USD 0.010/GiB RAM-hour**, **USD 0.10/GB disk-month**,
and **USD 0.01/GB in each network direction**. CPU/RAM bill only while ON;
physical stored data includes hibernation snapshots and remains billable while OFF.
A disk-month means 730 hours, prorated by elapsed time. GB means 1,000,000,000
bytes; GiB means 1,073,741,824 bytes. Conversions and fractional carry are exact.

The hosted tariff is `runtime/compose/pricing-usd-v1.json`. Production activation
is verified through the operator pricing endpoint; fresh workers still default
to shadow mode.
CPU/RAM are charged by allocation while QEMU exists. Persistent disk is charged
while retained, including when execution stops for a budget. A budget is an
execution threshold, not a waiver of storage costs or a hard final spending cap.
MicroVM credits are separate from existing Realtime credits.

An abrupt host failure can lose the last unflushed meter interval. The ledger
avoids fabricated ON time across an unknown worker outage. Monitor ledger and
snapshot disk growth, preserve the SQLite WAL in backups, and keep snapshot
QEMU/image versions pinned. Silent external connections must reconnect after
hibernation. See the operator and agent documentation for the full contract.
