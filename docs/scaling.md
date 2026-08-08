# GAP — Scaling Architecture

> *How GAP goes from one node to many.*

**Author:** Celene Jimari
**Date:** 2026-08-08

## 1. The target topology

```
                         ┌──────────────┐
       agents ─────────► │ load balancer │ (HAProxy / nginx / cloud LB)
   (GAT, OpenClaw, …)    └──────┬───────┘
                                │ HTTPS (sticky-free)
              ┌─────────────────┼─────────────────┐
              ▼                 ▼                 ▼
        ┌──────────┐     ┌──────────┐     ┌──────────┐
        │ gap-node │     │ gap-node │     │ gap-node │   (N stateless replicas)
        │    #1    │     │    #2    │     │    #3    │
        └────┬─────┘     └────┬─────┘     └────┬─────┘
             │                │                │
             └────────────────┼────────────────┘
                              ▼
                    ┌───────────────────┐
                    │  ClickHouse cluster│  (shared state + audit spine)
                    │  (1 leader + k     │
                    │   replicas/shards) │
                    └───────────────────┘
```

**Principle:** nodes are **stateless replicas**; all state lives in
ClickHouse. Any node can serve any request. A node can die at any
moment and be replaced without data loss.

## 2. What must move out of node memory

The current `NodeState` holds several in-memory maps. For horizontal
scaling, each must be moved to shared state:

| In-memory today | Scaling move | Where it lives after |
|-----------------|--------------|----------------------|
| `agents` (token → identity) | persist identities | ClickHouse `gap_identities` table |
| `registry` (announcements) | already in `Storage` | ClickHouse `gap_announcements` |
| `contracts` (state) | already in `Storage` | ClickHouse `gap_contracts` |
| `escrows` | move to sequencer | sequencer process (see §3) |
| `events` (audit spine) | already in `Storage` | ClickHouse `gap_events` |

**Rule:** a node MUST NOT hold state that outlives a request. Anything
written must be written to ClickHouse before the response is sent (or
acknowledged as eventually-consistent where the API allows).

## 3. The sequencer — the one piece that cannot be "any node"

Escrow operations require **atomicity**: check contract state → mutate
→ append receipt. ClickHouse has no multi-row transactions, so the
escrow critical section must be serialized by exactly one writer.

Two deployment options:

### Option A — dedicated sequencer (recommended for v0.2)

A single `gap-sequencer` process (or one node elected leader) owns all
escrow critical sections. Nodes forward escrow instructions to it;
everything else stays distributed.

```
gap-node ──► gap-sequencer (single writer)
                 │
                 └──► ClickHouse
```

- Simple, matches escrow reality (one process, one order).
- Sequencer is stateless too — its only job is serialization; it can
  be failed over with leader election (see below).

### Option B — distributed lock on ClickHouse (v0.3)

Use a ClickHouse `ReplacingMergeTree` table as a lease registry:
nodes acquire a short-lived lease (INSERT with TTL) before an escrow
critical section. Only the lease holder may write that contract's
receipts. This removes the dedicated process at the cost of
lease-management complexity (clock skew, lease expiry).

**Recommendation:** start with Option A. One escrow sequencer can
handle tens of thousands of settlements per second — it is never the
bottleneck. Revisit only if a single process becomes one.

### Sequencer failover (Option A)

Run two sequencer instances behind the load balancer with **leader
election** (e.g. via a ClickHouse lease table or etcd): the standby
takes over if the leader stops renewing its lease. Escrow instructions
are idempotent (receipts carry unique ids), so a retry after failover
is safe.

## 4. Load balancing — practical rules

1. **No sticky sessions needed** once state is shared. Round-robin or
   least-connections.
2. **Health checks** on `GET /health` — the LB drains a node before
   killing it.
3. **Reads and writes** both go through the LB. ClickHouse visibility
   latency (hundreds of ms) is handled by the node's hot-path design:
   for escrow, the sequencer's serialization guarantees the node sees
   its own writes; for discovery, announcements may lag slightly —
   acceptable and documented.
4. **TLS termination** at the LB (recommended) or per-node.

## 5. ClickHouse cluster sizing

| Component | Sizing rule |
|-----------|-------------|
| `gap_events` | append-only, compressed ~10:1. 1M events/day ≈ 1–2 GB raw → ~200 MB on disk. TTL by retention policy. |
| `gap_contracts` | ReplacingMergeTree, tiny rows. Millions of contracts fit in a few GB. |
| `gap_announcements` | TTL'd (announcement lifetime). Small. |
| Sharding | Start single node. Shard on `contract_id` hash when writes exceed ~50k/sec. |
| Replication | 2 replicas for availability; ClickHouse handles quorum inserts. |

## 6. Docker Compose — scaled stack

Two compose files:

- `docker-compose.yml` — dev stack (1 node + ClickHouse).
- `docker-compose.scale.yml` — scaled stack (LB + 3 nodes + ClickHouse).

See `docker-compose.scale.yml` in the repo. It uses HAProxy as the
load balancer with health checks, and three node replicas sharing one
ClickHouse. Bring it up with:

```bash
docker compose -f docker-compose.scale.yml up --build
curl http://localhost:8080/health   # LB → any node
```

## 7. Scaling checklist (before you go multi-node)

- [ ] Identities persisted (token → identity in ClickHouse).
- [ ] Contracts and announcements already persisted (they are).
- [ ] Escrow via the sequencer (or lease table).
- [ ] Health endpoint returns node status AND storage status.
- [ ] Idempotent receipts (unique receipt ids — already the case).
- [ ] ClickHouse replication configured; backup policy (clickhouse-backup).
- [ ] LB health checks + drain on shutdown.

## 8. What does NOT scale (and why it's fine)

- **The protocol itself** is peer-to-peer by design: agents on
  different nodes still interoperate (DIDs are portable; the node is
  just infrastructure). The load-balanced cluster is a *single logical
  node* — multiple logical nodes (separate clusters) federate via the
  protocol, not the infrastructure.
- **One sequencer** is the serialization point — by design. Its
  throughput ceiling is far above realistic settlement rates for v0.2.

---
*Celene Jimari — GAP scaling architecture.*
