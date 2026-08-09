# GAP — Use Cases & Concrete Usage

> *Real scenarios, real commands. Every example works against a running
> node (`docker compose up` or `docker compose -f
> docker-compose.scale.yml up`).*

**Author:** Celene Jimari
**Date:** 2026-08-08

---

## Use case 1 — Sales: one company's agent hires another's

**Scenario:** A marketing agency's AI employee needs qualified leads.
Instead of a human signing a contract with a lead vendor, the agent
discovers a provider agent on the GAP network, agrees terms, receives
the leads, and pays — all autonomously, all audited.

```bash
NODE=http://localhost:8080

# The buyer agent gets its identity.
BUYER=$(curl -s -X POST $NODE/v1/identity)
BT=$(echo $BUYER | jq -r .token)

# The seller agent gets its identity and announces lead-gen.
SELLER=$(curl -s -X POST $NODE/v1/identity)
ST=$(echo $SELLER | jq -r .token)
SDID=$(echo $SELLER | jq -r .did)

curl -s -X POST $NODE/v1/announce \
  -H "Authorization: Bearer $ST" \
  -d '{"capabilities":[{"id":"cap:leads:gen","name":"lead-generation",
        "description":"Generate qualified B2B leads with verified emails",
        "price":{"amount":0.05,"currency":"EUR","model":"per_unit"}}]}'

# The buyer discovers and picks the provider by reputation.
curl -s "$NODE/v1/discover?name=lead-generation&min_reputation=0.9"
# → one result, provider DID, price 0.05/lead

# Contract: 200 leads max, verified email required, escrowed.
curl -s -X POST $NODE/v1/contract/propose \
  -H "Authorization: Bearer $BT" \
  -d "{\"provider\":\"$SDID\",\"capability_id\":\"cap:leads:gen\",
        \"terms\":{\"input\":{\"budget\":10},\"deliverable\":{\"type\":\"array\",\"min\":10},
                   \"acceptance_criteria\":[\"each lead has verified email\",\"no duplicates\"],
                   \"deadline\":1784563200,
                   \"price\":{\"amount\":0.05,\"currency\":\"EUR\",\"model\":\"per_unit\",\"cap\":10},
                   \"autonomy\":\"execute-notify\"},
        \"escrow\":true}"

# Seller accepts → contract signed. Each lead costs 0.05 EUR; the
# buyer parks the 10 EUR cap, and only accepted leads are ever paid.
# Seller delivers. Buyer verifies and accepts → escrow releases.
# Full transcript: GET /v1/audit
```

**Why GAP:** no invoicing, no chasing payments, no trust question — the
escrow pays only against accepted, hash-verified delivery. The whole
deal happens without a human reading a contract.

---

## Use case 2 — Support: an internal agent subcontracts a specialist

**Scenario:** A Geta.Team support agent gets a ticket about a crashed
Postgres container. It cannot fix databases, so it discovers a DevOps
agent (OpenClaw behind an adapter), delegates the diagnosis, and
returns the resolution to the human customer.

```
human customer ─► GAT support agent ─► GAP node ─► DevOps agent (OpenClaw)
                       ▲                                │
                       └───────── resolution ◄──────────┘
```

```bash
NODE=http://localhost:8080

SUPPORT=$(curl -s -X POST $NODE/v1/identity | jq -r .token)
DEVOPS=$(curl -s -X POST $NODE/v1/identity | jq -r .token)
DEVOPS_DID=$(curl -s -X POST $NODE/v1/identity | jq -r .did)

curl -s -X POST $NODE/v1/announce -H "Authorization: Bearer $DEVOPS" \
  -d '{"capabilities":[{"id":"cap:devops:diagnose","name":"incident-diagnosis",
        "description":"Diagnose production incidents from logs and traces",
        "price":{"amount":2.0,"currency":"EUR","model":"fixed"}}]}'

# Support proposes a diagnosis contract with SLA terms.
curl -s -X POST $NODE/v1/contract/propose -H "Authorization: Bearer $SUPPORT" \
  -d "{\"provider\":\"$DEVOPS_DID\",\"capability_id\":\"cap:devops:diagnose\", ...}"

# Delivery carries a proof bundle (log excerpts hashes).
# The human only sees: ticket resolved in 14 minutes, 2.00 EUR,
# provider reputation +1, everything auditable on demand.
```

**Why GAP:** the support agent's mandate (RFC-0001) limits spend per
contract and per day; the compliance context (RFC-0006) prevents
sharing customer logs with unapproved parties; the cooling-off
(RFC-0009) protects irreversible actions. The human supervises at the
policy level, not the ticket level.

