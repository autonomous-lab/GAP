# GAP — Deployment & Storage Architecture

> *How GAP is persisted in production.*

**Author:** Celene Jimari
**Date:** 2026-08-08

## 1. The hybrid storage model

GAP's truth lives in **signed artifacts**, not in storage. The storage
layer answers only "where do the bytes live" — never "what is true".
Two backends are provided behind the `Storage` trait:

| Backend | When | Why |
|---------|------|-----|
| `SqliteStorage` | Development, tests, single-agent deployments | ACID, zero config, one file |
| `ClickHouseStorage` | Production, multi-agent scale | Append-only event spine + materialized state, massive compression, analytics on the same store |

The design follows the event-sourcing spine:

```
signed artifacts (envelopes, contracts, receipts)
        │
        ▼
  event spine (append-only, seq-numbered)
        │
        ▼
  materialized state (contracts, announcements) — derived, replaceable
```

## 2. ClickHouse specifics

ClickHouse is an OLAP store. GAP's event-sourcing makes this a
feature, not a workaround:

- **Events** live in a `MergeTree` table ordered by `seq` — append-only,
  compressed, trivially queryable for audit ("all events for contract X").
- **State** lives in `ReplacingMergeTree` tables keyed by
  `contract_id` / `agent_did` — upserts collapse to the latest row on
  merge. State is *derived*, never authoritative.
- **Hot state** is mirrored in the sequencer process for point reads;
  ClickHouse serves analytics and cold reads.

### 2.1 The sequencer (atomicity)

ClickHouse has no multi-row transactions. Operations that require
atomicity — escrow park/release, budget check-then-spend — go through
the [`Sequencer`](../src/storage/clickhouse.rs): a single-process
writer that serializes critical sections, writes the event, then
confirms. This matches the reality of an escrow agent: one process,
one order, one truth.

Pattern:

```rust
sequencer.critical(|state| {
    // check budget / contract state
    // mutate state
    // append event
    Ok(())
})?;
```

### 2.2 Visibility latency

ClickHouse inserts are batch-optimized; point reads may lag by
hundreds of milliseconds. Critical reads (budget, announcement
validity) MUST go through the sequencer's hot mirror, not the cluster.

## 3. The Storage trait contract

```rust
pub trait Storage {
    fn append_event(&mut self, kind: &str, payload: Value) -> Result<u64>;
    fn events_after(&self, seq: u64, limit: u64) -> Result<Vec<EventRecord>>;
    fn event_count(&self) -> Result<u64>;
    fn upsert_contract(&mut self, record: &ContractRecord) -> Result<()>;
    fn get_contract(&self, contract_id: &str) -> Result<Option<ContractRecord>>;
    fn contracts_in_state(&self, state: &str) -> Result<Vec<ContractRecord>>;
    fn upsert_announcement(&mut self, record: &AnnouncementRecord) -> Result<()>;
    fn get_announcement(&self, agent_did: &str) -> Result<Option<AnnouncementRecord>>;
    fn reap_expired(&mut self) -> Result<usize>;
}
```

**Cross-backend guarantee:** the conformance suite
(`storage::test_helpers::run_conformance_suite`) runs against BOTH
backends in CI — SQLite and ClickHouse must behave identically on the
trait contract. If a new backend passes the suite, it is storage-
conformant by construction.

## 4. Runtime integration

`Runtime::set_storage(Box<dyn Storage>)` attaches a backend; every
subsequent protocol event (`ctr.bound`, `ctr.transition`, …) is
appended to the spine automatically. The runtime stays functional
without storage (in-memory mode) — storage is an enhancement, not a
dependency.

## 5. Recommended production topology

```
┌─────────────────────────────────────────────┐
│  Sequencer process (single writer)          │
│  • escrow critical sections                 │
│  • budget check-then-spend                  │
│  • hot state mirror (in-memory)             │
└──────────────┬──────────────────────────────┘
               │ append
               ▼
┌─────────────────────────────────────────────┐
│  ClickHouse cluster                         │
│  • gap_events  (MergeTree, by seq)          │
│  • gap_contracts (ReplacingMergeTree)       │
│  • gap_announcements (ReplacingMergeTree)   │
│  • analytics & dashboards on the same store │
└─────────────────────────────────────────────┘
```

## 6. Why not everything on-chain?

Blockchain remains the **settlement and anchoring layer** only (spec
07, RFC-0003). Storage is off-chain because: micro-transactions are
too cheap to settle per-contract on-chain; enterprises will not put
NDA-covered data on public ledgers; and the event spine + signatures
already provide tamper-evidence at storage level. The chain is the
court, not the filing cabinet.

---
*Celene Jimari — GAP deployment architecture, observation window 2026.*
