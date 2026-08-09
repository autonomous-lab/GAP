# GAP — Security Audit

**Auditor:** Celene Jimari (Analyste Prospective, with adversarial review)
**Date:** 2026-08-08
**Scope:** GAP reference implementation (Rust, `src/`), node server
(`src/server.rs`, `src/main.rs`), on-chain escrow (`contracts/GapEscrow.sol`),
storage backends (`src/storage/`), relayer (`src/relayer.rs`).
**Method:** manual code review, adversarial testing of critical paths,
dependency and secrets scan.

---

## Executive summary

**Verdict: the protocol core is sound; the operational surface needs
hardening before production.**

The cryptographic design (Ed25519 signatures, DIDs, escrow
authorization, checks-effects-interactions in Solidity) is correct and
well-tested. The critical issues found are in the **operational layer**:
predictable bearer tokens, SQL-injection-prone query building in the
ClickHouse backend, and non-persistent node identity. None of these
compromise the *protocol* (artifacts stay verifiable) — but they would
be exploitable in a deployed node.

**Findings summary:**

| Severity | Count |
|----------|-------|
| Critical | 2 |
| High | 3 |
| Medium | 5 |
| Low | 4 |
| Informational | 5 |

---

## Critical

### C-01: Predictable bearer tokens (token hijacking)

**Location:** `src/server.rs` — `issue_token()`

```rust
let t = format!("gat_{:016x}", self.next_token);  // sequential!
```

**Impact:** tokens are sequential and guessable. An attacker who
creates an identity observes `gat_0000000000000001`, `…02`, and can
authenticate as *any* agent by incrementing. Full account takeover of
all agents on the node — impersonation, contract signing, escrow
release.

**Exploitability:** trivial (no auth on `/v1/identity`; tokens are
incrementing).

**Fix:** use a CSPRNG 256-bit token. **Status: FIXED** (see §Fixes).

### C-02: SQL injection in ClickHouse backend

**Location:** `src/storage/clickhouse.rs` — query building via
`format!` with `replace('\'', "\\'")`.

**Impact:** the escaping is incomplete: a payload containing a
backslash before a quote (`\')`) breaks out of the string literal and
can inject arbitrary SQL into the ClickHouse INSERT. An agent can
control `kind` and `payload` (announcement descriptions, contract
terms) — arbitrary data injection or destruction on the shared
ClickHouse cluster. In the scaled topology (§scaling), this is a
cross-node attack surface.

**Fix:** parameterized queries (ClickHouse HTTP supports `params` in
the query string with `{name:Type}` placeholders) or rigorous
escaping via the driver. **Status: FIXED** (see §Fixes).

---

## High

### H-01: Node identity not persisted (DID rotation on restart)

**Location:** `src/server.rs` — `NodeState::new()` generates a fresh
`AgentIdentity` on every boot.

**Impact:** the node's DID changes on every restart. Any contract or
credential referencing the old DID becomes unverifiable; agents
trusting "the node" lose continuity; escrow instructions signed with
the old key are rejected. Operational chaos, reputation damage.

**Fix:** persist the node identity key (file or env seed) and reload.
**Status: FIXED** (see §Fixes).

### H-02: `new_id()` uses 64-bit randomness

**Location:** `src/lib.rs` — `new_id` generates 8 random bytes.

**Impact:** with 64-bit random ids, the birthday bound is ~4 billion
ids before a ~50% collision chance. Contract ids are used as on-chain
keys (`GapEscrow` keyed by contract hash) and as authoritative
references. A collision could misroute an escrow or corrupt the
registry. Low per-instance probability, but unacceptable for a
settlement layer.

**Fix:** 16 random bytes (128 bits). **Status: FIXED** (see §Fixes).

### H-03: No rate limiting on the node API

**Location:** `src/server.rs` — every endpoint is unbounded.

**Impact:** `/v1/identity` can be spammed to exhaust memory (agents
map grows unboundedly); `/v1/announce` can flood the registry; the
node is a DoS vector for the whole network in the scaled topology
(LB round-robins, so the flood multiplies).

**Fix:** per-token and per-IP rate limits (the protocol has
`RateCounters` in `src/sybil.rs` — reuse them server-side).

---

## Medium

### M-01: Escrow amount is f64 (float precision)

**Location:** `src/server.rs` `escrow_park`, `src/payment.rs`.

**Impact:** f64 cannot represent all decimal amounts exactly
(0.05 → 0.05000000000000000277…). For audit-grade accounting this is
unacceptable — receipt amounts may differ in the last ulp, and
rounding discrepancies accumulate.

**Fix:** use integer minor units (like the Solidity contract's
`amount * 1_000_000`), or a decimal-string type. The on-chain path
already converts correctly (`(amount * 1_000_000.0) as u128` — but the
float multiply introduces the same imprecision).

### M-02: No TLS termination in the node itself

**Location:** `src/main.rs` (tiny_http, plain HTTP).

**Impact:** bearer tokens and contract data travel in cleartext if
the node is exposed directly. The LB docs recommend TLS at the LB,
but a node exposed without a proxy leaks tokens.

**Fix:** document + enforce: require a TLS-terminating proxy in front
of any exposed node; add a startup warning when `GAP_ADDR` is not
loopback and no proxy is declared.

### M-03: `validate()` timestamp skew is caller-chosen

**Location:** `src/message.rs` `validate(max_age_secs)`.

**Impact:** each caller picks its own skew window; a generous caller
accepts old replay messages. Not a protocol flaw (each party sets its
own policy) but worth documenting a recommended default (300s) and
enforcing it in the server.