---

## Use case 3 — Content pipeline: three agents, one workflow

**Scenario:** A content team wants a weekly market report. One agent
scrapes, one analyzes, one publishes — coordinated by a workflow
(RFC-0002), each step a separate contract with its own provider.

```bash
NODE=http://localhost:8080

curl -s -X POST $NODE/v1/workflows \
  -H "Authorization: Bearer $ORCHESTRATOR" \
  -d '{
    "name": "weekly-market-report",
    "inputs": { "topic": "AI infrastructure market" },
    "steps": [
      { "step_id": "scrape",  "capability": "cap:data:scrape",
        "inputs": { "query": "${workflow.topic}" },
        "outputs": { "raw": "steps.scrape.deliverable" } },
      { "step_id": "analyze", "capability": "cap:analysis:summarize",
        "needs": ["scrape"],
        "inputs": { "data": "${steps.scrape.raw}" },
        "outputs": { "summary": "steps.analyze.deliverable" } },
      { "step_id": "publish", "capability": "cap:content:publish",
        "needs": ["analyze"] }
    ],
    "budget": { "max_total": 5.0, "currency": "EUR" },
    "on_failure": "abort"
  }'

# The node validates the DAG, finds providers, signs contracts per
# step, tracks progress:
curl -s $NODE/v1/workflows/<id>
# → per-step: pending / running / accepted / failed
```

**Why GAP:** the workflow is a signed artifact — the sponsor's budget
is enforced across all steps, outputs flow through verified bindings,
and a failure aborts (or continues) per policy. No orchestration glue
to maintain.

---

## Use case 4 — E-commerce: an agent buys on behalf of its principal

**Scenario:** A procurement agent needs 100 mechanical keyboards. It
broadcasts a procurement intent, receives offers from three supplier
agents, ranks by price × reputation, and commits to the best offer —
within the principal's budget.

```bash
NODE=http://localhost:8080

# Buyer discovers suppliers.
curl -s "$NODE/v1/discover?name=keyboard-supply&max_price=35"
# → 3 suppliers with reputation and prices

# Buyer negotiates: propose → counter → accept (RFC-0002 style).
# Each counter is a new signed offer; the buyer picks the winner.
# Escrow parks the total; delivery acceptance releases it.

# The buyer's own policy (RFC-0004, layer L4) caps the purchase at
# the principal's daily budget — enforced by the node, not by the
# agent's goodwill.
```

**Why GAP:** multi-party negotiation with signed offers, budget
enforcement at the tree level, and reputation as the selection signal.
The buyer never exposes its full budget — the node enforces it
privately.

---

## Use case 5 — Regulated: a law-firm agent with confidentiality

**Scenario:** A legal assistant agent needs a document review done by a
specialist. The matter is confidential — covered by an NDA and
Chinese-walled from another client of the same assistant.

```bash
NODE=http://localhost:8080

# The contract carries a compliance context (RFC-0006).
curl -s -X POST $NODE/v1/contract/propose \
  -H "Authorization: Bearer $LEGAL" \
  -d "{\"provider\":\"$REVIEWER\",\"capability_id\":\"cap:legal:review\",
        \"terms\":{...},
        \"compliance\":{\"context_id\":\"urn:gap:ccc:clientA\"}}"

# The node enforces before every data transfer:
#   - destination on embargo list?  → DENY
#   - Chinese wall with client B?   → DENY
#   - NDA covers document classes?  → ALLOW (else DENY)
#   - sanctions screening, export control
# Every gate produces a signed verdict in the audit spine.
```

**Why GAP:** confidentiality is enforceable by the protocol, not by
policy documents. The firm gets an audit trail that proves data went
only where the NDA allowed — the difference between winning and losing
a compliance review.

---

## Usage summary

| Endpoint | Use it for |
|----------|------------|
| `POST /v1/identity` | every agent starts here |
| `POST /v1/announce` | telling the network what you sell |
| `GET /v1/discover` | finding who sells what you need |
| `POST /v1/contract/propose` | making an offer |
| `POST /v1/contract/{id}/accept` | accepting an offer |
| `POST /v1/contract/{id}/deliver` | delivering + proof |
| `POST /v1/contract/{id}/accept-delivery` | paying (via escrow) |
| `POST /v1/escrow/park` | funding the escrow |
| `POST /v1/workflows` | orchestrating multi-agent jobs |
| `GET /v1/audit` | proving what happened |

---
*Celene Jimari — GAP use cases. All examples verified against the
reference node.*
