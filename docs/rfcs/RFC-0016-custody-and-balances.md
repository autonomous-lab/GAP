# RFC-0016: Custody Modes, Prefunded Balances & Proof of Reserves

**Status:** Implemented
**Author(s):** Celene Jimari (Prospective Analysis, Geta.Team)
**Area:** Payment / Governance
**Created:** 2026-08-10
**Targets:** 0.2.0
**Supersedes:** none (extends spec part 05, RFC-0012)

## 1. Summary

The protocol has always said *when* money moves. It has never said **who
holds it in between**, and it has never given a buyer a way to find out
before committing.

This RFC adds three things:

1. **A declared custody mode.** Every node states in its AgentCard
   whether it is `non-custodial`, `custodial` or `hybrid`, under whose
   legal entity, and with what withdrawal SLA. Agents filter on it the
   way they already filter on reputation.
2. **Prefunded balances.** An agent deposits once and spends many times.
   Settlement between deposits is a ledger entry, not a transaction.
3. **Proof of reserves.** The node periodically signs an attestation
   that its holdings cover its liabilities, and the liabilities are
   independently recomputable from the audit spine.

The point is not to declare custody safe. It is to make it **legible**:
a measurable, comparable risk instead of an act of faith.

## 2. Motivation

### 2.1 On-chain per job does not survive its own arithmetic

A GAP contract is designed to be worth five cents. Settling one on chain
costs two transactions — park and release — each carrying an ERC-20
transfer.

| | Value | On-chain cost |
|---|---|---|
| One 0.05 contract | $0.05 | ~$0.002 to $0.010 in gas (L2) |
| A 2% protocol fee on it | $0.001 | ~$0.001 to collect |

**The fee costs about what it is worth to collect.** That is not fixable
by raising the rate: at these tickets, any percentage is dominated by the
cost of moving it. It is fixable by *not moving it per job*.

A prefunded balance changes the denominator. Gas is paid twice per agent
**lifetime**, not twice per contract:

| Deposit | Contracts at 0.05 | On-chain transactions |
|---|---|---|
| 50 USDC | 1,000 | 2 |
| 500 USDC | 10,000 | 2 |

### 2.2 Custody is orthogonal to openness

An open protocol does not have to be non-custodial. Mail is an open
protocol with custodial mailboxes; the specification governs the
messages, the operator governs the mailbox.

What changes under custody is **where the trust boundary sits**: on the
protocol for the rules, on the operator for the funds. A protocol that
leaves this implicit forces every buyer to guess. A protocol that
declares it lets the market price it.

### 2.3 Non-custodial remains correct above a threshold

Nothing here removes the on-chain path. A contract worth 500 USDC should
not sit in an operator's ledger to save $0.004 of gas. The two models
are complementary and the threshold is the design, not a compromise.

## 3. Custody modes

A node MUST declare exactly one mode.

| Mode | Who holds funds between park and release | Gas per contract |
|---|---|---|
| `non-custodial` | an escrow contract; the node relays and cannot move funds | 2 transactions |
| `custodial` | the node operator, against a ledger | none |
| `hybrid` | the operator below a declared threshold, the contract above it | none, then 2 |

Rules:

1. A node MUST NOT claim `non-custodial` if any code path lets the
   operator move user funds without a party's signature. Holding an
   agent's signing key in custody **does** constitute this, unless the
   settlement contract requires a signature the operator cannot produce.
2. A node in `hybrid` mode MUST declare the threshold and the currency
   it is denominated in.
3. The mode is a property of the node, not of a contract. A single
   contract's settlement path follows from the mode plus the threshold,
   and MUST be recorded in the contract's terms so both parties signed
   it.

## 4. Declaration

The AgentCard (RFC-0010) gains a `custody` object. It is REQUIRED for a
node that accepts real value.

```json
{
  "custody": {
    "mode": "hybrid",
    "threshold": { "amount": "25.000000", "currency": "USDC" },
    "operator": {
      "legal_name": "Geta.Team SAS",
      "jurisdiction": "FR",
      "id": "FR-123456789"
    },
    "withdrawal_sla_seconds": 86400,
    "proof_of_reserves": "/v1/reserves",
    "settlement_contract": "0x…"
  }
}
```

- `mode` — REQUIRED, one of the three above.
- `threshold` — REQUIRED for `hybrid`, forbidden otherwise.
- `operator` — REQUIRED when `mode != non-custodial`. An anonymous
  custodian is not a custodian anyone should use.
- `withdrawal_sla_seconds` — REQUIRED when funds can be held. Breaching
  it is an incident under RFC-0012, with the reputation consequences of
  any other SLA breach.
- `proof_of_reserves` — REQUIRED when funds can be held.
- `settlement_contract` — REQUIRED when the mode can settle on chain.

A client SHOULD refuse to park value on a node whose declaration is
absent, or whose mode it does not accept. Discovery SHOULD expose the
mode so agents can filter before negotiating rather than after.

## 5. Prefunded balances

### 5.1 Lifecycle

```
POST /v1/balance/deposit    credit an agent's balance (on-chain proof, or operator credit)
GET  /v1/balance            the agent's own balance and its ledger
POST /v1/balance/withdraw   request funds out; subject to the declared SLA
```

Escrow park in custodial mode debits the balance instead of moving
tokens. Release credits the provider's balance. Refund reverses. The
state machine of spec part 05 is unchanged: only the ledger behind it
differs.

### 5.2 Every movement is an event

Each credit, debit, hold and release appends to the audit spine. This is
the load-bearing property of the whole design:

> **The operator's liabilities are recomputable by anyone**, from
> events, without trusting a figure the operator reports.

A balance is therefore not an assertion. It is a fold over a signed,
append-only log, and a node that misreports one is contradicted by its
own history.

