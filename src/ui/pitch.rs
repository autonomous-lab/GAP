//! The argument, as served by the node itself.
//!
//! This content used to live only on a separate static landing page.
//! Splitting it there meant the page that explains *why* GAP exists and
//! the node that proves it works were two different websites, and only
//! one of them had data. Here the argument sits directly above the
//! live evidence: the same page that claims settlement is verifiable
//! lists the settlements and links each verdict.
//!
//! Everything in this module is static prose, so it is `const` rather
//! than `format!`: these blocks are full of JSON braces and a format
//! string would need every one of them doubled.

/// Why the shape of the market changes.
pub const SHIFT: &str = r#"
<p class="lead">The old stack optimised attention: ads, forms, SaaS seats, dashboards, humans
approving flows. The next one optimises intent. Agents discover counterparties, price work, sign
scope, park funds, deliver artifacts and settle, without a person in the loop until something
actually goes wrong.</p>
<div class="grid three" style="margin-top:20px">
  <div class="card"><h3>Every agent needs a passport</h3>
    <p>A self-certifying <code>did:gap:</code> identity that belongs to the agent, not to a
    marketplace or a vendor account.</p></div>
  <div class="card"><h3>Every task needs a contract</h3>
    <p>Scope, deadline, price and state transitions, signed by both sides before execution
    starts.</p></div>
  <div class="card"><h3>Every settlement needs proof</h3>
    <p>Escrow, delivery digest, receipts and dispute paths as protocol primitives, not as
    application folklore.</p></div>
</div>
<div class="split" style="margin-top:16px">
  <div class="card"><h3 class="dim">B2B2C funnel</h3>
    <ul class="bul" style="margin-top:10px">
      <li>A brand's CRM owns the customer path.</li>
      <li>A SaaS platform mediates identity and spend.</li>
      <li>Human approval is the bottleneck in every loop.</li>
    </ul></div>
  <div class="card"><h3 class="cy">A2A market</h3>
    <ul class="bul" style="margin-top:10px">
      <li>A buyer agent delegates intent and budget.</li>
      <li>A provider agent prices, signs and executes.</li>
      <li>An arbiter settles disputes by rule.</li>
    </ul></div>
</div>
<p class="lead" style="margin-top:16px">GAP does not replace communication protocols. It makes
commercial outcomes portable across them.</p>
"#;

/// What is actually missing today.
pub const PROBLEM: &str = r#"
<p class="lead">MCP connects an agent to tools. A2A lets agents exchange messages. But the moment
two agents from <em>different organisations</em> need to do business (agree on work, hold funds,
prove what happened), the stack loses its memory. Three things are missing, and they are the three
things commerce is made of.</p>
<div class="grid three" style="margin-top:20px">
  <div class="card"><h3 class="bad">No identity</h3>
    <p>An agent is a row in someone's database. It cannot prove who it is to a stranger, cannot
    carry a reputation, and its name can be taken away by whoever runs the registry.</p></div>
  <div class="card"><h3 class="bad">No contracts</h3>
    <p>A JSON message is not an agreement. Nothing is signed, nothing is binding, and there is no
    state machine that says who owes what to whom, as of when.</p></div>
  <div class="card"><h3 class="bad">No settlement</h3>
    <p>No escrow, no dispute path, no receipt both sides can verify later. Payment between agents
    today means an API key with a credit card behind it, and hope.</p></div>
</div>
"#;

/// Positioning against the protocols people already know.
pub const COMPARE: &str = r#"
<p class="lead">MCP is how an agent uses tools. A2A is how agents exchange messages. GAP is how
agents do <em>business</em>: the layer where identity, money and accountability live. An agent can
speak all three; only GAP makes the outcome settleable.</p>
<div class="tablewrap" style="margin-top:18px"><table class="cmp">
<tr><th></th><th>MCP</th><th>A2A</th><th class="hl">GAP</th></tr>
<tr><td>Solves</td><td>agent to tools</td><td>agent to agent messaging</td>
  <td class="hl">agent to agent commerce</td></tr>
<tr><td>Identity</td><td>host-scoped</td><td>vendor or platform</td>
  <td class="hl">self-certifying DID, portable</td></tr>
<tr><td>Agreements</td><td class="no">no</td><td class="no">no</td>
  <td class="hl">signed contract state machine</td></tr>
<tr><td>Payments</td><td class="no">no</td><td class="no">no</td>
  <td class="hl">escrow, settlement, disputes</td></tr>
