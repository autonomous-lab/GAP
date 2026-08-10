//! `/capability/{id}` — one service, and how it has actually gone.
//!
//! The node could already be audited along two axes: an agent, and a
//! job. This is the third, and it is the one a buyer shops on. Nobody
//! asks "is this agent good" in the abstract; they ask "has *this
//! service* been delivered before, and what happened".
//!
//! Jobs stay pseudonymous exactly as they are everywhere else. The
//! capability is public; who bought it is not.

use super::{esc, num, price, short, Meta};
use serde_json::Value;

pub fn capability_page(cap: &Value) -> String {
    let id = cap["capability_id"].as_str().unwrap_or("");
    let name = cap["name"].as_str().unwrap_or("").trim();
    let heading = if name.is_empty() { id } else { name };
    let description = cap["description"].as_str().unwrap_or("").trim();

    let offers = cap["offers"].as_array().cloned().unwrap_or_default();
    let jobs = cap["jobs"].as_array().cloned().unwrap_or_default();
    let settled = cap["settled"].as_u64().unwrap_or(0);

    // ---- who offers it -------------------------------------------
    let mut offer_rows = String::new();
    for o in &offers {
        let did = o["did"].as_str().unwrap_or("");
        let score = o["score"].as_f64().unwrap_or(0.5);
        offer_rows.push_str(&format!(
            r#"<tr><td><a href="/agent/{did}">{label}</a>
<div class="did" style="font-size:.72rem;margin-top:2px">{shortdid}</div></td>
<td><span class="score">{score:.2}</span>
<span class="dim" style="font-size:.85rem"> over {n} job(s)</span></td>
<td class="mono" style="color:var(--lime);white-space:nowrap">{p}</td></tr>"#,
            did = esc(did),
            label = esc(&super::display_name(o["name"].as_str().unwrap_or(""), did)),
            shortdid = esc(&short(did)),
            score = score,
            n = num(o["n"].as_u64().unwrap_or(0)),
            p = esc(&price(
                o["price"]["amount"].as_f64().unwrap_or(0.0),
                o["price"]["currency"].as_str().unwrap_or("")
            ))
        ));
    }
    let offers_block = if offer_rows.is_empty() {
        // A withdrawn capability keeps its history: that is most of the
        // point of writing one down.
        r#"<div class="empty">Nobody is offering this capability right now. Its history below
        stays public and verifiable regardless.</div>"#
            .to_string()
    } else {
        format!(
            r#"<div class="tablewrap"><table>
<tr><th>Agent</th><th>Reputation</th><th>Price</th></tr>{offer_rows}</table></div>"#
        )
    };

    // ---- what happened when it was bought ------------------------
    let mut job_rows = String::new();
    for j in &jobs {
        let verdict = j["verdict"].as_str().unwrap_or("--");
        let cls = match verdict {
            "conforms" => "ok",
            "nonconforming" => "bad",
            _ => "muted",
        };
        let jref = esc(j["job_ref"].as_str().unwrap_or(""));
        job_rows.push_str(&format!(
            r#"<tr><td class="mono"><a href="/job/{jref}">{jref}</a></td>
<td>{outcome}</td><td class="{cls}">{verdict}</td>
<td class="dim mono">{judge}</td><td>{attempt}</td><td class="dim">{ontime}</td></tr>"#,
            jref = jref,
            outcome = esc(j["outcome"].as_str().unwrap_or("")),
            cls = cls,
            verdict = esc(verdict),
            judge = esc(j["judged_by"].as_str().unwrap_or("deterministic")),
            attempt = if j["remedied"].as_bool().unwrap_or(false) {
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
    if job_rows.is_empty() {
        job_rows = r#"<tr><td colspan="6" class="dim" style="padding:26px 14px">Nobody has bought
            this capability on this node yet. When someone does, the verdict appears here in
            full.</td></tr>"#
            .into();
    }

    let rate = match cap["conform_rate"].as_f64() {
        Some(r) => super::stat(&format!("{:.0}%", r * 100.0), "verified conforming", "ok"),
        None if settled == 0 => super::stat("--", "verified conforming", "faint"),
        None => super::stat(&format!("0/{settled}"), "verified conforming", "faint"),
    };
    let on_time = match cap["on_time_rate"].as_f64() {
        Some(r) => super::stat(&format!("{:.0}%", r * 100.0), "delivered on time", ""),
        None => super::stat("--", "delivered on time", "faint"),
    };

    let body = format!(
        r#"<div class="hero" style="padding:44px 0 6px"><div class="wrap">
<div class="eyebrow"><a href="/agents" style="color:var(--muted)">Directory</a>
  <span class="faint">/</span> capability</div>
<h1 style="font-size:clamp(1.7rem,3.6vw,2.4rem);overflow-wrap:anywhere">{heading}</h1>
{blurb}
<p class="did" style="margin-bottom:16px">{id}</p>
<div class="stats" style="margin-top:6px">
  {s_settled}{s_rate}{s_ontime}{s_offers}{s_remedied}
</div>
</div></div>

<section class="tight"><div class="wrap">
<h2 style="margin:20px 0 12px">Who offers it</h2>
{offers_block}

<h2 style="margin:36px 0 8px">Contract history</h2>
<p class="lead" style="margin-bottom:14px">Every settled contract that used this capability, newest
first. Pseudonymous by construction: contract identifiers and counterparties are one-way digests,
so you can audit how the work went without learning who commissioned it. Click any job for the
criteria that were agreed, every check that ran and each judge's reasoning.</p>
<div class="tablewrap"><table>
<tr><th>Job</th><th>Outcome</th><th>Verdict</th><th>Judged by</th><th>Attempt</th><th></th></tr>
{job_rows}</table></div>

<p class="dim" style="margin-top:16px;font-size:.87rem">Machine-readable:
<a href="/v1/capability/{id}">/v1/capability/{id}</a> - discovery:
<a href="/v1/discover?name={query}">/v1/discover?name={query}</a></p>
</div></section>"#,
        heading = esc(heading),
        blurb = if description.is_empty() {
            String::new()
        } else {
            format!(r#"<p class="sub">{}</p>"#, esc(description))
        },
        id = esc(id),
        s_settled = super::stat(&num(settled), "contracts settled", ""),
        s_rate = rate,
        s_ontime = on_time,
        s_offers = super::stat(&num(offers.len() as u64), "agents offering", "cy"),
        s_remedied = super::stat(
            &num(cap["remedied"].as_u64().unwrap_or(0)),
            "needed rework",
            ""
        ),
        offers_block = offers_block,
        job_rows = job_rows,
        query = esc(if name.is_empty() { id } else { name }),
    );

    // A capability with a price is an offer, and telling a crawler so is
    // what gets it found by someone shopping rather than reading.
    let jsonld = offers
        .first()
        .map(|o| {
            serde_json::json!({
                "@context": "https://schema.org",
                "@type": "Service",
                "name": if name.is_empty() { id } else { name },
                "description": description,
                "serviceType": id,
                "offers": {
                    "@type": "Offer",
                    "price": o["price"]["amount"],
                    "priceCurrency": o["price"]["currency"]
                }
            })
            .to_string()
        })
        .unwrap_or_default();

    // Bound before the Meta: it borrows these, and a temporary would be
    // dropped at the end of the statement that built it.
    let label = if name.is_empty() { id } else { name };
    let title = format!("{label} - price, providers and verified delivery history | GAP");
    let description = format!(
        "What {label} costs on this GAP node, which agents offer it, and every contract that \
used it with its signed verdict."
    );
    let canonical = format!("/capability/{id}");
    let meta = Meta::new(&title, &description, &canonical, "/agents")
        .on_node(cap["node"].as_str().unwrap_or(""));
    let meta = if jsonld.is_empty() {
        meta
    } else {
        meta.with_jsonld(jsonld)
    };
    super::page(&meta, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cap() -> Value {
        json!({
            "capability_id": "cap:albert:image-generation",
            "name": "image-generation",
            "description": "Generate raster images from a prompt.",
            "node": "did:gap:node0123456789abcdef",
            "offers": [{
                "did": "did:gap:0123456789abcdef0123456789abcdef",
                "name": "Albert Einstein Imagegen",
                "price": { "amount": 0.02, "currency": "EUR" },
                "score": 0.93, "n": 14
            }],
            "jobs": [{
                "seq": 7, "job_ref": "job-7", "outcome": "accepted",
                "verdict": "conforms", "judged_by": "luna",
                "remedied": false, "on_time": true
            }],
            "settled": 1, "judged": 1, "conforming": 1,
            "conform_rate": 1.0, "on_time_rate": 1.0, "remedied": 0
        })
    }

    #[test]
    fn the_page_answers_what_a_buyer_actually_asks() {
        // Not "is this agent good" but "has this service been delivered
        // before, and how did it go".
        let html = capability_page(&cap());
        assert!(html.contains("image-generation"));
        assert!(html.contains("0.020000 EUR"));
        assert!(html.contains("Albert Einstein Imagegen"));
        assert!(html.contains(r#"href="/job/job-7""#));
        assert!(html.contains(r#"href="/agent/did:gap:0123456789abcdef0123456789abcdef""#));
    }

    #[test]
    fn a_withdrawn_capability_keeps_its_history() {
        // Records outlive an announcement, and that is most of the point
        // of keeping one.
        let mut c = cap();
        c["offers"] = json!([]);
        let html = capability_page(&c);
        assert!(html.contains("Nobody is offering this capability right now"));
        assert!(html.contains("job-7"), "the history survives: {html:.0}");
    }

    #[test]
    fn an_unbought_capability_does_not_read_as_flawless() {
        // A rate over zero evidence must not render as 100%.
        let mut c = cap();
        c["jobs"] = json!([]);
        c["settled"] = json!(0);
        c["conform_rate"] = Value::Null;
        c["on_time_rate"] = Value::Null;
        let html = capability_page(&c);
        assert!(html.contains("Nobody has bought"));
        assert!(html.contains(r#"<div class="v faint">--</div>"#));
    }

    #[test]
    fn a_hostile_capability_name_cannot_inject_markup() {
        let mut c = cap();
        c["name"] = json!("<script>alert(1)</script>");
        c["description"] = json!("</p><img src=x onerror=alert(2)>");
        let html = capability_page(&c);
        assert!(!html.contains("<script>alert(1)"));
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn it_never_reveals_who_commissioned_the_work() {
        let html = capability_page(&cap());
        assert!(!html.contains("urn:gap:ctr"));
        assert!(!html.to_lowercase().contains("counterparty"));
    }

    #[test]
    fn a_priced_capability_is_marked_up_as_an_offer() {
        // What gets it found by someone shopping rather than reading.
        let html = capability_page(&cap());
        assert!(html.contains(r#""@type":"Service""#));
        assert!(html.contains(r#""priceCurrency":"EUR""#));
    }
}
