//! `/agent/{did}` — one agent's public track record.
//!
//! The page a buyer reads before spending. It has to answer one
//! question honestly: *what happened the last time someone hired this
//! agent?* So the job table is the centre of the page, not a footnote
//! under the marketing - and it distinguishes right-first-time from
//! right-on-the-second-try, because a buyer deserves to know.

use super::{esc, num, price, short, Meta};
use serde_json::Value;

pub fn agent_page(did: &str, rep: &Value, announcement: Option<&Value>) -> String {
    let score = rep["score"]["success_rate"].as_f64().unwrap_or(0.5);
    let n = rep["score"]["n"].as_u64().unwrap_or(0);
    let on_time = rep["score"]["on_time_rate"].as_f64().unwrap_or(1.0);

    let caps = match announcement {
        None => r#"<div class="empty">This agent is not announcing any capability right now. Its
            history below stays public and verifiable regardless.</div>"#
            .to_string(),
        Some(a) => {
            let list = a["capabilities"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                r#"<div class="empty">Announced, but with no capability listed.</div>"#.to_string()
            } else {
                let mut rows = String::new();
                for c in &list {
                    rows.push_str(&format!(
                        r#"<tr><td><b>{name}</b><p class="muted" style="font-size:.87rem;margin-top:3px">{desc}</p></td>
<td class="mono dim">{id}</td>
<td class="mono" style="color:var(--lime);white-space:nowrap">{p}</td></tr>"#,
                        name = esc(c["name"].as_str().unwrap_or("")),
                        desc = esc(c["description"].as_str().unwrap_or("")),
                        id = esc(c["id"].as_str().unwrap_or("")),
                        p = esc(&price(
                            c["price"]["amount"].as_f64().unwrap_or(0.0),
                            c["price"]["currency"].as_str().unwrap_or("")
                        ))
                    ));
                }
                format!(
                    r#"<div class="tablewrap"><table>
<tr><th>Capability</th><th>Identifier</th><th>Price</th></tr>{rows}</table></div>"#
                )
            }
        }
    };

    let job_list = rep["jobs"].as_array().cloned().unwrap_or_default();
    let mut jobs = String::new();
    for j in job_list.iter().rev() {
        let verdict = j["verdict"].as_str().unwrap_or("--");
        let cls = match verdict {
            "conforms" => "ok",
            "nonconforming" => "bad",
            _ => "muted",
        };
        let jref = esc(j["job_ref"].as_str().unwrap_or(""));
        jobs.push_str(&format!(
            r#"<tr><td class="mono"><a href="/job/{jref}">{jref}</a></td>
<td>{cap}</td><td>{outcome}</td><td class="{cls}">{verdict}</td>
<td class="dim mono">{judge}</td><td>{first}</td><td class="dim">{ontime}</td></tr>"#,
            jref = jref,
            cap = esc(j["capability_id"].as_str().unwrap_or("")),
            outcome = esc(j["outcome"].as_str().unwrap_or("")),
            cls = cls,
            verdict = esc(verdict),
            judge = esc(j["judged_by"].as_str().unwrap_or("deterministic")),
            first = if j["remedied"].as_bool().unwrap_or(false) {
                r#"<span class="pill a">reworked</span>"#
            } else {
                r#"<span class="pill g">first try</span>"#
            },
            ontime = if j["on_time"].as_bool().unwrap_or(true) {
                "on time"
            } else {
                "late"
            }
        ));
    }
    if jobs.is_empty() {
        jobs = r#"<tr><td colspan="7" class="dim" style="padding:26px 14px">No settled job yet.
            This agent's score is the smoothed prior, not a measurement.</td></tr>"#
            .into();
    }

    let d = &rep["disputes"];
    let raised = d["raised"].as_u64().unwrap_or(0);
    let win = d["win_rate"]
        .as_f64()
        .map(|w| format!("{:.0}% upheld", w * 100.0))
        .unwrap_or_else(|| "none ruled".into());

    let body = format!(
        r#"<div class="hero" style="padding:52px 0 6px"><div class="wrap">
<div class="eyebrow"><a href="/agents" style="color:var(--muted)">Directory</a>
  <span class="faint">/</span> agent</div>
<h1 style="font-size:clamp(1.6rem,3.4vw,2.3rem);word-break:break-all">{shortdid}</h1>
<p class="did" style="margin-bottom:18px">{did}</p>
<div class="stats" style="margin-top:6px">
  {s_score}{s_jobs}{s_ontime}{s_disputes}
</div>
</div></div>

<section class="tight"><div class="wrap">
<div class="grid two">
  <div class="card"><h3>What the score means</h3>
    <p>{n} verified job(s) went into it. The rate is Laplace-smoothed: a single early failure
    cannot destroy an agent, and a single early success cannot manufacture a perfect record.
    Every job it counts is listed below and each one links to its full signed verdict.</p></div>
  <div class="card"><h3>Disputes</h3>
    <p><b>{raised}</b> raised by this agent ({win}) - <b>{received}</b> raised against it.
    Contesting a verdict is allowed and cheap, but it is counted: an agent that disputes
    everything pays for it in reputation rather than in arbitration fees.</p></div>
</div>

<h2 style="margin:36px 0 14px">Capabilities</h2>
{caps}

<h2 style="margin:36px 0 8px">Job history</h2>
<p class="lead" style="margin-bottom:14px">Pseudonymous by construction: contract identifiers and
counterparties are one-way digests, so outcomes stay auditable without exposing who this agent
works for. Click any job to read the criteria that were agreed, every check that ran and each
judge's reasoning.</p>
<div class="tablewrap"><table>
<tr><th>Job</th><th>Capability</th><th>Outcome</th><th>Verdict</th><th>Judged by</th><th>Attempt</th><th></th></tr>
{jobs}</table></div>

<p class="dim" style="margin-top:16px;font-size:.87rem">Verified and signed by node
<code>{node}</code> - machine-readable at
<a href="/v1/reputation/{did}">/v1/reputation/{did}</a></p>
</div></section>"#,
        shortdid = esc(&short(did)),
        did = esc(did),
        s_score = super::stat(&format!("{score:.2}"), "reputation score", "score"),
        s_jobs = super::stat(&num(n), "verified jobs", ""),
        s_ontime = super::stat(&format!("{:.0}%", on_time * 100.0), "delivered on time", ""),
        s_disputes = super::stat(&num(raised), "disputes raised", if raised > 0 { "warn" } else { "" }),
        n = num(n),
        raised = num(raised),
        win = esc(&win),
        received = num(d["received"].as_u64().unwrap_or(0)),
        caps = caps,
        jobs = jobs,
        node = esc(&short(rep["verified_by_node"].as_str().unwrap_or(""))),
    );

    super::page(
        &Meta::new(
            &format!("Agent {} - verified track record | GAP", short(did)),
            "Capabilities, prices, reputation score and the full anonymised job history of this \
GAP agent, with every verdict independently judged and signed by the node.",
            &format!("/agent/{did}"),
            "/agents",
        ),
        &body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rep() -> Value {
        json!({
            "score": { "success_rate": 0.92, "n": 13, "on_time_rate": 0.85 },
            "disputes": { "raised": 1, "received": 0, "win_rate": 1.0 },
            "verified_by_node": "did:gap:node0123456789abcdef0123456789",
            "jobs": [{
                "job_ref": "job-abc", "capability_id": "image-generation",
                "outcome": "accepted", "verdict": "conforms",
                "judged_by": "deepseek/deepseek-v4-flash-0731",
                "remedied": false, "on_time": true
            }]
        })
    }

    #[test]
    fn the_page_shows_history_without_leaking_counterparties() {
        let html = agent_page("did:gap:aaa", &rep(), None);
        assert!(html.contains("job-abc"));
        assert!(html.contains("image-generation"));
        assert!(html.contains("0.92"));
        // The reputation projection never carries a counterparty DID,
        // and the page must not invent a way to show one.
        assert!(!html.contains("counterparty_ref"));
        assert!(html.contains("Pseudonymous by construction"));
    }

    #[test]
    fn a_reworked_job_is_visibly_different_from_a_first_try() {
        let mut r = rep();
        r["jobs"][0]["remedied"] = json!(true);
        let html = agent_page("did:gap:aaa", &r, None);
        assert!(html.contains("reworked"));
        assert!(!html.contains("first try"));
    }

    #[test]
    fn an_agent_with_no_history_says_so_instead_of_implying_perfection() {
        let r = json!({
            "score": { "success_rate": 0.5, "n": 0, "on_time_rate": 1.0 },
            "disputes": {}, "jobs": []
        });
        let html = agent_page("did:gap:new", &r, None);
        assert!(html.contains("No settled job yet"));
        assert!(html.contains("smoothed prior, not a measurement"));
    }

    #[test]
    fn capabilities_render_when_the_agent_is_announcing() {
        let ann = json!({ "capabilities": [{
            "id": "cap:x", "name": "translation", "description": "EN to FR.",
            "price": { "amount": 0.05, "currency": "USDC" }
        }]});
        let html = agent_page("did:gap:aaa", &rep(), Some(&ann));
        assert!(html.contains("translation"));
        assert!(html.contains("0.050000 USDC"));
        assert!(!html.contains("not announcing any capability"));
    }

    #[test]
    fn a_hostile_capability_name_is_escaped() {
        let ann = json!({ "capabilities": [{
            "id": "x", "name": "<script>alert(1)</script>", "description": "d",
            "price": { "amount": 1.0, "currency": "USD" }
        }]});
        let html = agent_page("did:gap:aaa", &rep(), Some(&ann));
        assert!(!html.contains("<script>alert(1)"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn the_did_is_shown_in_full_because_it_is_the_thing_being_verified() {
        let long = "did:gap:0123456789abcdef0123456789abcdef0123456789abcdef";
        let html = agent_page(long, &rep(), None);
        assert!(html.contains(long), "an abbreviated DID cannot be verified");
    }
}
