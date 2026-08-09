# GAP — Benchmark Report

Reference measurements of the Rust implementation (protocol layer and
node HTTP layer), with the full methodology, the history of the three
campaigns, the bugs the benchmark uncovered, and the remaining
bottlenecks. The numbers are **factual**: they describe the capacity of
the current implementation, not a specification.

---

## 1. Overview

| Metric | Measured value |
|---|---|
| Propose (full path: signature + spine), c=1 | **10,972 req/s** (p50 0.08 ms) |
| Propose, c=16 (highest concurrency tested) | **14,407 req/s** (p50 0.78 ms, p99 6.19 ms) |
| Light endpoints (`/health`, `/v1/identity`), c=16 | 17,402 – 18,724 req/s |
| `/v1/audit` (spine, 100 events), c=1 | 12,945 req/s |
| Ed25519 signing | 14.0 µs (71,400 ops/s) |
| Ed25519 verification | 40.5 µs (24,700 ops/s) |
| SQLite spine append | 4.36 µs (229,000 ops/s) |
| Receipt-chain append | 475 ns (2.1M ops/s) |

The node is **stable under load**: throughput no longer collapses at
high concurrency (the two quadratic bugs that caused the collapse are
fixed, §8). The current ceiling is the global state Mutex (~27 µs
critical section), a consequence of the event-sourcing "one process,
one order" design.

---

## 2. Environment and hardware

| Parameter | Value |
|---|---|
| CPU | AMD EPYC 9645 (96 logical cores), 16 allocated to the container |
| RAM | 64 GB (36 GB available at measurement time) |
| OS | Linux (container), host kernel |
| Rust | 1.97.1, `release` profile (opt-level 3) |
| Criterion | 0.8.2 (protocol-layer benchmarks) |
| HTTP client | ureq 3.3 (keep-alive, one agent per worker) |
| HTTP server | tiny_http 0.12 (internal thread pool + application pool `GAP_WORKERS`) |
| Database | SQLite `:memory:` (the ClickHouse production backend was not measured — no daemon available in the environment) |

**Validity notes**

- The environment is a shared container: absolute values depend on the
  host; the **ratios** and the **bottleneck rankings** are robust.
- p99 latencies include container scheduler noise.
- The HTTP benchmarks lift the rate-limiting caps (`GAP_RATE_*_CAP`):
  they measure **raw capacity**, not the security policy (production
  defaults: 120 req/min per token, 600 req/min per IP).

---

## 3. Methodology

### 3.1 Protocol layer (criterion)

`benches/protocol.rs` measures the hot paths that bound node
throughput: Ed25519 key generation/signing/verification, contract
creation (propose), provider acceptance, escrow instructions,
receipt-chain append and verification, SQLite spine append and read.
Criterion: 100 samples, 3 s warm-up, ~5 s of measurement per benchmark,
reported values = medians.

### 3.2 HTTP layer (`examples/http_bench.rs`)

The benchmark starts the node **in-process** on an ephemeral port with
the same server loop as `main.rs` (worker pool, `GAP_BENCH_WORKERS`,
default 8) and the rate-limiting caps lifted.

For each concurrency level c ∈ {1, 4, 8, 16}, a fixed number of client
workers (1 ureq agent each, keep-alive) hammers each endpoint for a
fixed duration (default 5 s):

- `GET /health` — unauthenticated (the lightest path)
- `GET /v1/audit` — authenticated, spine read (100 events)
- `POST /v1/identity` — authenticated, Ed25519 key generation
- `POST /v1/contract/propose` — authenticated, signature + spine
  write (the representative full path)

Each phase reports: req/s, p50, p99, error count. Workers are joined
between phases; a warm-up (identities + capability announcement)
precedes the measurements.

### 3.3 Reproducing