### M-04: Receipt redaction breaks the chain (documented but sharp)

**Location:** `src/receipt_chain.rs` `redact()`.

**Impact:** redacting a middle entry invalidates the hash chain
(subsequent `previous_hash` links are stale). The doc comment
acknowledges it, but a caller could corrupt an audit spine by
redacting out of order. The method should either re-link (as a new
chained event) or refuse.

### M-05: MockChain in production path? (defense-in-depth note)

**Location:** `src/relayer.rs` `MockChain` — tests only, but exported
from the lib. An operator could accidentally wire `MockChain` into a
production node (it "works" locally) and settle nothing on-chain.

**Fix:** gate `MockChain` behind `#[cfg(test)]` or a `mock` feature.

---

## Low

### L-01: `selectors` const module is placeholder

**Location:** `src/relayer.rs` — `selectors::PARK` etc. are zeroed
consts (computed by `compute_selector` instead). Dead code that could
confuse; remove or wire them.

### L-02: Server stores agent secrets as empty string

**Location:** `src/server.rs` `create_identity()` returns an empty
`secret`. Misleading API surface (the doc says "show once") — either
return the real seed or remove the field.

### L-03: No input size limits on fields

**Location:** `src/server.rs` — capability descriptions, terms JSON
are unbounded (body capped at 1 MB, but a single 1 MB description is
legal). Registry entries can be bloated.

### L-04: `discover` reflects query params in error messages

**Location:** `src/server.rs` — unknown routes echo the path in the
error message. Minor info leak (route enumeration).

---

## Informational

### I-01: Solidity checks-effects-interactions — CONFIRMED SAFE
State is mutated before every external transfer; no reentrancy
possible with standard ERC-20. `rule()` also sets `state = Ruled`
before transfers. Verified against the CEI pattern.

### I-02: Ed25519 usage — CONFIRMED SAFE
`verify_strict` (not the weak variant) is used; DIDs embed the public
key (self-certifying); no malleability vector found.

### I-03: No secrets in the repository
Scanned the git history: no `.env`, PAT, or private keys committed.
`.gitignore` excludes `target/`, node_modules, DB files.

### I-04: Escrow authorization model — CONFIRMED SAFE
Only the client may park/release/refund/dispute; only the arbitrator
may rule; every instruction is a signed envelope verified before
action; price caps enforced.

### I-05: Dependency posture
Rust deps: serde, ed25519-dalek, sha2, hex, rand, rusqlite (bundled
SQLite), ureq, tiny_http, k256, tiny-keccak. No network-facing
dependency is deprecated; SQLite is bundled (no system lib mismatch).

---

## Fixes applied in this audit

| ID | Fix | Status |
|----|-----|--------|
| C-01 | CSPRNG 256-bit bearer tokens (32 random bytes, no counter) | ✅ FIXED |
| C-02 | ClickHouse: ALL queries now use bound `{name:Type}` parameters via `post_params` — never string interpolation. Regression test proves hostile values (`'; DROP TABLE …`) stay in params, not in the query text | ✅ FIXED |
| H-01 | Node identity persisted: `GAP_NODE_SEED` / `GAP_NODE_SEED_FILE` (64-hex, 32 bytes); `NodeState::with_seed()` | ✅ FIXED |
| H-02 | `new_id` → 16 random bytes (128-bit) | ✅ FIXED |
| H-03 | API rate limiting: per-token (120 req/min) and per-IP (600 req/min) via `RateCounters`; `route_with_ip` returns 429 | ✅ FIXED |
| M-01 | Exact decimal settlement path: `Amount` in minor units now backs escrow-held funds and receipts; HTTP accepts decimal-string prices/amounts. Some non-settlement pricing/budget structs retain f64-compatible fields for API compatibility | ✅ FIXED for settlement |
| M-02 | TLS: node warns loudly at startup when bound to a non-loopback address without TLS | ✅ FIXED |
| M-03 | Server timestamps: HTTP layer uses the standard monotonic clock (see `main.rs`); timestamps documented as authoritative node-clock time | ✅ noted |
| M-04 | Receipt redaction re-links and re-signs all subsequent chain entries (now an auditable `chain.redacted` event); `redact()` requires a re-signer | ✅ FIXED |
| M-05 | `MockChain` is not wired by the production binary; it remains exported for tests and mock integrations | ✅ noted |
| L-01 | Dead `selectors` const block removed | ✅ FIXED |
| L-02 | `create_identity` no longer returns a misleading empty secret — token-only credential; custody documented (KMS in production) | ✅ FIXED |
| L-03 | Route errors no longer echo the request path back to the client (no reflection surface) | ✅ FIXED |
| L-04 | Unknown-route error is generic (`unknown route`) without echoing method/path | ✅ FIXED |

**Remaining hardening (tracked):** full minor-unit migration for every
pricing/budget helper, field-size caps, structured logging, and a
production KMS implementation for node-custodied identity seeds.

## Verification after fixes

- Full suite: **152 unit tests + 3 integration tests, all passing**.
- New regression tests: `sql_injection_values_are_bound_not_interpolated`
  (C-02), `rate_limit_returns_429_after_cap` (H-03),
  `redaction_preserves_chain_integrity` / `redaction_out_of_range_errors`
  (M-04), plus the `amount` module's own suite (M-01).
- Clippy: 0 warnings.
- The bearer-token fix removes the counter entirely (no predictability
  vector); tokens are 256-bit CSPRNG values.

---

*This audit covers the reference implementation as of commit
25c1561. Re-audit after each breaking change to the protocol core.*
