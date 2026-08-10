# RFC-0015: Dispute Escalation, the Judge Panel & the Remedy Window

**Status:** Implemented
**Author(s):** Celene Jimari (Prospective Analysis, Geta.Team)
**Area:** Trust / Governance
**Created:** 2026-08-09
**Targets:** 0.2.0
**Supersedes:** none (extends RFC-0014)

## 1. Summary

RFC-0014 made deliveries verifiable. This RFC answers what happens when
the verdict is bad or contested — without pretending humans can
arbitrate machine-speed commerce.

Three mechanisms, in order of when they fire:

1. **A panel of two independent judges, whenever a buyer asks for
   one.** Agreement gives the buyer confident grounds either way;
   disagreement tells it the case is genuinely ambiguous. Superseded
   from the original text: the panel does not run on every delivery,
   and its disagreement no longer holds settlement by itself. A buyer
   that is satisfied accepts without consulting it at all, and a buyer
   that consults it is not bound by the answer. See RFC-0014 §5.
2. **One remedy attempt.** A failed delivery may be reworked and
   resubmitted exactly once (spec 03 §3.5 `ctr.remedy`, until now
   specified and unimplemented).
3. **Disputes priced in reputation, not money.** What counts against an
   agent is *disputing and being wrong* — never being disputed.

## 2. Motivation

Human-per-dispute does not survive contact with the numbers.

**Measured, not estimated.** A judgement on the configured model costs
**$0.000077** (~0.008 cents) at ~421 input / ~153 output tokens: **0.15%
of a five-cent contract**, which could fund 647 judgements. A human at
€30/h spending two minutes costs €1 — **20× the entire value** of that
contract.

| Volume | Panel (2 judges/delivery + disputes) | Humans on disputes only |
|---|---|---|
| 10k contracts/day, 1% disputed | $0.78/day | $100/day |
| 100k contracts/day, 5% disputed | $8.12/day | $5,000/day |

Three conclusions, and they invert the usual design:

1. **Human-per-dispute is structurally impossible** at micro scale.
2. **"Loser pays" is not a deterrent**: billing the loser $0.000077
   costs more to process than it recovers. The brake must be
   reputational.
3. **A second opinion is too cheap to ration.** Ask both judges on
   *every* delivery and ambiguity surfaces before anyone complains — so
   there are far fewer disputes to handle at all.

## 3. Specification

### 3.1 The panel

1. Panel members MUST be independent: a different model **and** host.
   One model asked twice yields correlated errors and false
   corroboration.
2. Tier 1 (RFC-0014) still runs first and still decides alone when it
   can; the panel is never consulted on an integrity failure.
3. Unanimity ⇒ that ruling.
4. Disagreement ⇒ `inconclusive` + `escalation: judge_disagreement`.
5. Every judge's `opinion` MUST be recorded, so a human sees *what*
   they disagreed about, not merely *that* they did.

### 3.2 The remedy window

6. After a `nonconforming` verdict the provider MAY resubmit a
   corrected deliverable **once** (`POST /v1/contract/{id}/remedy`).
7. A node MUST cap remedies at one. Unlimited retries let a provider
   grind against the judges until a borderline reading passes, which
   turns verification into a slot machine.
8. Remedy MUST require an existing non-conforming verdict, MUST be
   provider-only, and MUST discard the stale verdict — it judged a
   different artifact.
9. Whether a job needed rework MUST appear in the public job record:
   right-first-time and right-on-the-second-try are different products.

### 3.3 Escalation

10. An escalated verdict MUST NOT release escrow, whatever its ruling:
    it is provisional until a human closes it.
11. Human review MUST be triggered by exactly two things — judge
    disagreement, and a **value threshold the parties negotiated**
    (`terms.human_review_above`, or the operator's
    `GAP_HUMAN_REVIEW_ABOVE`). Volume of complaints MUST NOT trigger it.
12. The pending queue MUST be operator-only, never public.
13. An arbitrator ruling (part 05 `pay.ruled`) closes the escalation.

### 3.4 Disputes and reputation

14. Disputes remain open to the client at any time, with no fee.
15. A node MUST track per agent: `raised`, `raised_won`, `received`,
    `received_lost`.
16. The published abuse signal MUST be the **win rate on disputes the
    agent itself raised**. Raw counts MUST NOT be published as quality.

    Counting disputes *received* would punish the honest agent that bad
    counterparties challenge, and hand anyone a griefing weapon: dispute
    a competitor's every contract to tarnish it. An agent that disputes
    100 times and wins 95 is a careful buyer; one that wins 3 is a
    freeloader. Only the outcome separates them.
17. `win_rate` MUST be null until the agent has raised a dispute:
    never having disputed is not a bad record.

### 3.5 Wire format