```bash
# Protocol layer (criterion)
cargo bench --bench protocol

# HTTP layer — in-process server, 8-worker pool, 5 s per phase
cargo run --release --example http_bench 5

# HTTP layer — external server (the gap binary), caps lifted
GAP_RATE_TOKEN_CAP=10000000 GAP_RATE_IP_CAP=10000000 ./target/release/gap
GAP_BENCH_TARGET=http://127.0.0.1:8080 ./target/release/examples/http_bench 5

# Pool size
GAP_BENCH_WORKERS=16 ./target/release/examples/http_bench 5
```

---

## 4. Microbenchmarks — protocol layer

Criterion medians (see §3.1).

| Operation | Time | Throughput |
|---|---|---|
| Identity: Ed25519 key generation | 13.77 µs | 72,600 ops/s |
| Ed25519 signing (32 bytes) | 14.00 µs | 71,400 ops/s |
| Ed25519 verification (32 bytes) | 40.50 µs | 24,700 ops/s |
| Contract: propose (creation + client signature) | 19.03 µs | 52,500 ops/s |
| Contract: provider accept (verify + sign) | 81.87 µs | 12,200 ops/s |
| Contract: JSON serialization | 553.7 ns | 1.81M ops/s |
| Escrow: signed park instruction | 16.91 µs | 59,100 ops/s |
| Escrow: register + verify + apply park | 149.9 µs | 6,700 ops/s |
| Receipt chain: append (hash + link) | 475.4 ns | 2.10M ops/s |
| Receipt chain: verify a 1,000-entry chain | 1.57 ms | 638 chains/s |
| SQLite: spine event append | 4.36 µs | 229,000 ops/s |
| SQLite: read 100 events | 38.6 µs | 25,900 reads/s |

**Reading the numbers**

- Ed25519 crypto dominates the signed paths: ~14 µs per signature,
  ~40 µs per verification. A signed + accepted contract costs ~82 µs of
  crypto.
- The receipt chain is nearly free: 475 ns per append. Verifying a
  1,000-entry chain is linear by design (RFC-0003); Merkle anchors
  (`root_commitment`) are the fast verification path for external
  integrations.
- The SQLite spine sustains 229k events/s on append (O(1) sequence
  counter, see §8).

---

## 5. HTTP benchmark — current configuration

Server: 8-worker pool (`GAP_WORKERS`/`GAP_BENCH_WORKERS`), JSON parsing
outside the global Mutex. Duration: 5 s per cell, 0 errors.

| Concurrency | Endpoint | req/s | p50 | p99 |
|---|---|---|---|---|
| 1 | GET /health | 14,871 | 0.04 ms | 1.34 ms |
| 1 | GET /v1/audit | 12,945 | 0.06 ms | 0.15 ms |
| 1 | POST /v1/identity | 12,827 | 0.07 ms | 0.13 ms |
| 1 | POST /v1/contract/propose | 10,972 | 0.08 ms | 0.17 ms |
| 4 | GET /health | 17,492 | 0.07 ms | 3.67 ms |
| 4 | GET /v1/audit | 8,115 | 0.40 ms | 2.75 ms |
| 4 | POST /v1/identity | 16,155 | 0.11 ms | 3.55 ms |
| 4 | POST /v1/contract/propose | 14,206 | 0.16 ms | 3.39 ms |
| 8 | GET /health | 17,539 | 0.10 ms | 4.26 ms |
| 8 | GET /v1/audit | 7,154 | 0.95 ms | 4.12 ms |
| 8 | POST /v1/identity | 16,788 | 0.22 ms | 4.21 ms |
| 8 | POST /v1/contract/propose | 14,328 | 0.37 ms | 4.20 ms |
| 16 | GET /health | 18,724 | 0.20 ms | 6.61 ms |
| 16 | GET /v1/audit | 6,420 | 2.15 ms | 8.80 ms |
| 16 | POST /v1/identity | 17,402 | 0.45 ms | 6.83 ms |
| 16 | POST /v1/contract/propose | 14,407 | 0.78 ms | 6.19 ms |

**Reading the numbers**

- Propose (the most expensive path: authentication + O(1) provider
  lookup + Ed25519 signature + spine write + response) sustains
  ~14.4k req/s at high concurrency with sub-millisecond p50s.