<tr><td>Accountability</td><td class="no">no</td><td>task artifacts</td>
  <td class="hl">hash-chained audit spine</td></tr>
<tr><td>Delegation limits</td><td class="no">no</td><td class="no">no</td>
  <td class="hl">mandates with budgets and depth</td></tr>
</table></div>
"#;

/// The audit, published rather than promised.
pub const SECURITY: &str = r#"
<p class="lead">Most protocols ship a roadmap. GAP ships its audit: findings, fixes and regression
tests, in the repository, where you can check them.</p>
<div class="grid two" style="margin-top:20px">
  <div class="card"><h3>Audited, in public</h3>
    <p>The reference node went through a security audit published as <code>SECURITY-AUDIT.md</code>:
    19 findings, including 2 criticals, guessable sequential session tokens and a SQL injection in
    the ClickHouse layer. Every actionable finding is fixed with a regression test. External review
    is welcome; the surface is small on purpose.</p></div>
  <div class="card"><h3>Escrow with no admin key</h3>
    <p>Funds are held by <code>GapEscrow.sol</code>, not by any node or company. Park, release,
    refund, dispute and rule, with checks-effects-interactions and a per-contract arbiter. The node
    signs and relays transactions; it is never custodian. If the operator disappears, settlements
    still work.</p></div>
  <div class="card"><h3>Tamper-evident history</h3>
    <p>Every receipt is hash-chained (RFC-0003). Redaction, because GDPR is real, re-links and
    re-signs the chain and is itself an auditable event. Integrity and the right to erasure stop
    fighting each other.</p></div>
  <div class="card"><h3>Crypto, done boring</h3>
    <p>Ed25519 with strict verification, 256-bit CSPRNG bearer tokens, 128-bit identifiers, a
    persisted node identity. No floating-point money: amounts are exact minor units at stablecoin
    scale, end to end.</p></div>
</div>
"#;

/// Numbers, including the bad one.
pub const BENCHMARKS: &str = r#"
<p class="lead">The first load test was a disaster, and it is documented on purpose: 41 requests
per second at 16 concurrent clients. Profiling found two accidental O(n squared) hot paths, a
<code>MAX(seq)</code> scan on every audit insert and a full agent-table scan on every proposal,
which was a free denial-of-service vector. The full archaeology is in <code>BENCHMARK.md</code>.</p>

<div class="card" style="margin-top:20px">
  <h3>POST /v1/contract/propose, throughput at 16 concurrent clients</h3>
  <div class="bars">
    <div class="bar-row"><span class="bl">Campaign 1<br><i>naive implementation</i></span>
      <span class="bt"><i style="width:0.3%"></i></span><b>41 req/s</b></div>
    <div class="bar-row"><span class="bl">Campaign 2<br><i>both hot paths fixed</i></span>
      <span class="bt"><i style="width:100%"></i></span><b>15,294 req/s</b></div>
    <div class="bar-row"><span class="bl">Shipped<br><i>worker pool, p50 0.78 ms</i></span>
      <span class="bt"><i style="width:94%"></i></span><b>14,407 req/s</b></div>
  </div>
  <p class="dim" style="font-size:.85rem;margin-top:12px">AMD EPYC, 16 cores, release build, linear
  scale. The first bar really is that small: 373 times on the worst case. That is what benchmarks
  are for.</p>
</div>

<div class="tablewrap" style="margin-top:16px"><table>
<tr><th>Endpoint</th><th>Concurrency</th><th>Throughput</th><th>p50</th></tr>
<tr><td class="mono">POST /v1/contract/propose</td><td>1</td><td><b>10,972 req/s</b></td><td>0.08 ms</td></tr>
<tr><td class="mono">POST /v1/contract/propose</td><td>16</td><td><b>14,407 req/s</b></td><td>0.78 ms</td></tr>
<tr><td class="mono">GET /health</td><td>16</td><td><b>18,724 req/s</b></td><td>0.20 ms</td></tr>
<tr><td class="mono">POST /v1/identity</td><td>16</td><td><b>17,402 req/s</b></td><td>0.45 ms</td></tr>
<tr><td class="mono">Ed25519 sign / verify</td><td>n/a</td><td><b>14.0 / 40.5 us</b></td><td>n/a</td></tr>
<tr><td class="mono">Audit spine append (SQLite)</td><td>n/a</td><td><b>229,000 ops/s</b></td><td>n/a</td></tr>
</table></div>
<p class="dim" style="margin-top:12px;font-size:.87rem">Methodology, environment and reproduction
steps are in the report. These numbers are a floor, not a marketing ceiling.</p>
"#;

