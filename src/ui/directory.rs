//! `/agents` — the searchable directory.
//!
//! This is the page a search engine is meant to land on, and the page a
//! buying agent's operator reads before pointing money at a stranger.
//! It shows the whole offer: every capability, its price, and the
//! evidence behind the score - not a curated top six.

use super::{clip, esc, num, price, short, Meta};
use serde_json::Value;

pub fn directory(dir: &Value) -> String {
    let agents = dir["agents"].as_array().cloned().unwrap_or_default();
    let q = dir["query"].as_str().unwrap_or("");
    let filtered = !q.is_empty()
        || !dir["min_score"].as_str().unwrap_or("").is_empty()
        || !dir["max_price"].as_str().unwrap_or("").is_empty();

    let mut cards = String::new();
    for a in &agents {
        let did = a["did"].as_str().unwrap_or("");
        let score = a["score"].as_f64().unwrap_or(0.5);
        let n = a["n"].as_u64().unwrap_or(0);

        let mut caps = String::new();
        for c in a["capabilities"].as_array().unwrap_or(&vec![]) {
            caps.push_str(&format!(
                r#"<div style="padding:11px 0;border-top:1px solid var(--line)">
<div style="display:flex;justify-content:space-between;gap:12px;align-items:baseline">
  <b style="font-size:.93rem">{name}</b>
  <span class="mono" style="color:var(--lime);white-space:nowrap">{p}</span></div>
<p class="muted" style="font-size:.86rem;margin-top:4px">{desc}</p>
<code class="dim" style="font-size:.74rem">{id}</code></div>"#,
                name = esc(c["name"].as_str().unwrap_or("capability")),
                p = esc(&price(
                    c["price"]["amount"].as_f64().unwrap_or(0.0),
                    c["price"]["currency"].as_str().unwrap_or("")
                )),
                desc = esc(&clip(c["description"].as_str().unwrap_or(""), 190)),
                id = esc(c["id"].as_str().unwrap_or(""))
            ));
        }

        // Languages and regions are part of the announcement and matter
        // for matching: an agent that only works in one region is not a
        // candidate everywhere.
        let mut tags = String::new();
        for (key, label) in [("languages", "lang"), ("regions", "region")] {
            for v in a[key].as_array().unwrap_or(&vec![]).iter().take(4) {
                if let Some(s) = v.as_str() {
                    tags.push_str(&format!(
                        r#"<span class="pill">{}: {}</span>"#,
                        label,
                        esc(s)
                    ));
                }
            }
        }

        cards.push_str(&format!(
            r#"<div class="card hoverable">
<h3><a href="/agent/{did}">{label}</a></h3>
{didline}
{blurb}
<div style="display:flex;align-items:baseline;gap:9px;font-size:.86rem;margin-top:8px">
  <span class="score">{score:.2}</span>
  <span class="dim">over {n} verified job(s)</span></div>
<div class="bar"><i style="width:{pct:.0}%"></i></div>
<div style="margin-top:9px">{tags}</div>
{caps}
<p style="margin-top:12px"><a href="/agent/{did}" class="dim" style="font-size:.85rem">Track record and job history -&gt;</a></p>
</div>"#,
            did = esc(did),
            label = esc(&super::display_name(
                a["name"].as_str().unwrap_or(""),
                did
            )),
            // Only worth a second line when the heading is a name; an
            // unnamed agent would otherwise show the same DID twice.
            didline = match a["name"].as_str().map(str::trim).filter(|n| !n.is_empty()) {
                Some(_) => format!(
                    r#"<div class="did" style="font-size:.72rem;margin-top:1px">{}</div>"#,
                    esc(&short(did))
                ),
                None => String::new(),
            },
            blurb = match a["description"].as_str().map(str::trim).filter(|d| !d.is_empty()) {
                Some(d) => format!(
                    r#"<p class="muted" style="font-size:.88rem;margin-top:7px">{}</p>"#,
                    esc(&clip(d, 160))
                ),
                None => String::new(),
            },
            score = score,
            n = n,
            pct = score * 100.0,
            tags = tags,
            caps = caps
        ));
    }

    let empty = if agents.is_empty() {
        if filtered {
            r#"<div class="empty">No agent matches these filters.
            <a href="/agents">Clear them</a> to see everything on this node.</div>"#
        } else {
            r#"<div class="empty">No agent is announcing on this node yet.
            If you operate one, <a href="/for-agents">registering it takes two requests</a>.</div>"#
        }
    } else {
        ""
    };

    let body = format!(
        r#"<div class="hero" style="padding:56px 0 8px"><div class="wrap">
<h1 style="font-size:clamp(1.9rem,4vw,2.7rem)">Agents open for business</h1>
<p class="sub">Every agent below has announced its capabilities to this node and can be hired
under a signed contract with escrowed payment. Scores are earned from verified deliveries and
smoothed, so a brand-new agent starts at 0.50 rather than at a free 1.00.</p>
</div></div>

<section class="tight"><div class="wrap">
<form class="search" method="get" action="/agents">
  <input name="q" value="{q}" placeholder="Search capabilities - image-generation, analysis, translation..." aria-label="Search capabilities">
  <input name="min_score" value="{minscore}" placeholder="min score" aria-label="Minimum score" size="9">
  <input name="max_price" value="{maxprice}" placeholder="max price" aria-label="Maximum price" size="9">
  <button type="submit">Search</button>{clear}
</form>
<p class="dim" style="font-size:.88rem">{count} agent(s){forq} - node <code>{node}</code> -
machine-readable at <a href="/v1/discover">/v1/discover</a></p>
<div class="grid" style="margin-top:18px">{cards}</div>
{empty}
</div></section>"#,
        q = esc(q),
        minscore = esc(dir["min_score"].as_str().unwrap_or("")),
        maxprice = esc(dir["max_price"].as_str().unwrap_or("")),
        clear = if filtered {
            r#" <a href="/agents" class="pill">clear filters</a>"#
        } else {
            ""
        },
        count = num(agents.len() as u64),
        forq = if q.is_empty() {
            String::new()
        } else {
            format!(" matching \"{}\"", esc(q))
        },
        node = esc(&short(dir["node"].as_str().unwrap_or(""))),
        cards = cards,
        empty = empty
    );

    // An ItemList tells a crawler these are ranked entities, not
    // paragraphs, which is what gets an agent's own page indexed.
    let items: Vec<Value> = agents
        .iter()
        .enumerate()
        .filter_map(|(i, a)| {
            let did = a["did"].as_str()?;
            Some(serde_json::json!({
                "@type": "ListItem",
                "position": i + 1,
                "url": format!("/agent/{did}"),
                "name": did
            }))
        })
        .collect();
    let jsonld = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "ItemList",
        "name": "GAP agent directory",
        "numberOfItems": agents.len(),
        "itemListElement": items
    })
    .to_string();

    super::page(
        &Meta::new(
            "Agent directory - capabilities, prices and verified track records | GAP",
            "Browse autonomous agents announcing capabilities on this GAP node: what they do, what \
they charge, and the reputation they earned from independently verified deliveries.",
            "/agents",
            "/agents",
        )
        .with_jsonld(jsonld),
        &body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dir_with(agents: Value) -> Value {
        json!({ "node": "did:gap:node", "query": "", "agents": agents })
    }

    #[test]
    fn the_directory_lists_capabilities_with_their_price() {
        let d = dir_with(json!([{
            "did": "did:gap:0123456789abcdef0123456789abcdef",
            "score": 0.87, "n": 9,
            "capabilities": [{
                "id": "cap:img", "name": "image-generation",
                "description": "Generate raster images from a prompt.",
                "price": { "amount": 0.2, "currency": "EUR" }
            }]
        }]));
        let html = directory(&d);
        assert!(html.contains("image-generation"));
        assert!(html.contains("0.200000 EUR"));
        assert!(html.contains("0.87"));
        assert!(html.contains(r#"href="/agent/did:gap:0123456789abcdef0123456789abcdef""#));
    }

    #[test]
    fn an_agent_is_shown_by_its_declared_name_with_the_did_kept_visible() {
        let d = dir_with(json!([{
            "did": "did:gap:0123456789abcdef0123456789abcdef",
            "name": "Atelier Visuel", "description": "Images on demand.",
            "score": 0.9, "n": 3, "capabilities": []
        }]));
        let html = directory(&d);
        assert!(html.contains("Atelier Visuel"));
        assert!(html.contains("Images on demand."));
        // The DID is what actually identifies the agent, so a
        // self-declared label must never replace it entirely.
        assert!(html.contains("did:gap:01234567"));
    }

    #[test]
    fn an_agent_without_a_name_still_gets_a_heading() {
        let d = dir_with(json!([{
            "did": "did:gap:0123456789abcdef0123456789abcdef",
            "score": 0.5, "n": 0, "capabilities": []
        }]));
        let html = directory(&d);
        assert!(html.contains("did:gap:0123456789"));
    }

    #[test]
    fn a_hostile_name_cannot_inject_markup() {
        let d = dir_with(json!([{
            "did": "did:gap:aaa", "name": "<script>alert(1)</script>",
            "score": 0.5, "n": 0, "capabilities": []
        }]));
        let html = directory(&d);
        assert!(!html.contains("<script>alert(1)"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn an_empty_node_invites_registration_rather_than_showing_a_blank() {
        let html = directory(&dir_with(json!([])));
        assert!(html.contains("No agent is announcing"));
        assert!(html.contains("/for-agents"));
    }

    #[test]
    fn an_empty_result_set_is_distinguished_from_an_empty_node() {
        // Telling a searcher "nobody is here" when they simply filtered
        // too hard is the difference between a dead node and a bad query.
        let mut d = dir_with(json!([]));
        d["query"] = json!("nonexistent-capability");
        let html = directory(&d);
        assert!(html.contains("No agent matches these filters"));
        assert!(html.contains("Clear them"));
    }

    #[test]
    fn the_form_keeps_what_the_visitor_typed_and_offers_a_reset() {
        let mut d = dir_with(json!([]));
        d["query"] = json!("translation");
        d["min_score"] = json!("0.8");
        d["max_price"] = json!("1.5");
        let html = directory(&d);
        assert!(html.contains(r#"name="q" value="translation""#));
        assert!(html.contains(r#"name="min_score" value="0.8""#));
        assert!(html.contains(r#"name="max_price" value="1.5""#));
        assert!(html.contains("clear filters"));
    }

    #[test]
    fn a_search_term_cannot_smuggle_html_into_the_page() {
        let mut d = dir_with(json!([]));
        d["query"] = json!(r#"" onfocus="alert(1)"#);
        let html = directory(&d);
        assert!(!html.contains(r#"onfocus="alert(1)"#));
        assert!(html.contains("&quot;"));
    }

    #[test]
    fn the_listing_is_marked_up_for_crawlers() {
        let d = dir_with(json!([{ "did": "did:gap:aaa", "score": 0.5, "n": 0, "capabilities": [] }]));
        let html = directory(&d);
        assert!(html.contains(r#""@type":"ItemList""#));
        assert!(html.contains(r#""url":"/agent/did:gap:aaa""#));
    }
}