- Light endpoints plateau at ~17–19k req/s (processing-thread
  saturation; see §9 for the bottleneck).
- `/v1/audit` degrades slightly with spine size (`ORDER BY seq LIMIT`
  read + serialization of 100 events inside the critical section):
  12.9k → 6.4k req/s between c=1 and c=16.

---

## 6. History of the three campaigns

Propose (full path) by server configuration, for each concurrency
level:

| Configuration | c=1 | c=4 | c=8 | c=16 |
|---|---|---|---|---|
| Sequential loop, before the perf audit (v1) | 602 | 467 | 211 | **41** |
| + quadratic fixes (O(1) SQLite counter, O(1) DID index) | 10,357 | 14,377 | 15,096 | 15,294 |
| + worker pool + out-of-lock parsing (current) | 10,972 | 14,206 | 14,328 | 14,407 |

**Stages**

1. **v1 — collapse under load.** The first campaign revealed a dramatic
   collapse: 602 → 41 req/s from c=1 to c=16, p50 latencies of 393 ms.
   Instrumentation (§7) isolated two O(n) behaviors in hot paths
   (§8.1, §8.2).
2. **Quadratic fixes.** +17× at c=1, +373× at c=16, stable throughput.
3. **Worker pool.** Request parsing and response serialization now run
   in parallel; the global Mutex only serializes the protocol core.
   Modest throughput gain (the bottleneck is the Mutex, §9) but better
   p99 latencies at high concurrency (6.19 ms vs 6.86 ms at c=16).

---

## 7. Instrumentation — how the diagnosis was made

Debugging numbers, kept here for posterity and for reproducibility of
the analysis:

| Measurement | Value | Lesson |
|---|---|---|
| `route()` called directly (release, 20k iterations) | 27.1 µs/req | protocol logic is ~27 µs; the rest is plumbing |
| Instrumented server (`read_body` / `route` / `respond`) | 7 / 53 / 71 µs (132 µs total) | the server handles a propose in ~130 µs |
| In-process mini-bench, 1 worker, empty table | 10,804 req/s | the server alone is fast |
| Mini-bench + 65k created identities | 583 req/s | the O(n) agent scan was the killer |
| curl vs ureq, same server | 15 ms vs 1.37 ms (propose, SQLite file) | file fsync + `MAX(seq)` dominated; client keep-alive was not the factor |
| Main server (SQLite file) vs `:memory:` | 1.37 ms vs 0.2 ms per propose | the file backend adds commit/fsync cost |

**Dead ends eliminated**: the ureq client (clean POST, verified by
socket capture — no `Expect: 100-continue`, no chunked encoding),
tiny_http's internal pool (capacity-8 queue, adaptive pool), the rate
limiter (O(1)), the `MAX(seq)` counter alone (fixed but insufficient —
the agent scan was the other half).

**Documented methodology trap**: an early probe of `route()` measured
0.3 µs — a false result: the default rate limit (120 req/min per
token) returned instant errors after 120 calls. Performance probes
must lift the caps or count response statuses.

---

## 8. Bugs found and fixed thanks to the benchmark

### 8.1 SQLite: `MAX(seq)` per insert (O(n) per write)

`append_event` computed the sequence with
`SELECT COALESCE(MAX(seq), -1) + 1 FROM events` on **every** insert —
a full scan of the events table per write. The bigger the spine, the
slower every write (O(n) in spine size). **Fixed** with an O(1)
in-memory counter, initialized once at open from `MAX(seq)`
(single-writer per process, consistent with the architecture).
Regression test: `sqlite_sequence_continues_after_reopen`.

### 8.2 Server: full agent scan per propose (O(n), DoS vector)

`propose_contract` verified the provider with
`agents.values().any(|a| a.identity.did().to_string() == provider)` —
O(n) **with an allocation per entry**. Since `POST /v1/identity` is
unauthenticated, a stream of identity creations was enough to bring
the node down. **Fixed** with a `did → token` index (O(1)), maintained
at identity creation. Regression test:
`propose_lookup_is_constant_time_with_many_agents` (5,000 agents,
200 proposes, bound < 5 ms/req).

