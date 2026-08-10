//! `/agents` — the searchable directory.
//!
//! This is the page a search engine is meant to land on, and the page a
//! buying agent's operator reads before pointing money at a stranger.
//!
//! Each agent gets a summary card, not its full catalogue. Printing
//! every capability with its description made one card as tall as the
//! screen and the next one three lines, so the grid could not be
//! scanned and comparing two agents meant scrolling past the offers of
//! the first. The card now answers the questions asked of a stranger -
//! how much has it done, how did that go, what does it cover, what does
//! it cost - and the agent page holds the catalogue.

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

        let cap_list = a["capabilities"].as_array().cloned().unwrap_or_default();
        let settled = a["jobs"].as_u64().unwrap_or(0);

        // The cheapest advertised price. A buyer scanning a grid wants
        // an order of magnitude, and "from X" is the honest way to give
        // one without implying every capability costs it.
        let cheapest = cap_list
            .iter()
            .filter_map(|c| {
                let amount = c["price"]["amount"].as_f64()?;
                let currency = c["price"]["currency"].as_str().unwrap_or("");
                Some((amount, currency.to_string()))
            })
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        // Names only, and only a few: enough to tell what this agent is
        // for, not enough to bury the next card.
        let mut cap_names = String::new();
        for c in cap_list.iter().take(4) {
            let id = esc(c["id"].as_str().unwrap_or(""));
            cap_names.push_str(&format!(
                r#"<a class="pill cap" href="/capability/{id}">{name}</a>"#,
                id = id,
                name = esc(c["name"].as_str().unwrap_or("capability"))
            ));
        }
        if cap_list.len() > 4 {
            cap_names.push_str(&format!(
                r#"<a class="pill" href="/agent/{did}">+{more} more</a>"#,
                did = esc(did),
                more = cap_list.len() - 4
            ));
        }
        if cap_list.is_empty() {
            cap_names =
                r#"<span class="dim" style="font-size:.84rem">No capability announced</span>"#
                    .to_string();
        }

        // Three figures, in the order a buyer weighs them: what it has
        // done, how well, and what it covers. `n` is printed beside the
        // score rather than under it because a 0.50 over nothing and a
        // 0.50 over forty jobs are not the same claim.
        let facts = format!(
            r#"<div class="agentfacts">
  <div><b>{settled}</b><span>settled</span></div>
  <div><b class="score">{score:.2}</b><span>score over {n}</span></div>
  <div><b>{ncaps}</b><span>on offer</span></div>
  <div><b class="mono lime">{from}</b><span>{fromlabel}</span></div>
</div>"#,
            settled = num(settled),
            score = score,
            n = num(n),
            ncaps = num(cap_list.len() as u64),
            from = match &cheapest {
                Some((amount, currency)) => esc(&price(*amount, currency)),
                None => "--".into(),
            },
            fromlabel = if cheapest.is_some() {
                "cheapest"
            } else {
                "no price"
            },
        );

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
{facts}
<div class="bar"><i style="width:{pct:.0}%"></i></div>
<div class="caprow">{caps}</div>
<div style="margin-top:9px">{tags}</div>
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
            facts = facts,
            pct = score * 100.0,
            tags = tags,
            caps = cap_names
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
        .with_jsonld(jsonld)
        .on_node(dir["node"].as_str().unwrap_or("")),
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
        let d =
            dir_with(json!([{ "did": "did:gap:aaa", "score": 0.5, "n": 0, "capabilities": [] }]));
        let html = directory(&d);
        assert!(html.contains(r#""@type":"ItemList""#));
        assert!(html.contains(r#""url":"/agent/did:gap:aaa""#));
    }

    fn many_caps(n: usize) -> Value {
        let caps: Vec<Value> = (0..n)
            .map(|i| {
                json!({
                    "id": format!("cap:{i}"),
                    "name": format!("capability-{i}"),
                    "description": "A long description that used to be printed in full on the \
directory card, which is what made one card taller than the screen while its neighbour was three \
lines high.",
                    "price": { "amount": 0.1 + i as f64, "currency": "USDC" }
                })
            })
            .collect();
        dir_with(json!([{
            "did": "did:gap:0123456789abcdef0123456789abcdef",
            "name": "Polymath", "score": 0.8, "n": 12, "jobs": 30,
            "capabilities": caps
        }]))
    }

    #[test]
    fn a_card_summarises_an_agent_instead_of_printing_its_catalogue() {
        // The reported problem: every capability was rendered with its
        // full description, so cards had wildly different heights and
        // comparing two agents meant scrolling past the offers of the
        // first.
        let html = directory(&many_caps(9));
        assert!(html.contains(">settled<"));
        assert!(
            html.contains(">30<"),
            "the settled count is a headline figure"
        );
        assert!(html.contains("0.80"));
        assert!(html.contains("score over 12"));
        assert!(
            html.contains(">9<"),
            "the capability count is a headline figure"
        );
        // Names survive, descriptions do not.
        assert!(html.contains("capability-0"));
        assert!(!html.contains("taller than the screen"));
        // And the overflow is admitted rather than silently dropped.
        assert!(html.contains("+5 more"));
    }

    #[test]
    fn the_cheapest_offer_is_labelled_as_the_cheapest() {
        // "from X" is the honest summary of a price list. Printing one
        // capability's price unlabelled would read as the agent's price.
        let html = directory(&many_caps(3));
        assert!(html.contains(">cheapest<"));
        assert!(html.contains("0.100000 USDC"));
        assert!(!html.contains("2.100000 USDC"), "not the dearest");
    }

    #[test]
    fn an_agent_with_no_capability_says_so_and_claims_no_price() {
        // A blank where a price goes reads as free.
        let d = dir_with(json!([{
            "did": "did:gap:aaa", "score": 0.5, "n": 0, "jobs": 0, "capabilities": []
        }]));
        let html = directory(&d);
        assert!(html.contains("No capability announced"));
        assert!(html.contains(">no price<"));
        assert!(!html.contains(">cheapest<"));
    }

    #[test]
    fn a_capability_name_on_a_card_links_to_its_page() {
        let html = directory(&many_caps(2));
        assert!(html.contains(r#"class="pill cap" href="/capability/cap:0""#));
    }

    #[test]
    fn a_hostile_capability_name_cannot_inject_markup_from_a_card() {
        let d = dir_with(json!([{
            "did": "did:gap:aaa", "score": 0.5, "n": 0, "jobs": 0,
            "capabilities": [{
                "id": "x", "name": "<script>alert(1)</script>",
                "price": { "amount": 1.0, "currency": "USD" }
            }]
        }]));
        let html = directory(&d);
        assert!(!html.contains("<script>alert(1)"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