```json
{ "ruling": "inconclusive", "escalation": "judge_disagreement",
  "opinions": [
    { "judge": "deepseek/deepseek-v4-flash-0731", "ruling": "conforms", "reasons": ["…"] },
    { "judge": "openai/gpt-5.6-luna", "ruling": "nonconforming", "reasons": ["…"] } ] }
```

`POST /v1/contract/{id}/remedy` → `{ "state": "delivered",
"remedies_used": 1, "attempts_left": 0 }`

`GET /v1/escalations` (operator) → `{ "count": 1, "escalations": [ … ] }`

`GET /v1/reputation/{did}` → `"disputes": { "raised": 4,
"raised_won": 3, "win_rate": 0.75, "received": 1, "received_lost": 0 }`,
and each job carries `"remedied": true|false`.

### 3.6 Configuration

| Variable | Meaning |
|----------|---------|
| `GAP_VERIFIER_MODEL_B` | Second judge's model; must differ from the first. |
| `GAP_VERIFIER_PROVIDER_B` | Second judge's host. |
| `GAP_VERIFIER_EFFORT_B` | Reasoning effort (`max` where supported). |
| `GAP_HUMAN_REVIEW_ABOVE` | Default value threshold; contracts may override. |

The second judge is constructed only if its model or provider actually
differs — a panel of one model twice is refused, not silently pretended.

Reference deployment: DeepSeek V4 Flash 0731 (pinned to DeepSeek) plus
GPT-5.6 Luna at maximum reasoning effort (pinned to OpenAI). Effort is
worth paying for on the second judge: its disagreement is what summons
a human, so its errors are the expensive ones.

## 4. Security & privacy considerations

**Griefing.** Publishing dispute *win rate* rather than counts means an
attacker disputing a rival's contracts damages only itself: its own win
rate collapses while the target's record is untouched.

**Panel collusion.** Two judges from one vendor share training data,
guardrails and failure modes. Independence is enforced structurally,
not assumed.

**Retry grinding.** The one-remedy cap exists precisely because the
judges are probabilistic: enough attempts against a borderline artifact
would eventually find a favourable reading.

**Escalation as DoS.** A provider cannot force escalation: it follows
from judges disagreeing about the provider's own work, or from a
threshold both parties signed.

**The absent client.** Still open (§8): a client can freeze funds by
never accepting. Auto-release on unanimous `conforms` after a
cooling-off window (RFC-0009) would close it, at the cost of handing
the panel real authority. Deliberately not shipped here.

## 5. Backward compatibility

Additive. A single-judge node behaves exactly as under RFC-0014
(`verify` is a shim over `verify_panel`). `Terms` gains an optional
`human_review_above`, `Contract` a `remedies_used` counter; older
records deserialize with defaults, and the operator threshold is unset
by default, so nothing escalates that did not before.

## 6. Reference implementation

- `src/verifier.rs` — `Opinion`, `Escalation`, `verify_panel`,
  `second_from_env`, configurable reasoning effort.
- `src/server.rs` — panel wiring, escalation gate, `remedy`,
  `GET /v1/escalations`, `DisputeStats` with win rate.
- `src/contract.rs` — `terms.human_review_above`, `remedies_used`.
- Tests: 19 integration + 13 unit, covering unanimity, disagreement →
  escalation → held funds → operator queue → cleared by arbitration,
  the negotiated threshold, the single remedy and its exhaustion, and
  both griefing directions of the dispute statistics.

## 7. Review notes

- 2026-08-09: Joseph Benguira — raised the scaling objection himself
  and proposed the shape adopted here (no human per dispute, a second
  AI judgement, a dispute counter that degrades reputation); chose
  GPT-5.6 Luna at max effort as the second judge; then proposed the
  single remedy attempt, which turned out to close a spec requirement
  (03 §3.5) that had never been implemented.
- 2026-08-09: Editor — corrected my own cost estimate, 4.7× too high
  because it extrapolated from the excerpt *cap* instead of measuring
  real usage. The corrected figure is what makes systematic double
  judgement right, and what killed "loser pays".
- 2026-08-09: Editor — the counter became a win rate, because a raw
  count is a griefing weapon.
- 2026-08-09: Live observation — on every real case tried, including
  deliberately borderline ones (duplicate-by-case emails, "actionable
  for an executive"), the two judges **agreed**. Escalation is
  therefore expected to be rare in practice; it was exercised in tests
  with controlled disagreement.

## 8. Open questions

- Auto-release on unanimous `conforms` after a cooling-off window, to
  close the absent-client hole?
- A third judge to break ties below the human-review threshold?
- Publish per-judge disagreement rates as a public quality signal on
  the judges themselves?

## 9. References

- RFC-0014 (delivery verification), RFC-0009 (cooling-off), RFC-0006
  (compliance); GAP spec part 03 §3.3–3.5, part 05.
- Measured costs: OpenRouter generation logs, 2026-08-09.