### 5.3 How funds arrive, and how receipt is checked

A deposit is never **asserted**. It is **observed**.

The first cut of this endpoint accepted an `amount` from the caller and
credited it. That is a faucet: the depositor is precisely the party that
benefits from overstating the figure. The amount MUST come from the
rail, never from the request.

**On-chain rail.** The agent transfers the settlement token to the
node's deposit address and hands the node a transaction hash. The node
then reads the chain and decides for itself:

| Check | Why it is load-bearing |
|---|---|
| the transaction succeeded | a reverted transfer moved nothing |
| the log is a `Transfer` of the configured token | any contract can emit an event that looks like one |
| the recipient is this node's deposit address | otherwise an agent credits itself by pointing at somebody else's payment |
| the confirmation depth is sufficient | a reorg can unmake a transfer |
| the hash has not been credited before | replaying one transfer is the cheapest attack of the lot |

The crediting event records the transaction hash, the sender and the
depth, so a disputed credit is traceable to something outside the node's
own word.

Sub-unit dust MUST truncate down, never round up. Crediting value that
was not sent puts the ledger above its reserves, which is exactly what
§6 exists to catch.

**Attribution.** Nothing on chain says which agent a transfer belongs
to. A node MUST resolve this deliberately: a deposit address derived per
agent, a deposit contract carrying the agent identifier in its calldata,
or a sender address the agent has proved it controls. Crediting on an
unproven claim of a sender address lets one agent capture another's
payment.

**Operator rail.** For rails the node cannot read (bank transfer, card),
the operator credits the balance under its own authority. This MUST be
admin-gated and MUST carry an external reference. An operator crediting
a balance out of nothing is precisely what proof of reserves exposes, so
it had better point at something outside this node.

A custodial node that can verify neither rail MUST refuse deposits
rather than credit them on trust.

**Chargebacks.** A card payment reversed after the balance has been
spent is an unrecoverable loss for the operator, not for the protocol.
Operators using reversible rails SHOULD hold a settlement delay at least
as long as the chargeback window.

### 5.4 Insufficient funds

A park that exceeds the available balance MUST be refused before the
contract advances, with the shortfall stated. It MUST NOT be allowed to
settle into a negative balance: a custodian that extends credit is a
lender, which is a different regulated activity.

Balance checks compose with the principal's daily budget (spec 06 §6.5).
Both apply; the stricter wins.

## 6. Proof of reserves

A custodial node MUST publish, at `proof_of_reserves`, a signed
attestation:

```json
{
  "at": 1786300000,
  "spine_seq": 128417,
  "liabilities": { "amount": "1284.150000", "currency": "USDC" },
  "holdings":    { "amount": "1300.000000", "currency": "USDC" },
  "holding_accounts": ["0x…"],
  "signature": "ed25519:…"
}
```

- `spine_seq` pins the attestation to a point in the audit spine, so a
  verifier can replay events up to that sequence and recompute
  `liabilities` independently. An attestation that cannot be checked
  this way is decoration.
- `holdings` MUST cover `liabilities`. A node whose attestation shows a
  shortfall is insolvent and SHOULD stop accepting deposits.
- Attestations SHOULD be published at least daily and MUST be appended
  to the spine themselves.

This is proof of *reserves*, not proof of *solvency*: it says nothing
about obligations recorded elsewhere. Saying so plainly is part of the
mechanism, because a partial proof presented as a total one is worse
than none.

## 7. What this does not fix

Stated because omitting it would make this RFC a sales document.

1. **Custody is a regulated activity.** Holding third-party funds
   attaches obligations to *each operator*, in its own jurisdiction —
   licensing, AML, segregation of client funds. An open protocol
   distributes that duty; it does not dilute it.
2. **Proof of reserves is a snapshot.** It does not prevent a solvent
   node from becoming insolvent one second later.
3. **A declared SLA is not a guarantee.** It converts a breach into a
   visible, permanent reputational fact. That is a deterrent, not an
   escrow.
4. **A node's reputation is not yet portable.** Agents carry signed
   history between nodes; nodes do not. Until they do, the market signal
   on an operator is weaker than the one on an agent.

## 8. Security considerations

- **Do not conflate key custody with fund custody.** A node holding an
  agent's Ed25519 seed can already act as it. A node claiming
  `non-custodial` while holding the key that authorises settlement is
  making a false claim, and this RFC calls it that.
- **The threshold is an attack surface.** An agent that wants custodial
  treatment can split one large contract into many small ones. Nodes
  SHOULD track cumulative exposure per counterparty, not just per
  contract.
- **Withdrawal is the moment of truth.** Rate-limit it against theft;
  do not rate-limit it into an excuse. The declared SLA is the contract.
- **Deposits are attacker-controlled input.** An on-chain deposit MUST
  be credited only after the configured confirmation depth, and the
  crediting event MUST record the transaction hash so a disputed credit
  can be traced.

## 9. Open questions

1. **Node reputation.** Operators should carry a public track record
   like agents do — withdrawal latency, attestation regularity,
   incidents. That is a protocol change of its own.
2. **Cross-node balances.** An agent with a balance on node A cannot
   spend it on node B. Whether it should is unresolved; the honest
   default for now is that it cannot.
3. **Float.** Interest on held deposits is real revenue and the single
   clearest reason a regulator will take an interest. Left out
   deliberately.

## 10. Conformance

A node claiming RFC-0016 support MUST:

- expose `custody` in its AgentCard, and honour it;
- refuse a park that exceeds an agent's available balance;
- append every balance movement to the audit spine;
- publish a signed, spine-pinned reserves attestation when it can hold
  funds;
- expose the custody mode through discovery.

A node MUST NOT declare `non-custodial` while retaining any unilateral
path to user funds.
