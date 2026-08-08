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

Serveur mono-processus, boucle `recv → route → respond` séquentielle,
état partagé derrière un `Mutex` global (design actuel). Durée : 5 s par
cellule, 0 erreur.

| Concurrence | Endpoint | req/s | p50 | p99 |
|---|---|---|---|---|
| 1 | GET /health | 14 871 | 0.04 ms | 1.34 ms |
| 1 | GET /v1/audit | 14 267 | 0.06 ms | 0.13 ms |
| 1 | POST /v1/identity | 13 356 | 0.06 ms | 0.12 ms |
| 1 | POST /v1/contract/propose | 602 | 1.60 ms | 2.58 ms |
| 4 | GET /health | 16 335 | 0.08 ms | 3.92 ms |
| 4 | GET /v1/audit | 6 656 | 0.58 ms | 0.79 ms |
| 4 | POST /v1/identity | 15 894 | 0.11 ms | 3.79 ms |
| 4 | POST /v1/contract/propose | 467 | 8.06 ms | 16.5 ms |
| 8 | GET /health | 17 683 | 0.13 ms | 4.31 ms |
| 8 | GET /v1/audit | 6 177 | 1.29 ms | 1.72 ms |
| 8 | POST /v1/identity | 16 445 | 0.22 ms | 4.22 ms |
| 8 | POST /v1/contract/propose | 211 | 34.3 ms | 66.3 ms |
| 16 | GET /health | 19 213 | 0.24 ms | 7.28 ms |
| 16 | GET /v1/audit | 5 222 | 2.76 ms | 7.08 ms |
| 16 | POST /v1/identity | 17 360 | 0.48 ms | 6.54 ms |
| 16 | POST /v1/contract/propose | 41 | 393 ms | 409 ms |

## Analyse factuelle

1. **Le node léger tient ~15-19 k req/s.** Les endpoints sans écriture
   (`/health`, `/v1/identity`) plafonnent autour de 15-19 k req/s et
   *s'améliorent* légèrement avec la concurrence jusqu'à saturation CPU
   de la boucle unique (~19 k req/s à c=16).

2. **Le propose (chemin complet) plafonne à ~600 req/s en mono-client
   et s'effondre sous concurrence** (602 → 41 req/s de c=1 à c=16,
   latence p50 1.6 ms → 393 ms). Deux causes identifiées :
   - la boucle serveur traite les requêtes **séquentiellement** (un
     seul thread `recv/respond`), donc la file s'allonge en O(concurrence) ;
   - chaque requête tient le **Mutex global** pendant toute la chaîne
     (rate limit → parse → signature → écriture spine), ce qui
     sérialise tout et aggrave la contention à mesure que la file
     grossit.
   Le chemin propose réel coûte ~19 µs de crypto mais ~1.6 ms de bout en
   bout (parsing HTTP + JSON + Mutex + SQLite + réponse) — un facteur
   ~85× qui vient de la plomberie serveur, pas du protocole.

3. **`/v1/audit` se dégrade avec la durée du bench** (14 k → 5 k req/s) :
   la table d'événements grossit (~200 k lignes en fin de bench) et la
   lecture fait un scan croissant. C'est cohérent avec le backend
   SQLite de dev ; ClickHouse (MergeTree, indexation par `(kind, seq)`)
   est le backend prévu pour ce pattern en production.

4. **La crypto n'est pas le goulot HTTP.** Signer un contrat coûte
   19 µs ; le serveur passe 98 % du temps dans la plomberie.

## Implications pour le scaling

- **Par node** : capacité utile ~15 k req/s (lectures/identités) et
  ~600 propose/s (écritures signées). Le scaling horizontal prévu
  (`docker-compose.scale.yml` + HAProxy, `docs/scaling.md`) est le bon
  outil : chaque node ajoute ~600 propose/s de capacité de traitement.
- **Le Mutex global et la boucle séquentielle sont les premiers
  optimisations à faire** avant tout autre travail de perf : passer la
  boucle serveur sur un pool de threads (le trait `Storage` est déjà
  `Send + Sync`) et réduire la section critique (rate limit → sans
  lock, ou shardé par token) multiplierait le propose par ~10-20.
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
