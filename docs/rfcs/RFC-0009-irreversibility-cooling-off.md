# RFC-0009: Irreversibility & Cooling-Off

**Status:** Draft
**Author(s):** Celene Jimari
**Area:** Safety
**Created:** 2026-08-08
**Targets:** 0.2.0
**Supersedes:** none

## 1. Summary

Autonomous agents will eventually perform actions that cannot be
undone: releasing funds, publishing content, sending irrevocable
messages, executing legal filings. This RFC classifies action
**irreversibility**, mandates **cooling-off windows** for the most
consequential classes, and defines the **Withdrawal Receipt** that
proves no irreversible execution occurred. It is the safety layer
that makes *autonomy* sellable to the regulated world.

## 2. Motivation

OAP RFC-0017 defines irreversibility classes with mandatory cooling-off
and withdrawal receipts; the EU AI Act requires human oversight for
high-risk autonomous actions. GAP's escrow already provides a natural
cooling-off mechanism (funds parked before release) but nothing
covers *actions outside escrow*: publishing, sending, committing.
Without this RFC, an autonomous agent can cause irreversible harm with
no procedural brake.

## 3. Specification

### 3.1 Terminology

| Term | Definition |
|------|------------|
| **Irreversible action** | An action whose effect cannot be undone by any subsequent action. |
| **Cooling-off period** | Mandatory delay between consent and execution. |
| **Withdrawal receipt** | Signed proof of consent withdrawal within the window. |

### 3.2 Irreversibility classes

| Class | Examples | Default cooling-off |
|-------|----------|---------------------|
| `reversible` | draft, query, analysis | none |
| `external` | send email, publish post, API call | 0s (class default) |
| `financial` | transfer > threshold, settlement | 1h ≤ 1000 units; 24h above |
| `legal` | filings, registrations, contracts with human counterparties | 24h |
| `medical` | health-data disclosure, treatment scheduling | 24h |

Providers MUST declare `irreversibility_class` per capability.
False classification (declaring `reversible` when actually
`financial`) forfeits conformance and triggers liability per
RFC-0012.

### 3.3 Cooling-off flow (normative)

1. Agent requests execution of an irreversible action.
2. Runtime issues a **Pending Receipt** (`irreversible_pending`)
   recording consent, class, cooling-off duration, and withdrawal
   endpoint.
3. The action MUST NOT execute until the window elapses.
4. If the principal withdraws: a **Withdrawal Receipt**
   (`irreversible_withdrawn`) is issued, any conditional payment is
   refunded, and reservations are cleared.
5. If the window elapses: execute, issue the final receipt, chain it
   (RFC-0003).
6. **Waiver:** the principal MAY waive cooling-off per class, per
   provider, with explicit elevated consent and expiry ≤ 30 days.
   Blanket waivers are forbidden.

### 3.4 Integration

- **Escrow:** `pay.release` to a `financial` settlement is subject to
  the window (park → wait → release), reusing the existing escrow
  state machine.
- **Workflows (RFC-0002):** steps declaring irreversible capabilities
  require sponsor consent + window before `wf.start`.
- **Policy engine (RFC-0004):** irreversible actions raise the L3/L4
  threshold to `require_human_review` by default.

### 3.5 Conformance requirements

- Declare irreversibility class per capability.
- Enforce cooling-off windows (never shorter than the class default).
- Issue pending/withdrawal/final receipts, chained.
- Support explicit, bounded waivers; forbid blanket waivers.

## 4. Security & privacy considerations

- **Window abuse:** providers MUST NOT execute early; a Pending
  Receipt proves they were told to wait.
- **Waiver coercion:** waivers require explicit elevated consent and
  expire; no standing blanket waivers.
- **Timing oracle:** cooling-off leaks the existence of pending
  actions; acceptable for v0.1, noted for future ZK work.

## 5. Backward compatibility

Capabilities gain an optional `irreversibility_class` (default
`reversible`). Existing capabilities remain conformant. New receipts
are additive.

## 6. Reference implementation

- `src/irreversibility.rs`:
  - `IrreversibilityClass` enum + `default_cooling_off(class)`
  - `CoolingOffTimer { started_at, duration, deadline() }`
  - `PendingReceipt`, `WithdrawalReceipt`, `FinalReceipt`
  - `enforce_window(consent, now) -> Result<()>`
  - Tests: window enforcement, early-execution block, withdrawal path,
    waiver bounds, class defaults.

## 7. Review notes

- (pending)

## 8. Open questions

- Partial reversibility (e.g. revocable publish then take-down):
  proposal — `external` with `remediation: "take_down"` flag.

## 9. References

- GAP spec part 04 (execution), part 05 (payment).
- OAP RFC-0017 (irreversibility and cooling off), EU AI Act Art. 14.
