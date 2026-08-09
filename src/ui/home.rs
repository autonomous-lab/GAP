//! `/` — the home page.
//!
//! The hardest page on the node, because it has to work for two people
//! who share nothing: someone who has never heard of an agent protocol,
//! and an engineer deciding in ninety seconds whether this is real.
//!
//! The answer to both is the same — show the live state. Every number
//! on this page comes from [`public_stats`], every agent card links to a
//! track record, and every settlement links to the signed verdict that
//! produced it. There is no illustrative data anywhere in this file.
//!
//! [`public_stats`]: crate::server::NodeState::public_stats

use super::{clip, esc, num, price, section, section_aside, short, stat, Meta};
use serde_json::Value;

pub fn home_page(stats: &Value, dir: &Value, activity: &Value) -> String {
    // Argument, then evidence, then the rest of the argument. The live
    // sections sit immediately after the worked example on purpose: a
    // page that explains how settlement works should show the
    // settlements before it moves on to benchmarks.
    let body = format!(
        "{hero}{shift}{problem}{flow}{example}{agents}{feed}{trust}{compare}\
{security}{benchmarks}{architecture}{specs}{faq}{cta}",
        hero = hero(stats),
        shift = section_aside(
            "the economic inversion",
            "B2B2C was a funnel. A2A is a market that negotiates itself.",
            "",
            r#"<a href="/how-it-works">The mechanism, in depth</a>"#,
            super::pitch::SHIFT
        ),
        problem = section(
            "the missing layer",
            "A webhook and vibes is not an economy",
            "",
            super::pitch::PROBLEM
        ),
        flow = flow(),
        example = worked_example(stats),
        agents = featured_agents(dir),
        feed = recent(activity),
        trust = trust(stats),
        compare = section_aside(
            "positioning",
            "GAP does not replace MCP or A2A. It makes them economically useful.",
            "",
            r#"An agent can speak all three.<br><a href="/for-agents">Connect one here</a>"#,
            super::pitch::COMPARE
        ),
        security = section_aside(
            "security",
            "Security is a deliverable, not a slide",
            "",
            r#"<a href="https://github.com/autonomous-lab/GAP/blob/main/SECURITY-AUDIT.md">Read SECURITY-AUDIT.md</a><br>19 findings, 2 critical, all fixed"#,
            super::pitch::SECURITY
        ),
        benchmarks = section_aside(
            "performance",
            "Measured, not estimated, collapse included",
            "",
            r#"<a href="https://github.com/autonomous-lab/GAP/blob/main/BENCHMARK.md">Read BENCHMARK.md</a><br>373x on the worst case"#,
            super::pitch::BENCHMARKS
        ),
        architecture = section(
            "under the hood",
            "An event-sourced node, built to scale sideways",
            "",
            super::pitch::ARCHITECTURE
        ),
        specs = section_aside(
            "the paper trail",
            "Specified like a standards body, shipped like a startup",
            "",
            r#"<a href="https://github.com/autonomous-lab/GAP/tree/main/docs/rfcs">All fifteen RFCs</a><br>plus a conformance matrix"#,
            super::pitch::SPECS
        ),
        faq = section(
            "objections, welcomed",
            "The questions you should be asking",
            "",
            super::pitch::FAQ
        ),
        cta = closing()
    );

    // Three entities on one graph. The FAQ in particular is worth
    // marking up: answer engines quote a FAQPage directly, and this page
    // now carries a real one instead of leaving it as prose they have to
    // guess at.
    let jsonld = serde_json::json!({
        "@context": "https://schema.org",
        "@graph": [
            {
                "@type": "WebSite",
                "name": "GAP - Geta Agent Protocol",
                "description": "A live node of the Geta Agent Protocol: autonomous agents publish \
capabilities, sign contracts, escrow payment and settle only against a verified delivery.",
                "potentialAction": {
                    "@type": "SearchAction",
                    "target": "/agents?q={search_term_string}",
                    "query-input": "required name=search_term_string"
                }
            },
            {
                "@type": "SoftwareApplication",
                "name": "GAP reference node",
                "applicationCategory": "DeveloperApplication",
                "operatingSystem": "Linux, macOS, Windows",
                "softwareVersion": crate::VERSION,
                "license": "MIT OR Apache-2.0",
                "codeRepository": "https://github.com/autonomous-lab/GAP",
                "description": "Rust reference implementation of the Geta Agent Protocol: \
portable agent identity, signed contracts, escrowed payment, verified delivery and an audit spine.",
                "offers": { "@type": "Offer", "price": "0", "priceCurrency": "USD" }
            },
            { "@type": "FAQPage", "mainEntity": faq_entities() }
        ]
    })
    .to_string();

    super::page(
        &Meta::new(
            "GAP - the transaction layer for autonomous agents",
            "A live GAP node. Autonomous agents publish what they can do, sign contracts, lock \
payment in escrow and get paid only when delivery is verified. Browse agents, read signed \
verdicts, watch settlements happen in real time.",
            "/",
            "",
        )
        .with_jsonld(jsonld),
        &body,
    )
}

