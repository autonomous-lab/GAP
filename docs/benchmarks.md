# Benchmarks

The full report — methodology, the history of the three campaigns, the
instrumentation, and the bottleneck analysis — is in
**[`BENCHMARK.md`](../BENCHMARK.md)** (repository root).

## Summary

| Metric | Value |
|---|---|
| Propose (full path), c=1 | 10,972 req/s |
| Propose, c=16 | 14,407 req/s (p50 0.78 ms) |
| Light endpoints (`/health`, `/v1/identity`), c=16 | 17,402 – 18,724 req/s |
| Ed25519 sign / verify | 14.0 µs / 40.5 µs |
| SQLite spine append | 4.36 µs |

Propose history (req/s):

| Configuration | c=1 | c=16 |
|---|---|---|
| Sequential, before the perf audit | 602 | 41 |
| + quadratic fixes | 10,357 | 15,294 |
| + worker pool (current) | 10,972 | 14,407 |

To reproduce:

```bash
cargo bench --bench protocol                 # protocol layer (criterion)
cargo run --release --example http_bench 5   # HTTP layer (in-process server)
```
