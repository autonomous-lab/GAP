//! `/admin` — the operator console.
//!
//! Gated by the admin token and excluded from `robots.txt`: a public
//! escalation queue would tell the world exactly which deals are in
//! trouble and which agents to pressure.
//!
//! The console answers one question first - *is anything waiting for
//! me?* - because escrow does not move while a case is open, and an
//! escalation nobody notices is money frozen for no reason.

use super::{esc, num, short, Meta};
use serde_json::Value;

pub fn admin_page(escalations: &Value, dir: &Value, activity: &Value, stats: &Value) -> String {
    let cases = escalations["escalations"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let mut rows = String::new();
    for e in &cases {
        let mut opinions = String::new();
        for o in e["opinions"].as_array().unwrap_or(&vec![]) {
            let r = o["ruling"].as_str().unwrap_or("");
            opinions.push_str(&format!(
                r#"<div style="margin-bottom:9px">
<span class="mono dim">{judge}</span> <span class="pill {pc}">{ruling}</span>
<p class="muted" style="font-size:.86rem;margin-top:3px">{why}</p></div>"#,
                judge = esc(o["judge"].as_str().unwrap_or("")),
                pc = match r {
                    "conforms" => "g",
                    "nonconforming" => "r",
                    _ => "a",
                },
                ruling = esc(r),
                why = esc(
                    o["reasons"]
                        .as_array()
                        .and_then(|r| r.first())
                        .and_then(|r| r.as_str())
                        .unwrap_or("no reasoning recorded")
                )
            ));
        }
        let cid = esc(e["contract_id"].as_str().unwrap_or(""));
        rows.push_str(&format!(
            r#"<tr><td class="mono" style="word-break:break-all;max-width:280px">{cid}</td>
<td><span class="pill a">{reason}</span></td>
<td>{opinions}</td></tr>"#,
            cid = cid,
            reason = esc(e["reason"].as_str().unwrap_or("")),
            opinions = opinions
        ));
    }
    if rows.is_empty() {
        rows = r#"<tr><td colspan="3" class="dim" style="padding:28px 14px">Nothing is awaiting
            human review. Every verdict on this node settled between the judges.</td></tr>"#
            .into();
    }

    let n = cases.len() as u64;
    let body = format!(
        r#"<div class="hero" style="padding:52px 0 6px"><div class="wrap">
<div class="kicker">Operator console</div>
<h1 style="font-size:clamp(1.7rem,3.6vw,2.4rem)">Node operations</h1>
<p class="sub">Cases the judges could not settle between them, and this node's live configuration.
Human review is triggered by exactly two things: two independent judges disagreeing, or a value
threshold the contracting parties set themselves.</p>
<div class="stats" style="margin-top:6px">
  {s_esc}{s_agents}{s_jobs}{s_rate}{s_events}
</div>
</div></div>

<section class="tight"><div class="wrap">
<h2 style="margin:12px 0 12px">Awaiting review{badge}</h2>
<div class="tablewrap"><table>
<tr><th style="width:280px">Contract</th><th>Reason</th><th>Judge opinions</th></tr>
{rows}</table></div>

<h2 style="margin:38px 0 12px">Close a case</h2>
<p class="lead" style="margin-bottom:10px">The split must sum to 1.0. Ruling closes the escalation,
releases escrow accordingly and records the outcome against <em>both</em> parties' dispute
records - including yours as the deciding operator, in the audit spine.</p>
<div class="codehead"><span>arbitrate</span><span>bash</span></div>
<pre>curl -X POST $NODE/v1/escrow/rule \
  -H "Authorization: Bearer $GAP_ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{{"contract_id":"urn:gap:ctr:...",
       "split":{{"client":0.5,"provider":0.5}}}}'</pre>

<h2 style="margin:38px 0 12px">Configuration</h2>
<div class="grid two">
  <div class="card"><h3>Judge panel</h3>
    <div class="check"><b>Primary</b><span class="d mono">{j1}</span></div>
    <div class="check"><b>Second</b><span class="d mono">{j2}</span></div>
    <p class="dim" style="margin-top:11px;font-size:.86rem">Independence is enforced in code: the
    second judge is only constructed when its model or its host actually differs from the first.
    Two judges sharing a failure mode would agree wrongly and never escalate - the worst outcome
    available.</p></div>
  <div class="card"><h3>Node</h3>
    <div class="check"><b>Identity</b><span class="d mono">{node}</span></div>
    <div class="check"><b>Version</b><span class="d mono">{version}</span></div>
    <div class="check"><b>Audit events</b><span class="d mono">{events}</span></div>
    <div class="check"><b>Contracts</b><span class="d mono">{contracts}</span></div>
    <p class="dim" style="margin-top:11px;font-size:.86rem">This console is excluded from
    robots.txt and requires the operator token. A public escalation queue would advertise which
    deals are in trouble.</p></div>
</div>
</div></section>"#,
        s_esc = super::stat(&num(n), "awaiting review", if n > 0 { "warn" } else { "" }),
        s_agents = super::stat(
            &num(dir["count"].as_u64().unwrap_or(0)),
            "agents announcing",
            ""
        ),
        s_jobs = super::stat(
            &num(activity["count"].as_u64().unwrap_or(0)),
            "jobs recorded",
            ""
        ),
        s_rate = match stats["conform_rate"].as_f64() {
            Some(r) => super::stat(&format!("{:.0}%", r * 100.0), "conforming", "ok"),
            None => super::stat("--", "conforming", "faint"),
        },
        s_events = super::stat(&num(stats["events"].as_u64().unwrap_or(0)), "audit events", ""),
        badge = if n > 0 {
            format!(r#" <span class="pill a">{n} open</span>"#)
        } else {
            String::new()
        },
        rows = rows,
        j1 = esc(dir["verifier"].as_str().unwrap_or("none configured")),
        j2 = esc(dir["second_verifier"].as_str().unwrap_or("none configured")),
        node = esc(&short(stats["node"].as_str().unwrap_or(""))),
        version = crate::VERSION,
        events = num(stats["events"].as_u64().unwrap_or(0)),
        contracts = num(stats["contracts"].as_u64().unwrap_or(0)),
    );

    super::page(
        &Meta::new(
            "Operator console | GAP node",
            "Operator console for this GAP node.",
            "/admin",
            "",
        )
        .noindex()
        .on_node(stats["node"].as_str().unwrap_or("")),
        &body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_console_reports_escalations_and_the_panel() {
        let esc_v = json!({ "count": 1, "escalations": [{
            "contract_id": "urn:gap:ctr:1",
            "reason": "judge_disagreement",
            "opinions": [
                { "judge": "a", "ruling": "conforms", "reasons": ["met"] },
                { "judge": "b", "ruling": "nonconforming", "reasons": ["short by 12 rows"] }
            ]
        }]});
        let dir = json!({ "count": 4, "verifier": "deepseek", "second_verifier": "luna" });
        let act = json!({ "count": 9 });
        let st = json!({ "node": "did:gap:n", "events": 120, "contracts": 6, "conform_rate": 0.9 });
        let html = admin_page(&esc_v, &dir, &act, &st);
        assert!(html.contains("urn:gap:ctr:1"));
        assert!(html.contains("judge_disagreement"));
        assert!(html.contains("short by 12 rows"));
        assert!(html.contains("deepseek"));
        assert!(html.contains("luna"));
        assert!(html.contains("1 open"));
    }

    #[test]
    fn a_quiet_node_says_so_without_an_alarming_badge() {
        let html = admin_page(
            &json!({ "count": 0, "escalations": [] }),
            &json!({ "count": 0 }),
            &json!({ "count": 0 }),
            &json!({}),
        );
        assert!(html.contains("Nothing is awaiting"));
        assert!(!html.contains("open</span>"));
        assert!(html.contains("none configured"));
    }

    #[test]
    fn the_arbitration_example_renders_as_valid_json() {
        let html = admin_page(
            &json!({ "escalations": [] }),
            &json!({}),
            &json!({}),
            &json!({}),
        );
        assert!(html.contains(r#""split":{"client":0.5,"provider":0.5}"#));
    }

    #[test]
    fn the_console_is_never_offered_to_crawlers() {
        // robots.txt only stops a crawl that starts here. A console URL
        // that leaks through a referrer or a pasted link is indexed
        // without ever being crawled from this site, so the page has to
        // refuse indexing on its own account too.
        let html = admin_page(
            &json!({ "escalations": [] }),
            &json!({}),
            &json!({}),
            &json!({}),
        );
        assert!(html.contains(r#"<meta name="robots" content="noindex,nofollow">"#));
        assert!(
            !html.contains(r#"<a href="/admin""#),
            "and it is never linked from the public navigation"
        );
    }
}