### 8.3 Security: `/v1/audit` readable without authentication

Found by the exhaustive route tests (`tests/http_routes.rs`): the
tamper-evident spine was readable anonymously — it is evidence, not
public data. **Fixed**: authentication required (400 without a valid
token).

### 8.4 API consistency and missing routes

Also via the route tests: `GET /v1/contract/{id}` returned the state
in Debug format (`"Draft"`) while every other route uses the lowercase
wire format (`"draft"`); and four documented routes were not
implemented (`/v1/escrow/release`, `/v1/escrow/refund`,
`/v1/escrow/rule`, `/v1/contract/{id}/dispute`). All fixed, along with
the requirement that the contract be signed before park (explicit
`escrow_violation` error).

---

## 9. Analysis of the current bottlenecks

1. **The global state Mutex is the ceiling.** Every request holds the
   lock through the processing core (rate limit + lookup + crypto +
   spine write + response construction), ~27 µs for a propose. The
   theoretical single-node ceiling is therefore ~37k req/s on the full
   path; the measured ~14.4k req/s reflects real std Mutex contention
   at high concurrency.
2. **`/v1/audit`**: the `ORDER BY seq LIMIT` read is O(log n + limit),
   but serializing the 100 events inside the critical section and the
   response size (tens of KB) bound throughput to ~6–13k req/s
   depending on load.
3. **Chain verification (1.57 ms / 1,000 entries)** is linear by
   design (a chain, RFC-0003); `root_commitment` (Merkle) is the fast
   path for external verifiers.
4. **Crypto is not an HTTP bottleneck**: signing a contract costs
   19 µs; the server spends most of its time in the lock and in
   plumbing.

### Next optimizations (by impact)

- Replace the global `Mutex` with an `RwLock`: reads (`/health`,
  `/v1/discover`, `/v1/audit`, `GET /contract/{id}`) become parallel;
  writes stay serialized by event sourcing.
- Shard state by agent (writes to different contracts stop blocking
  each other).
- Batch spine events (group commit) to amortize SQLite commit cost on
  the file backend.

---

## 10. Scaling implications

- **Per node**: ~14k req/s on the full path, ~17–19k req/s on light
  endpoints. The planned horizontal scaling
  (`docker-compose.scale.yml` + HAProxy, `docs/scaling.md`) is the
  right tool: each node adds ~14k req/s of processing capacity.
- The node is designed to be **protocol-stateless** (the spine is the
  source of truth, states are materialized): horizontal scaling needs
  no inter-node coordination for contracts (each node serves its own
  fleet of agents).
- The ClickHouse backend (production) targets the read/analytics
  patterns that degrade SQLite (`/v1/audit` at scale); the spine is
  indexed there by `(kind, seq)`.

---

## 11. Limits of these measurements

- **ClickHouse backend not measured** (no daemon in the test
  environment); it is tested via a simulated transport only. The
  SQLite numbers are a lower bound for writes and an (unfavorable)
  upper bound for bulk reads.
- **On-chain relayer not measured** (no EVM node available); the
  measured escrow operations are the off-chain reference path.
- The environment is a shared container (16 cores allocated from a
  96-core EPYC): absolute values depend on the host.
- The HTTP benchmark measures raw capacity (rate-limiting caps
  lifted); in production, the default caps (120/600 per minute)
  deliberately reduce the throughput observable per client.

---

## 12. References

- Tools: `benches/protocol.rs` (criterion), `examples/http_bench.rs`
  (HTTP), `tests/http_routes.rs` (route coverage).
- Deployment architecture: `docs/deployment.md`, `docs/scaling.md`.
- Reference document for the measured API: `docs/node-api.md`.
- Security: `SECURITY-AUDIT.md` (the §8 fixes are referenced there).

---
*Celene Jimari — GAP benchmark report, observation window 2026.*