/// How the node is built.
pub const ARCHITECTURE: &str = r#"
<p class="lead">About 30 Rust modules. Everything that happens is an event on the spine; state is
materialised from it. SQLite for development, ClickHouse for production, and one conformance suite
both backends must pass identically. The future should be exotic in outcome, not in failure
modes.</p>
<div class="grid two" style="margin-top:20px">
  <div class="card"><h3>Storage spine</h3>
    <p>Event sourcing behind a <code>Storage</code> trait. ClickHouse in production with escrow
    writes serialised through a sequencer: atomicity without pretending an OLAP store is OLTP. A
    cross-backend conformance suite keeps SQLite and ClickHouse honest.</p></div>
  <div class="card"><h3>Horizontal scale</h3>
    <p>Stateless nodes behind HAProxy: <code>docker-compose.scale.yml</code> ships a load balancer,
    three nodes and ClickHouse. Multi-stage musl image, health checks, one command up.</p></div>
  <div class="card"><h3>Fast where it counts</h3>
    <p>Worker-pool HTTP server, JSON parsing outside the critical section, O(1) hot paths through
    indexed lookups and in-memory sequence counters. Signature verification is the honest
    bottleneck, as it should be.</p></div>
  <div class="card"><h3>Made to interoperate</h3>
    <p>AgentCard at <code>/.well-known/gap-agent.json</code>, with an MCP adapter and single-file
    SDKs. Conformance levels (RFC-0011) define what "speaks GAP" actually means, with no
    vibes-based compatibility.</p></div>
</div>
"#;

/// The paper trail.
pub const SPECS: &str = r#"
<p class="lead">Seven normative spec parts plus fifteen RFCs implemented in the reference node,
plus a published conformance matrix that says exactly what is not implemented. Known-answer test
vectors lock the wire format byte for byte, and the tokenomics part is labelled what it is: design
intent.</p>
<div style="margin:18px 0">
  <span class="pill">00 Overview</span><span class="pill">01 Identity</span>
  <span class="pill">02 Discovery</span><span class="pill">03 Contracts</span>
  <span class="pill">04 Execution</span><span class="pill">05 Payment</span>
  <span class="pill">06 Governance</span><span class="pill">07 Tokenomics (informative)</span>
  <span class="pill g">Test vectors</span>
</div>
<!-- A dense list, not fifteen near-identical cards. Fifteen cards is a
     repository table of contents wearing a landing page's clothes: it
     costs 1,500px of scroll and tells a reader nothing a list does not. -->
<div class="rfcs">
  <div><b>RFC-0001</b> Delegation <i>mandates with budgets, escalation depth and revocation, so an
    agent can hire without a blank cheque</i></div>
  <div><b>RFC-0002</b> Workflows <i>DAG composition across agents, with contracts at every edge</i></div>
  <div><b>RFC-0003</b> Receipt chain <i>hash-linked, anchorable, tamper-evident</i></div>
  <div><b>RFC-0004</b> Policy engine <i>layered rules with explainable decision records</i></div>
  <div><b>RFC-0005</b> Credentials <i>verifiable claims: projection, revocation, compliance</i></div>
  <div><b>RFC-0006</b> Compliance <i>embargoes, Chinese walls and NDAs as protocol objects</i></div>
  <div><b>RFC-0007</b> Sybil resistance <i>delegation-tree aggregation, one bid per tree</i></div>
  <div><b>RFC-0008</b> Subscriptions <i>consent, renewal and budget caps for recurring work</i></div>
  <div><b>RFC-0009</b> Cooling-off <i>irreversibility windows on settlements</i></div>
  <div><b>RFC-0010</b> Discovery <i>AgentCard at /.well-known/, so no registry monopoly</i></div>
  <div><b>RFC-0011</b> Conformance <i>levels that define what "speaks GAP" means</i></div>
  <div><b>RFC-0012</b> SLAs <i>incident classification and divergence reporting</i></div>
  <div><b>RFC-0013</b> Event delivery <i>signed webhooks and a resumable stream, so agents stop
    polling and every push is verifiable</i></div>
  <div><b>RFC-0014</b> Verified delivery <i>integrity first, a judge second, and the judge can
    never overrule the maths</i></div>
  <div><b>RFC-0015</b> Escalation <i>only judge disagreement summons a human; one rework attempt,
    disputes priced by win rate rather than volume</i></div>
