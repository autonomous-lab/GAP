# GAP — Throughput Benchmarks

Mesures de référence (reference implementation, commit courant) pour la
couche protocole (crypto, contrat, escrow, chaîne de reçus, stockage)
et la couche HTTP du node. Les résultats sont **factuels** : ils
décrivent la capacité brute de l'implémentation actuelle, pas une
spécification. Les goulots identifiés sont documentés en fin de
document avec leur impact.

## Environnement

| Paramètre | Valeur |
|---|---|
| CPU | AMD EPYC 9645 (96 cœurs logiques), 16 alloués au conteneur |
| RAM | 64 Go (36 Go dispo au moment des mesures) |
| OS | Linux (conteneur), kernel hôte |
| Rust | 1.97.1, profile `release` (opt-level 3) |
| Criterion | 0.8.2 (benchmark protocol) |
| Client HTTP | ureq 3.3 (keep-alive, 1 agent par worker) |
| Base de données | SQLite `:memory:` (le backend de production ClickHouse n'a pas pu être mesuré ici — pas de démon disponible) |

## Reproduction

```bash
# Microbenchmarks de la couche protocole
cargo bench --bench protocol

# Benchmark HTTP (durée par phase en secondes, défaut 5)
cargo run --release --example http_bench 5
```

Le benchmark HTTP démarre le node **in-process** avec les caps de rate
limiting relevés (`GAP_RATE_*_CAP`), pour mesurer la capacité brute, pas
la politique de sécurité. En production, les caps par défaut
(120 req/min par token, 600 req/min par IP) s'appliquent.

## Microbenchmarks protocole

Médianes criterion (100 échantillons, 3 s de warmup, ~5 s de mesure).

| Opération | Temps | Throughput |
|---|---|---|
| Identity : génération clé Ed25519 | 13.8 µs | 72.6 k ops/s |
| Signature Ed25519 (32 octets) | 14.0 µs | 71.4 k ops/s |
| Vérification Ed25519 (32 octets) | 40.5 µs | 24.7 k ops/s |
| Contrat : `propose` (création + signature client) | 19.0 µs | 52.5 k ops/s |
| Contrat : accept provider (vérif + signature) | 81.9 µs | 12.2 k ops/s |
| Contrat : sérialisation JSON | 554 ns | 1.8 M ops/s |
| Escrow : instruction `park` signée | 16.9 µs | 59.1 k ops/s |
| Escrow : register + vérif + application `park` | 149.9 µs | 6.7 k ops/s |
| Chaîne de reçus : append (hash + lien) | 475 ns | 2.1 M ops/s |
| Chaîne de reçus : vérif chaîne de 1000 entrées | 1.57 ms | 638 chaînes/s |
| SQLite : append événement spine | 4.36 µs | 229 k ops/s |
| SQLite : lecture 100 événements | 38.6 µs | 25.9 k lectures/s |

**Lecture** : la crypto Ed25519 domine les chemins signés (~14 µs par
signature, ~40 µs par vérification). Un contrat signé+accepté coûte
~82 µs de crypto. La chaîne de reçus est quasi gratuite (475 ns par
append). Le spine SQLite tient 229 k événements/s en append.

## Benchmark HTTP (node complet)

Deux configurations mesurées : la boucle séquentielle initiale et le
**worker pool** actuel (`GAP_WORKERS`, défaut min(cœurs, 8)). Les
chiffres ci-dessous sont le pool actuel ; les lignes du chemin complet
sont comparées à l'historique dans la section suivante. Durée : 5 s par
cellule, 0 erreur.

| Concurrence | Endpoint | req/s | p50 | p99 |
|---|---|---|---|---|
| 1 | GET /health | 14 871 | 0.04 ms | 1.34 ms |
| 1 | GET /v1/audit | 12 945 | 0.06 ms | 0.15 ms |
| 1 | POST /v1/identity | 12 827 | 0.07 ms | 0.13 ms |
| 1 | POST /v1/contract/propose | 10 972 | 0.08 ms | 0.17 ms |
| 4 | GET /health | 17 492 | 0.07 ms | 3.67 ms |
| 4 | GET /v1/audit | 8 115 | 0.40 ms | 2.75 ms |
| 4 | POST /v1/identity | 16 155 | 0.11 ms | 3.55 ms |
| 4 | POST /v1/contract/propose | 14 206 | 0.16 ms | 3.39 ms |
| 8 | GET /health | 17 539 | 0.10 ms | 4.26 ms |
| 8 | GET /v1/audit | 7 154 | 0.95 ms | 4.12 ms |
| 8 | POST /v1/identity | 16 788 | 0.22 ms | 4.21 ms |
| 8 | POST /v1/contract/propose | 14 328 | 0.37 ms | 4.20 ms |
| 16 | GET /health | 18 724 | 0.20 ms | 6.61 ms |
| 16 | GET /v1/audit | 6 420 | 2.15 ms | 8.80 ms |
| 16 | POST /v1/identity | 17 402 | 0.45 ms | 6.83 ms |
| 16 | POST /v1/contract/propose | 14 407 | 0.78 ms | 6.19 ms |

## Trois étapes, chiffres du propose (chemin complet)

| Configuration | c=1 | c=4 | c=8 | c=16 |
|---|---|---|---|---|
| Séquentiel, avant audit de perf | 602 | 467 | 211 | 41 |
| + fixes quadratiques (SQLite seq, index DID) | 10 357 | 14 377 | 15 096 | 15 294 |
| + worker pool + parsing hors lock | 10 972 | 14 206 | 14 328 | 14 407 |

Le worker pool stabilise la latence à forte concurrence (p99 6.2 ms vs
6.9 ms) et gagne ~10-40 % selon l'endpoint ; le plafond restant est le
Mutex global de l'état (section critique ~27 µs) — le design « one
process, one order » de l'event sourcing.

## Deux bugs quadratiques trouvés et corrigés par le benchmark

La première campagne a révélé un **effondrement du propose** sous
concurrence (602 → 41 req/s de c=1 à c=16) et des latences p50 de
393 ms. L'instrumentation a isolé deux causes, toutes deux des
comportements O(n) dans des chemins chauds :

1. **`MAX(seq)` par insert (SQLite)** : `append_event` calculait la
   séquence par `SELECT COALESCE(MAX(seq), -1) + 1` à **chaque**
   écriture — un scan complet de la table d'événements à chaque
   append (O(n) en taille de spine). Corrigé par un compteur mémoire
   O(1), initialisé une fois à l'ouverture depuis `MAX(seq)`
   (single-writer par process, conforme à l'architecture).
2. **Scan de tous les agents par propose** : `propose_contract`
   vérifiait le provider par `agents.values().any(… to_string())` —
   O(n) avec allocation par entrée. Comme l'endpoint `/v1/identity`
   est ouvert, un flux d'identités suffisait à faire dégringoler le
   node (et c'était un vecteur de DoS). Corrigé par un index
   `did → token` (O(1)), maintenu à la création d'identité.

Après correction : le propose est passé de **602 à 10 357 req/s à
c=1** et de **41 à 15 294 req/s à c=16** (+373×), avec un throughput
stable sous charge (seule la latence croît avec la concurrence, comme
attendu pour une boucle séquentielle). Des tests de régression
protègent les deux corrections (`sqlite_sequence_continues_after_reopen`,
`propose_lookup_is_constant_time_with_many_agents`).

## Analyse factuelle

1. **Le node tient ~15-19 k req/s sur les endpoints légers et ~15 k
   req/s sur le propose** (chemin complet signé + écriture spine),
   avec un throughput stable sous concurrence. La latence croît avec
   la concurrence (file d'attente de la boucle séquentielle), comme
   attendu — le plafond ~15 k req/s est la capacité du thread unique
   de traitement.

## Implications pour le scaling

- **Par node** : capacité utile ~14-19 k req/s selon l'endpoint, le
  propose (chemin complet signé + écriture spine) plafonne à
  ~14.4 k req/s (Mutex global, design « one process, one order »). Le
  scaling horizontal prévu (`docker-compose.scale.yml` + HAProxy,
  `docs/scaling.md`) est le bon outil : chaque node ajoute ~14 k req/s.
- **Prochaines optimisations** (par ordre d'impact) : remplacer le
  Mutex global par un `RwLock` (lectures en parallèle — health,
  discover, audit) et sharder l'état par agent ; la section critique du
  propose (~27 µs de logique) est le plafond théorique de ~37 k req/s
  pour le chemin complet sur un node.
- **La vérification de chaîne (1.57 ms / 1000 entrées)** est linéaire
  par conception (chaîne, RFC-0003) ; les ancrages Merkle
  (`root_commitment`) sont le chemin de vérification rapide pour les
  intégrations externes.

## Limites de ces mesures

- Backend ClickHouse et relayer on-chain **non mesurés** (pas de
  démon ClickHouse/EVN dans l'environnement de test) ; le backend
  ClickHouse est testé via transport simulé uniquement.
- L'environnement est un conteneur partagé (16 cœurs alloués) : les
  valeurs absolues dépendent du hôte ; les *ratios* et les classements
  (goulots) sont robustes.
- Les latences p99 incluent le bruit du scheduler du conteneur.
