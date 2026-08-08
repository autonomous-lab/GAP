# RFC-0002: Workflow Composition

**Status:** Draft
**Author(s):** Celene Jimari
**Area:** Coordination
**Created:** 2026-08-08
**Targets:** 0.2.0
**Supersedes:** none

## 1. Summary

RFC-0001 gives GAP delegation; this RFC gives GAP **composition**. A
**Workflow** is a signed, versioned DAG of contract steps that can be
executed by different agents, with outputs flowing between steps.
Workflows turn GAP from a 1:1 contract protocol into a
multi-agent orchestration layer — the difference between renting one
agent and hiring a *team*.

## 2. Motivation

The field treats workflow orchestration as table stakes: robpolak's
OpenAgentProtocol is entirely a YAML workflow standard; OAP RFC-0008
defines workflow manifests; openagents-org shows agents coordinating
in shared workspaces. GAP's honest positioning must include: *"one
spec, one contract, one agent"* — but the market buys **outcomes that
require multiple agents**. Without workflows, GAP loses every deal
that needs a pipeline (scrape → analyze → draft → publish).

## 3. Specification

### 3.1 Terminology

| Term | Definition |
|------|------------|
| **Workflow** | A signed DAG of steps. |
| **Step** | A contract template + input binding + output binding. |
| **Input binding** | Expression referencing prior step outputs or workflow inputs. |
| **Output binding** | Named handle exposing a step's deliverable to later steps. |
| **Workflow sponsor** | The principal/agent that signs and funds the workflow. |

### 3.2 Workflow manifest

```json
{
  "workflow_id": "urn:gap:wf:b7c2",
  "version": "0.1.0",
  "sponsor": "did:gap:9f2c…",
  "name": "content-pipeline",
  "inputs": { "topic": { "type": "string" } },
  "steps": [
    {
      "step_id": "scrape",
      "capability": "cap:data:scrape",
      "provider_policy": "any_verified",
      "inputs": { "query": "${workflow.topic}" },
      "outputs": { "raw": "steps.scrape.deliverable" },
      "terms": { "price": { "amount": 0.01, "model": "per-unit" },
                 "deadline_offset_secs": 3600,
                 "autonomy": "execute-notify" }
    },
    {
      "step_id": "analyze",
      "capability": "cap:analysis:summarize",
      "needs": ["scrape"],
      "inputs": { "data": "${steps.scrape.raw}" },
      "outputs": { "summary": "steps.analyze.deliverable" }
    },
    {
      "step_id": "publish",
      "capability": "cap:content:publish",
      "needs": ["analyze"],
      "inputs": { "content": "${steps.analyze.summary}" },
      "outputs": { "url": "steps.publish.deliverable" }
    }
  ],
  "budget": { "max_total": 100, "currency": "EUR" },
  "on_failure": "abort",   // abort | continue | compensate
  "expires_at": "2026-08-09T00:00:00Z",
  "sponsor_sig": "ed25519:…"
}
```

### 3.3 Semantics (normative)

1. **DAG ordering:** a step MAY start when all `needs` steps have
   reached `Delivered` and their outputs are verified.
2. **Provider selection:** the sponsor (or its orchestrating agent,
   via delegation RFC-0001) selects a provider per step via discovery
   (part 02), subject to `provider_policy` (e.g. min reputation).
3. **Binding evaluation:** `${expr}` bindings are evaluated against
   workflow inputs and prior step outputs; binding failure aborts.
4. **Budget:** the workflow carries a max total; the orchestrator MUST
   NOT sign step contracts whose sum exceeds it.
5. **Failure modes:**
   - `abort` — stop, mark workflow `Failed`, refund escrowed steps.
   - `continue` — skip failed step, proceed with remaining steps.
   - `compensate` — run declared compensation steps (RFC-0009
     irreversibility interplay).
6. **Idempotency:** each step executes under exactly one contract; a
   step MUST NOT be re-contracted after reaching `Accepted` unless the
   workflow declares `retryable: true`.

### 3.4 Workflow state machine

```
  pending ─► provisioning ─► running ─► completed
     │            │             │
     ▼            ▼             ▼
  cancelled    failed        failed (partial)
```

| State | Meaning |
|-------|---------|
| `pending` | created, not started |
| `provisioning` | provider selection in progress |
| `running` | one or more steps executing |
| `completed` | all steps accepted |
| `failed` | terminal failure per `on_failure` |
| `cancelled` | sponsor cancelled before start |

### 3.5 Message kinds

| Kind | Meaning |
|------|---------|
| `wf.create` | publish workflow manifest |
| `wf.start` | begin execution |
| `wf.progress` | step status update |
| `wf.complete` | workflow finished |
| `wf.fail` | workflow failed |
| `wf.cancel` | sponsor cancellation |

### 3.6 Conformance requirements

- Parse and validate workflow manifests (schema, DAG acyclicity,
  binding well-formedness).
- Enforce step ordering and budget.
- Evaluate bindings; abort on failure.
- Emit workflow lifecycle messages signed by the orchestrator.
- Interoperate with RFC-0001 (orchestrator acts via delegation).

## 4. Security & privacy considerations

- **Malicious bindings:** bindings are read-only expressions over
  declared inputs/outputs; no general code execution.
- **Provider swap:** a compromised orchestrator could select rogue
  providers; `provider_policy` and reputation gates mitigate.
- **Data flow:** step outputs flow between agents; compliance context
  (RFC-0006) applies to each hop.
- **Budget bombs:** max_total + per-step caps enforced by escrow.

## 5. Backward compatibility

Additive: new kinds, new module. The `Contract` type is unchanged;
workflow steps instantiate normal contracts. Existing deployments
ignore `wf.*` messages.

## 6. Reference implementation

New module `src/workflow.rs`:

- `Workflow { id, version, sponsor, inputs, steps, budget, on_failure,
  expires, sig }`
- `Step { id, capability, needs, inputs, outputs, terms }`
- `WorkflowEngine` — validates DAG (topological sort), evaluates
  bindings, tracks per-step state, enforces budget.
- `BindingError`, `CycleError`, `BudgetExceeded` errors.
- Tests: DAG validation (cycle detection), binding evaluation,
  budget enforcement, failure modes, state machine.

## 7. Review notes

- (pending)

## 8. Open questions

- Nested workflows (step = sub-workflow)? Proposal: yes, via
  `capability: "gap:workflow:<id>"` — v0.2 milestone.
- Parallel branches with shared budget allocation — proposal: per-branch
  budget slices declared by the sponsor.

## 9. References

- GAP spec parts 02 (discovery), 03 (contracts), 05 (payment).
- RFC-0001 (delegation), RFC-0009 (irreversibility).
- robpolak OpenAgentProtocol (YAML workflows), OAP RFC-0008.