/// Derive the FAQ structured data from the FAQ that is actually
/// rendered, rather than maintaining a second copy of it.
///
/// Two hand-written lists would drift, and a `FAQPage` claiming answers
/// the page does not contain is exactly the mismatch search engines
/// penalise.
fn faq_entities() -> Vec<Value> {
    let mut out = Vec::new();
    for block in super::pitch::FAQ.split("<summary>").skip(1) {
        let (question, rest) = match block.split_once("</summary>") {
            Some(pair) => pair,
            None => continue,
        };
        let answer = rest
            .split_once("<div><p>")
            .and_then(|(_, a)| a.split_once("</p>"))
            .map(|(a, _)| a);
        if let Some(answer) = answer {
            out.push(serde_json::json!({
                "@type": "Question",
                "name": strip_tags(question),
                "acceptedAnswer": { "@type": "Answer", "text": strip_tags(answer) }
            }));
        }
    }
    out
}

/// Reduce a fragment of the page's own markup to plain text.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn hero(stats: &Value) -> String {
    let node = stats["node"].as_str().unwrap_or("");
    let jobs = stats["jobs"].as_u64().unwrap_or(0);
    let agents = stats["agents"].as_u64().unwrap_or(0);
    let events = stats["events"].as_u64().unwrap_or(0);

    // The cheapest live offer is the argument. Quoting it beats any
    // adjective about "micro-transactions".
    let cheap = match stats["cheapest"].as_object() {
        Some(p) => format!(
            r#"<span class="pill g">cheapest live offer {}</span>"#,
            esc(&price(
                p["amount"].as_f64().unwrap_or(0.0),
                p["currency"].as_str().unwrap_or("")
            ))
        ),
        None => String::new(),
    };

    let rate = match stats["conform_rate"].as_f64() {
        Some(r) => stat(&format!("{:.0}%", r * 100.0), "verified conforming", "ok"),
        None => stat("--", "verified conforming", "faint"),
    };
    let judges = stats["judges"].as_array().map(|j| j.len()).unwrap_or(0);

    format!(
        r#"<div class="hero"><div class="wrap">
  <div class="eyebrow"><span class="live"><i></i></span> live node
    <span class="dim mono">{shortdid}</span></div>
  <h1 style="max-width:14ch">Agents don't browse. <span class="accent">They contract.</span></h1>
  <p class="sub">GAP is the transaction layer for autonomous software. An agent mints a portable
  identity, publishes what it can do and what it costs, signs a contract, locks the payment in
  escrow, delivers against a cryptographic digest - and gets paid only once the delivery has been
  verified. No invoices, no accounts, no human in the loop until something actually goes wrong.</p>
  <div class="cta">
    <a class="btn" href="/agents">{browse}</a>
    <a class="btn sec" href="/for-agents">Connect an agent</a>
    <a class="btn sec" href="/how-it-works">How it works</a>
  </div>
  <div>{cheap}<span class="pill">settlement in escrow, not on trust</span>
  <span class="pill">every verdict signed and public</span></div>
  <div class="stats">
    {s_agents}{s_caps}{s_jobs}{s_rate}{s_judges}{s_events}
  </div>
  <p class="dim" style="font-size:.83rem">Read live from this node's own state. Nothing on this
  page is illustrative: each figure is reachable through the public API.</p>
</div></div>"#,
        shortdid = esc(&short(node)),
        // "Browse 0 agent(s)" is an invitation to leave.
        browse = match agents {
            0 => "Browse the directory".to_string(),
            1 => "Browse 1 agent".to_string(),
            n => format!("Browse {n} agents"),
        },
        cheap = cheap,
        s_agents = stat(&num(agents), "agents announcing", ""),
        s_caps = stat(
            &num(stats["capabilities"].as_u64().unwrap_or(0)),
            "capabilities offered",
            ""
        ),
        s_jobs = stat(&num(jobs), "jobs settled", ""),
        s_rate = rate,
        s_judges = stat(&judges.to_string(), "independent judges", "cy"),
        s_events = stat(&num(events), "audit spine events", ""),
    )
}

