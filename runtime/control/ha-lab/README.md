# Isolated fleet-control HA prototype

This lab runs **three real PostgreSQL instances managed by Patroni and three etcd
members** on an internal Docker network. It uses a small synthetic wallet, not
GAP's live authority or customer data. No ports are published. All six containers
run on one test host: this validates a process/network scenario, not independent
physical failure domains or a production HA deployment.

## Run

Requires a Linux Docker host with at least 3 GiB spare RAM and access to download
the pinned images/dependencies. Do not run the trust-authenticated configuration
on a shared/external network. The fixture disables the watchdog and uses test-only
credentials; it must not be promoted into a production configuration.

```sh
docker build -t gap-ha-lab-patroni runtime/control/ha-lab
docker pull quay.io/coreos/etcd:v3.6.4
python3 runtime/control/ha-lab/run.py
```

PostgreSQL 17.6, Patroni 4.1.5, psycopg 3.2.10 and etcd 3.6.4 are pinned for this
experiment, not endorsed as production patch levels. The runner refuses to reuse
an existing `gap-ha-lab-net`, removes only containers/configs created by its run,
and saves `/tmp/gap-ha-lab-result.json`. A hard process kill can bypass cleanup;
inspect the `gap.ha-lab=true` label before removing interrupted lab resources.

## What passed

1. The primary has two streaming replicas and a synchronous standby recorded
   both in PostgreSQL **and in etcd** before fault injection.
2. A 25-credit debit is confirmed against a synthetic 100-credit wallet.
3. Disconnecting the primary's Docker network leaves its SQL process reachable
   through local `docker exec`, but it cannot confirm another debit.
4. A synchronous successor is elected. The previously confirmed debit remains.
   Retrying a 10-credit request twice produces one receipt and a balance of 65.
5. Two concurrent requests for the remaining 65 credits yield exactly one success.
   Total receipts sum to 100; balance is zero.
6. Reconnecting the old primary returns it as a read-only replica with the same
   receipt total and exactly one primary in the cluster.

Observed time from network isolation through successful retry on the successor:
**20.91 seconds** in one successful run. This includes failure detection, election,
synchronous readiness and test requests; it is not an election-only measurement,
a p95 or an SLA. The approximately five-second GAP policy lease would expire
before this recovery completed. No production lease was extended.

## Finding from the first run

The first run checked only `pg_stat_replication.sync_state`. Its etcd `/sync`
record still had no synchronous standby, and election remained blocked after the
primary disconnected. No debit was confirmed on the isolated primary. Waiting
for the synchronous member to be published in the consensus state corrected the
fixture's readiness condition. The final run retains `maximum_lag_on_failover=0`;
no weakening to asynchronous replication was needed.

## Guarantees not established

- The real `Authority` implementation still uses SQLite. This SQL wallet is a
  contract experiment, not a PostgreSQL adapter for its full API/schema.
- No host/power failure, frozen Patroni process, watchdog/STONITH, loss of multiple
  data replicas, TLS identity, backup recovery or cross-datacenter outage was tested.
- Lab clients have administrative SQL access. Production clients must not be able
  to bypass synchronous durability or write to a stale primary. Reads from an old
  primary can also expose unacknowledged local transactions and must be gated.
- A timed-out transaction is ambiguous: retry the same operation ID. Never infer
  rollback just from a timeout or reissue a new debit identifier.

Next implementation: preserve the authority's transactional/idempotency contracts
in a PostgreSQL adapter, add leader-aware endpoints and independent fencing,
then run the existing authority tests plus targeted partitions. A real deployment
requires independent failure domains, secured replication and an explicit policy
for the measured recovery gap. The existing HA card remains open.

References: [Patroni synchronous modes](https://patroni.readthedocs.io/en/latest/replication_modes.html)
and [configuration](https://patroni.readthedocs.io/en/latest/yaml_configuration.html).