</div>
"#;

/// The objections, asked before the reader has to.
pub const FAQ: &str = r#"
<p class="lead">These are the pushbacks we would raise ourselves. Short answers here, long answers
in the repository.</p>
<details class="faq"><summary>Yet another agent protocol?</summary>
<div><p>The existing ones solve communication. MCP is agent to tools; A2A is agent to agent
messages. Neither answers how two agents from different companies agree on work, hold funds and
prove what happened. That transactional layer is what GAP specifies, and unlike most protocol
announcements it comes with a working node, a published audit and reproducible benchmarks rather
than a README and a waitlist.</p></div></details>
<details class="faq"><summary>Is this a crypto project?</summary>
<div><p>No token to pump, and no tokenomics-as-business-model. Settlement is denominated in
stablecoin units; the on-chain escrow is <em>optional</em> and exists for one reason: holding funds
without trusting anyone, including us. No admin key, no upgrade path to a rug pull. The off-chain
reference escrow works with no blockchain at all.</p></div></details>
<details class="faq"><summary>Why ClickHouse for something that looks like OLTP?</summary>
<div><p>Because the spine is not OLTP: it is an append-only event log, which is exactly what
ClickHouse is built for. The genuinely transactional part, escrow, is serialised through a
single-writer sequencer with post-write verification, and materialised state uses
ReplacingMergeTree. SQLite remains the development and test backend, and a conformance suite forces
both to behave identically.</p></div></details>
<details class="faq"><summary>Is it production-ready?</summary>
<div><p>It is v0.1 and says so. Honest state: benchmarks are single-node, chain backends are mocked
in CI, and the conformance matrix names what is not implemented rather than hiding it. What is
here works and is testable, which is a different claim from "ready for your money at
scale".</p></div></details>
<details class="faq"><summary>What stops an agent from lying about its work?</summary>
<div><p>The digest. The bytes the buyer receives must hash to what the provider committed to before
any judge is consulted, and no model can overrule that. Beyond integrity, the agreed acceptance
criteria go to independent judges that cannot see each other's answers; if they disagree, escrow
does not move and a human decides.</p></div></details>
<details class="faq"><summary>Who is behind this, and what happens if they disappear?</summary>
<div><p>The specification and the reference node are open. If this operator vanished tomorrow,
running nodes and parked funds would keep working, because identities are self-certifying and
on-chain escrow has no admin key. That is not altruism: a commerce protocol nobody else can trust
is worthless.</p></div></details>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_em_dashes_reach_the_reader() {
        // House style: an em dash reads as machine-generated filler.
        // The punctuation each sentence actually needs was chosen
        // instead, and a stray one creeping back in is a regression.
        for (name, block) in [
            ("SHIFT", SHIFT),
            ("PROBLEM", PROBLEM),
            ("COMPARE", COMPARE),
            ("SECURITY", SECURITY),
            ("BENCHMARKS", BENCHMARKS),
            ("ARCHITECTURE", ARCHITECTURE),
            ("SPECS", SPECS),
            ("FAQ", FAQ),
        ] {
            assert!(!block.contains('\u{2014}'), "em dash in {name}");
            assert!(!block.contains('\u{2013}'), "en dash in {name}");
        }
    }

    #[test]
    fn every_block_is_balanced_html() {
        // These are pasted into a page wholesale; one unclosed div
        // silently swallows everything rendered after it.
        for (name, block) in [
            ("SHIFT", SHIFT),
            ("PROBLEM", PROBLEM),
            ("COMPARE", COMPARE),
            ("SECURITY", SECURITY),
            ("BENCHMARKS", BENCHMARKS),
            ("ARCHITECTURE", ARCHITECTURE),
            ("SPECS", SPECS),
            ("FAQ", FAQ),
        ] {
            for tag in ["div", "table", "details", "p", "ul"] {
                let open = block.matches(&format!("<{tag}")).count();
                let close = block.matches(&format!("</{tag}>")).count();
                assert_eq!(open, close, "unbalanced <{tag}> in {name}");
            }
        }
    }

    #[test]
    fn the_bad_benchmark_is_still_published() {
        // The 41 req/s failure is the most credible number on the page.
        // Quietly dropping it would turn a track record into a brochure.
        assert!(BENCHMARKS.contains("41 req/s"));
        assert!(BENCHMARKS.contains("disaster"));
    }
}
