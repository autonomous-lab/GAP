# Serverless microVM rollout

The worker adds disk hibernation after 900 seconds of inbound inactivity,
automatic protocol wake, per-agent always-on permission, prepaid resource metering
and 72-hour unpaid storage retention. Native programs and optional Compose use
the same VM, network and billing paths.

## Validation

- 46 worker unit tests: lifecycle validation, quota and port isolation, immutable
  tariffs, atomic/idempotent charges, fractional carry, budget behavior,
  recharge/deletion boundary and host queue counters independent of guest MAC.
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

The first eight-request batches completed in approximately 1.3–1.7 seconds in
isolated runs. One batch took 7.6 seconds while other KVM/Compose tests were building
on the same host. These are acceptance results, not a latency SLA or a substitute
for representative workload benchmarks. HTTP retains its response timeout after
connection establishment; initial short connect timeouts no longer truncate
slow responses. Test workers use separate catalog directories and wake ports.

## Operating limits and activation

Production rollout starts in `shadow` without a tariff. No fabricated production
prices or test credit balances are installed. Real debit and the automatic
72-hour zero-credit deletion policy require operator activation of `enforced`
after rates and initial balances are configured. The implementation of that
policy is exercised with isolated test rates; elapsed retention is accelerated
in the test account rather than waiting three days.

Pricing still requires the host cost, margin and chosen cash-to-credit conversion.
CPU/RAM are charged by allocation while QEMU exists. Persistent disk is charged
while retained, including when execution stops for a budget. A budget is an
execution threshold, not a waiver of storage costs or a hard final spending cap.
MicroVM credits are separate from existing Realtime credits.

An abrupt host failure can lose the last unflushed meter interval. The ledger
avoids fabricated ON time across an unknown worker outage. Monitor ledger and
snapshot disk growth, preserve the SQLite WAL in backups, and keep snapshot
QEMU/image versions pinned. Silent external connections must reconnect after
hibernation. See the operator and agent documentation for the full contract.
