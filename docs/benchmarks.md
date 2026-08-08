# Benchmarks

Le rapport complet, avec la méthodologie, l'historique des trois
campagnes, l'instrumentation et l'analyse des goulots, est dans
**[`BENCHMARK.md`](../BENCHMARK.md)** (racine du dépôt).

## Résumé

| Métrique | Valeur |
|---|---|
| Propose (chemin complet), c=1 | 10 972 req/s |
| Propose, c=16 | 14 407 req/s (p50 0.78 ms) |
| Endpoints légers (`/health`, `/v1/identity`), c=16 | 17 402 – 18 724 req/s |
| Signature / vérification Ed25519 | 14.0 µs / 40.5 µs |
| Append spine SQLite | 4.36 µs |

Historique du propose (req/s) :

| Configuration | c=1 | c=16 |
|---|---|---|
| Séquentiel, avant audit de perf | 602 | 41 |
| + fixes quadratiques | 10 357 | 15 294 |
| + worker pool (actuel) | 10 972 | 14 407 |

Reproduction :

```bash
cargo bench --bench protocol                 # couche protocole (criterion)
cargo run --release --example http_bench 5   # couche HTTP (serveur in-process)
```