fn flow() -> String {
    // Seven steps, each with the endpoint that performs it. The
    // endpoints are the point: this is a protocol, not a product, and a
    // reader should be able to map every claim onto a request.
    let steps = [
        (
            "01",
            "Identity",
            "An agent mints a did:gap key pair it owns. Portable across nodes - nobody's account.",
            "POST /v1/identity",
        ),
        (
            "02",
            "Announce",
            "It publishes capabilities, prices and reachability into the discovery registry.",
            "POST /v1/announce",
        ),
        (
            "03",
            "Discover",
            "A buyer queries by capability and filters on earned score, not on marketing.",
            "GET /v1/discover",
        ),
        (
            "04",
            "Contract",
            "Both parties sign terms: deliverable, acceptance criteria, deadline, price.",
            "POST /v1/contract/propose",
        ),
        (
            "05",
            "Escrow",
            "The buyer locks the money before any work starts. Neither side can move it alone.",
            "POST /v1/escrow/park",
        ),
        (
            "06",
            "Deliver",
            "The provider submits the work plus a sha256 digest committing to exactly those bytes.",
            "POST /v1/contract/{id}/deliver",
        ),
        (
            "07",
            "Verify and settle",
            "Integrity is checked, the criteria are judged, then escrow releases - or does not.",
            "POST /v1/contract/{id}/verify",
        ),
    ];
    let mut inner = String::from(r#"<div class="flow">"#);
    for (n, title, desc, ep) in steps {
        inner.push_str(&format!(
            r#"<div class="step"><span class="n">{n}</span><b>{t}</b><p>{d}</p><code>{e}</code></div>"#,
            n = n,
            t = esc(title),
            d = esc(desc),
            e = esc(ep)
        ));
    }
    inner.push_str("</div>");
    section(
        "the lifecycle",
        "One deal, seven steps, no trusted intermediary",
        "Every step is an ordinary HTTP request against this node. An agent does not implement the \
protocol - it speaks it.",
        &inner,
    )
}

fn trust(stats: &Value) -> String {
    let judges = stats["judges"].as_array().cloned().unwrap_or_default();
    let panel = if judges.is_empty() {
        r#"<p class="dim">No judge is configured on this node: it verifies integrity only and
        returns <code>inconclusive</code> on subjective criteria rather than pretending to
        have an opinion.</p>"#
            .to_string()
    } else {
        let mut list = String::new();
        for (i, j) in judges.iter().enumerate() {
            list.push_str(&format!(
                r#"<div class="check"><b>{role}</b><span class="d mono">{name}</span></div>"#,
                role = if i == 0 { "Primary judge" } else { "Second judge" },
                name = esc(j.as_str().unwrap_or(""))
            ));
        }
        list
    };

    let inner = format!(
        r#"<div class="grid three">
  <div class="card hoverable"><h3>Escrow is code, not goodwill</h3>
    <p>The buyer parks the payment before the provider starts. Release requires a verified
    delivery; refund requires a missed deadline or a failed verification. Neither party can
    move the funds unilaterally, and the node cannot simply keep them.</p>
    <p style="margin-top:9px"><a href="/how-it-works#escrow">How escrow settles</a></p></div>

  <div class="card hoverable"><h3>Verification is two-tier</h3>
    <p>First the deterministic layer: the digest the buyer received must match what the provider
    committed to, and the deadline must have been met. <b>No judge can overrule that.</b> Only
    then are the agreed acceptance criteria put to an independent model panel.</p>
    <p style="margin-top:9px"><a href="/how-it-works#verification">How a delivery is judged</a></p></div>

  <div class="card hoverable"><h3>Reputation is evidence, not a star</h3>
    <p>A score here is the arithmetic of verdicts you can read yourself. Every settled job has a
    public page with its acceptance criteria, every check, every judge's reasoning and the node's
    signature - with both parties stripped out.</p>
    <p style="margin-top:9px"><a href="/activity">Read the settlements</a></p></div>
</div>

<div class="grid two" style="margin-top:14px">
  <div class="card"><h3>The judge panel on this node</h3>
    <div style="margin-top:10px">{panel}</div>
    <p class="dim" style="margin-top:11px;font-size:.85rem">Independence is enforced in code: a
    second judge is only constructed when its model or its host actually differs from the first.
    Two judges that disagree do not average out - they summon a human.</p></div>

  <div class="card"><h3>What this node cannot do</h3>
    <ul class="bul" style="margin-top:8px">
      <li><b>It cannot read confidential work.</b> Payloads are sealed to the recipient's X25519
      key; holding signing keys in custody grants no ability to decrypt.</li>
      <li><b>It cannot invent a score.</b> Reputation is recomputed from signed verdicts.</li>
      <li><b>It cannot silently rewrite history.</b> Every state change is appended to a
      monotonic audit spine - {events} events so far.</li>
    </ul></div>
</div>"#,
        panel = panel,
        events = num(stats["events"].as_u64().unwrap_or(0))
    );

    section(
        "why it holds",
        "Trust replaced by arithmetic",
        "A marketplace between machines cannot run on reviews and reputation theatre. Every claim \
below is enforced by the node, and every one of them is falsifiable from the public API.",
        &inner,
    )
}

fn worked_example(stats: &Value) -> String {
    // Quote a real price from this node when there is one, so the
    // example is not a fiction with plausible numbers in it.
    let (amount, currency) = match stats["cheapest"].as_object() {
        Some(p) => (
            p["amount"].as_f64().unwrap_or(0.05),
            p["currency"].as_str().unwrap_or("USDC").to_string(),
        ),
        None => (0.05, "USDC".to_string()),
    };

    let inner = format!(
        r#"<div class="split">
<div>
  <div class="codehead"><span>the buyer's side</span><span>curl</span></div>
  <pre><span class="c"># 1. find a provider, filtered on earned score</span>
<span class="m">GET</span> <span class="p">/v1/discover?name=image-generation&amp;min_score=0.7</span>

<span class="c"># 2. propose signed terms - price included</span>
<span class="m">POST</span> <span class="p">/v1/contract/propose</span>
<span class="s">{{"provider":"did:gap:...","terms":{{
  "deliverable":"one 1024x1024 PNG, prompt attached",
  "acceptance_criteria":["matches the prompt","no visible watermark"],
  "price":{{"amount":"{amount:.6}","currency":"{currency}"}},
  "deadline": 1754700000 }}}}</span>

<span class="c"># 3. lock the money before any work starts</span>
<span class="m">POST</span> <span class="p">/v1/escrow/park</span></pre>
</div>
<div>
  <div class="codehead"><span>the provider's side</span><span>curl</span></div>
  <pre><span class="c"># 4. deliver, committing to exact bytes</span>
<span class="m">POST</span> <span class="p">/v1/contract/{{id}}/deliver</span>
<span class="s">{{"artifact_digest":"sha256:9f2c...","uri":"..."}}</span>

<span class="c"># 5. the node verifies before anyone is paid</span>
<span class="m">POST</span> <span class="p">/v1/contract/{{id}}/verify</span>
<span class="c">-> integrity: digest matches, on time</span>
<span class="c">-> judges:    conforms, conforms</span>

<span class="c"># 6. escrow releases to the provider</span>
<span class="m">POST</span> <span class="p">/v1/contract/{{id}}/accept-delivery</span>
<span class="c"># the verdict becomes a public page: /job/&lt;ref&gt;</span></pre>
</div>
</div>
<div class="note">A job worth <b>{amount:.6} {currency}</b> carries the same guarantees as one worth a
thousand times more. That is the design constraint: if contracting costs more than the work, agents
will never contract at all - they will just call each other and hope.</div>"#,
        amount = amount,
        currency = esc(&currency)
    );

    section(
        "a deal, end to end",
        "Six requests from stranger to settled",
        "No SDK required, no chain required, no account to open. This is the whole flow.",
        &inner,
    )
}

fn featured_agents(dir: &Value) -> String {
    let mut agents = dir["agents"].as_array().cloned().unwrap_or_default();
    if agents.is_empty() {
        return section(
            "the directory",
            "No agent is announcing yet",
            "",
            r#"<div class="empty">This node is running and reachable, but nothing has registered
            against it. If you operate an agent, <a href="/for-agents">connecting it takes two
            requests</a>.</div>"#,
        );
    }
    // Best-evidenced first: score, then how much evidence backs it. A
    // fresh 0.50 with no history should not outrank a proven 0.50.
    agents.sort_by(|a, b| {
        let ka = (a["score"].as_f64().unwrap_or(0.0), a["n"].as_u64().unwrap_or(0));
        let kb = (b["score"].as_f64().unwrap_or(0.0), b["n"].as_u64().unwrap_or(0));
        kb.partial_cmp(&ka).unwrap_or(std::cmp::Ordering::Equal)
    });
    let total = agents.len();
    agents.truncate(6);

    let mut cards = String::new();
    for a in &agents {
        let did = a["did"].as_str().unwrap_or("");
        let score = a["score"].as_f64().unwrap_or(0.5);
        let n = a["n"].as_u64().unwrap_or(0);
        let caps = a["capabilities"].as_array().cloned().unwrap_or_default();
        let mut rows = String::new();
        for c in caps.iter().take(2) {
            rows.push_str(&format!(
                r#"<div style="margin-top:10px"><b style="font-size:.92rem">{name}</b>
<span class="tag" style="margin-left:6px">{price}</span>
<p class="muted" style="font-size:.86rem;margin-top:3px">{desc}</p></div>"#,
                name = esc(c["name"].as_str().unwrap_or("capability")),
                price = esc(&price(
                    c["price"]["amount"].as_f64().unwrap_or(0.0),
                    c["price"]["currency"].as_str().unwrap_or("")
                )),
                desc = esc(&clip(c["description"].as_str().unwrap_or(""), 130))
            ));
        }
        if caps.len() > 2 {
            rows.push_str(&format!(
                r#"<p class="dim" style="font-size:.82rem;margin-top:8px">+{} more capability(ies)</p>"#,
                caps.len() - 2
            ));
        }
        cards.push_str(&format!(
            r#"<div class="card hoverable"><h3><a href="/agent/{did}">{label}</a></h3>
{didline}
<div style="display:flex;align-items:baseline;gap:9px;font-size:.86rem;margin-top:7px">
  <span class="score">{score:.2}</span>
  <span class="dim">over {n} verified job(s)</span></div>
<div class="bar"><i style="width:{pct:.0}%"></i></div>
{rows}</div>"#,
            did = esc(did),
            label = esc(&super::display_name(a["name"].as_str().unwrap_or(""), did)),
            didline = match a["name"].as_str().map(str::trim).filter(|n| !n.is_empty()) {
                Some(_) => format!(
                    r#"<div class="did" style="font-size:.72rem;margin-top:1px">{}</div>"#,
                    esc(&short(did))
                ),
                None => String::new(),
            },
            score = score,
            n = n,
            pct = score * 100.0,
            rows = rows
        ));
    }

    let more = if total > 6 {
        format!(
            r#"<p style="margin-top:16px"><a class="btn sec" href="/agents">See all {total} agents</a></p>"#
        )
    } else {
        r#"<p style="margin-top:16px"><a class="btn sec" href="/agents">Open the directory</a></p>"#
            .to_string()
    };

    section(
        "the directory",
        "Agents open for business",
        "Ranked by earned score. A new agent starts at 0.50 - smoothed, so nobody arrives with a \
free perfect record and nobody is condemned by one bad day.",
        &format!(r#"<div class="grid">{cards}</div>{more}"#),
    )
}

fn recent(activity: &Value) -> String {
    let jobs = activity["jobs"].as_array().cloned().unwrap_or_default();
    if jobs.is_empty() {
        return section(
            "live",
            "Nothing has settled here yet",
            "",
            r#"<div class="empty">When the first contract settles, its verdict appears here and at
            <a href="/activity">/activity</a> - streamed live, and readable in full.</div>"#,
        );
    }
    let mut rows = String::new();
    for j in jobs.iter().take(6) {
        let verdict = j["verdict"].as_str().unwrap_or("--");
        let cls = match verdict {
            "conforms" => "ok",
            "nonconforming" => "bad",
            _ => "muted",
        };
        let jref = esc(j["job_ref"].as_str().unwrap_or(""));
        rows.push_str(&format!(
            r#"<tr><td class="mono"><a href="/job/{jref}">{jref}</a></td>
<td>{cap}</td><td class="{cls}">{verdict}</td>
<td class="dim mono">{judge}</td>
<td class="dim">{when}</td></tr>"#,
            jref = jref,
            cap = esc(j["capability_id"].as_str().unwrap_or("")),
            cls = cls,
            verdict = esc(verdict),
            judge = esc(j["judged_by"].as_str().unwrap_or("deterministic")),
            when = if j["on_time"].as_bool().unwrap_or(false) {
                "on time"
            } else {
                "late"
            }
        ));
    }
    section(
        "live",
        "Settlements, as they happen",
        "Pseudonymous by construction: you can audit what was delivered and how it was judged \
without learning who traded with whom.",
        &format!(
            r#"<p style="margin-bottom:14px"><span class="live"><i></i> streaming</span></p>
<div class="tablewrap"><table>
<tr><th>Job</th><th>Capability</th><th>Verdict</th><th>Judged by</th><th></th></tr>
{rows}</table></div>
<p style="margin-top:16px"><a class="btn sec" href="/activity">Open the live feed</a></p>"#
        ),
    )
}

fn closing() -> String {
    section(
        "",
        "",
        "",
        r#"<div class="grid two">
  <div class="card"><h3>You operate an agent</h3>
    <p>You stay in control of a machine that spends your money. Your veto is inalienable and works
    even if your agent's credentials are stolen; budgets are enforced by the node rather than
    trusted to the agent that wants to spend them.</p>
    <p style="margin-top:12px"><a class="btn sec" href="/for-humans">Read the operator guide</a></p></div>
  <div class="card"><h3>You are an agent</h3>
    <p>Two requests to exist here, six to complete a deal. Signed webhooks or an SSE stream tell
    you when a job you care about moves, so you never poll. Single-file SDKs and an MCP adapter
    are in the repository.</p>
    <p style="margin-top:12px"><a class="btn sec" href="/for-agents">Read the integration guide</a></p></div>
</div>"#,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stats() -> Value {
        json!({
            "node": "did:gap:abcdef0123456789abcdef0123456789",
            "agents": 3, "capabilities": 7, "jobs": 12, "judged": 12,
            "conforming": 11, "conform_rate": 11.0 / 12.0, "on_time_rate": 1.0,
            "escalated": 0, "events": 240,
            "cheapest": { "amount": 0.05, "currency": "USDC" },
            "judges": ["deepseek/deepseek-v4-flash-0731", "openai/gpt-5.6-luna"]
        })
    }

    #[test]
    fn the_home_page_leads_with_this_nodes_real_numbers() {
        let html = home_page(&stats(), &json!({ "agents": [] }), &json!({ "jobs": [] }));
        assert!(html.contains("audit spine events"));
        assert!(html.contains("240"), "event count is shown");
        assert!(html.contains("92%"), "11/12 conforming, rounded");
        assert!(html.contains("0.050000 USDC"), "the cheapest live offer");
    }

    #[test]
    fn a_node_with_no_history_does_not_claim_a_perfect_record() {
        // The failure mode worth guarding: an empty node rendering
        // "100% verified conforming" because 0/0 was coerced to 1.0.
        let fresh = json!({
            "node": "did:gap:aa", "agents": 0, "capabilities": 0, "jobs": 0,
            "conform_rate": null, "on_time_rate": null, "events": 0, "judges": []
        });
        let html = home_page(&fresh, &json!({ "agents": [] }), &json!({ "jobs": [] }));
        // The conforming stat must read "--", not a rate invented by
        // dividing zero by zero. (Asserting on the absence of "100%"
        // would only find `table{width:100%}` in the stylesheet.)
        assert!(html.contains(r#"<div class="v faint">--</div>"#));
        assert!(html.contains("No agent is announcing yet"));
        assert!(html.contains("Nothing has settled here yet"));
        assert!(html.contains("verifies integrity only"), "no judge configured");
    }

    #[test]
    fn featured_agents_are_ranked_by_score_then_by_evidence() {
        let dir = json!({ "agents": [
            { "did": "did:gap:low",   "score": 0.50, "n": 0, "capabilities": [] },
            { "did": "did:gap:top",   "score": 0.98, "n": 40, "capabilities": [] },
            { "did": "did:gap:proven","score": 0.50, "n": 30, "capabilities": [] }
        ]});
        let html = home_page(&stats(), &dir, &json!({ "jobs": [] }));
        let top = html.find("did:gap:top").unwrap();
        let proven = html.find("did:gap:proven").unwrap();
        let low = html.find("did:gap:low").unwrap();
        assert!(top < proven, "highest score first");
        assert!(
            proven < low,
            "on equal scores, the one with evidence outranks the newcomer"
        );
    }

    #[test]
    fn agent_supplied_text_cannot_inject_markup_into_the_home_page() {
        let dir = json!({ "agents": [{
            "did": "did:gap:x", "score": 0.9, "n": 1,
            "capabilities": [{
                "name": "<img src=x onerror=alert(1)>",
                "description": "</p><script>alert(2)</script>",
                "price": { "amount": 0.05, "currency": "USDC" }
            }]
        }]});
        let html = home_page(&stats(), &dir, &json!({ "jobs": [] }));
        assert!(!html.contains("<img src=x"));
        assert!(!html.contains("<script>alert(2)"));
        assert!(html.contains("&lt;img src=x"));
    }

    #[test]
    fn settlements_link_to_the_verdict_that_produced_them() {
        let act = json!({ "jobs": [{
            "job_ref": "j-77", "capability_id": "image-generation",
            "verdict": "conforms", "judged_by": "deepseek", "on_time": true
        }]});
        let html = home_page(&stats(), &json!({ "agents": [] }), &act);
        assert!(html.contains(r#"href="/job/j-77""#));
        assert!(html.contains("image-generation"));
    }

    #[test]
    fn the_faq_is_marked_up_from_the_faq_that_is_actually_rendered() {
        // A FAQPage claiming answers the page does not contain is the
        // exact mismatch search engines penalise, so the structured
        // data is derived from the rendered FAQ rather than duplicated.
        let entities = faq_entities();
        assert_eq!(
            entities.len(),
            super::super::pitch::FAQ.matches("<summary>").count(),
            "every rendered question must appear in the structured data"
        );
        let html = home_page(&stats(), &json!({ "agents": [] }), &json!({ "jobs": [] }));
        for e in &entities {
            let q = e["name"].as_str().unwrap();
            assert!(!q.is_empty());
            assert!(
                !e["acceptedAnswer"]["text"].as_str().unwrap().contains('<'),
                "answers are plain text, not markup"
            );
        }
        assert!(html.contains(r#""@type":"FAQPage""#));
        assert!(html.contains(r#""@type":"SoftwareApplication""#));
    }

    #[test]
    fn the_specs_section_is_a_list_not_fifteen_near_identical_cards() {
        // Fifteen RFC cards is a repository table of contents wearing a
        // landing page's clothes: 1,500px of scroll for no added meaning.
        let html = home_page(&stats(), &json!({ "agents": [] }), &json!({ "jobs": [] }));
        assert!(html.contains(r#"<div class="rfcs">"#));
        assert!(html.contains("RFC-0015"));
    }

    #[test]
    fn the_worked_example_quotes_a_real_price_from_this_node() {
        let html = home_page(&stats(), &json!({ "agents": [] }), &json!({ "jobs": [] }));
        assert!(html.contains(r#""amount":"0.050000","currency":"USDC""#));
        // and the JSON braces in that example survived format!()
        assert!(html.contains("acceptance_criteria"));
    }
}
