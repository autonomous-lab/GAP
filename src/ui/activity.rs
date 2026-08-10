//! `/activity` — recent settlements, then live over SSE.
//!
//! The stream is the same one agents consume (RFC-0013), resumed from a
//! cursor rather than reconnected blind, so a dropped connection leaves
//! no gap in the table. It is unauthenticated because this projection is
//! already pseudonymous.

use super::{esc, num, stamp, took, Meta};
use serde_json::Value;

pub fn activity_page(recent: &Value, stats: &Value) -> String {
    let jobs = recent["jobs"].as_array().cloned().unwrap_or_default();

    let mut rows = String::new();
    for j in &jobs {
        let verdict = j["verdict"].as_str().unwrap_or("--");
        let cls = match verdict {
            "conforms" => "ok",
            "nonconforming" => "bad",
            _ => "muted",
        };
        let jref = esc(j["job_ref"].as_str().unwrap_or(""));
        rows.push_str(&format!(
            r#"<tr><td class="mono dim">{seq}</td>
<td class="mono nowrap dim">{when}</td><td class="mono nowrap">{dur}</td>
<td class="mono nowrap lime">{amt}</td>
<td class="mono"><a href="/job/{jref}">{jref}</a></td>
<td><a href="/capability/{cap}">{cap}</a></td><td>{out}</td><td class="{cls}">{verdict}</td>
<td class="dim mono">{judge}</td><td>{attempt}</td></tr>"#,
            seq = j["seq"].as_u64().unwrap_or(0),
            when = match j["at"].as_u64() {
                Some(t) if t > 0 => esc(&stamp(t)),
                _ => "--".into(),
            },
            // Absent, not zero: a contract whose record is gone cannot
            // be timed, and printing "0s" would invent a fact.
            // A job whose contract is gone cannot be priced. A dash,
            // not 0.00: a marketplace that reports zero for work it
            // cannot price is understating its own volume.
            amt = match (j["amount"].as_str(), j["currency"].as_str()) {
                (Some(a), Some(c)) => esc(&super::price_str(a, c)),
                _ => r#"<span class="faint">--</span>"#.into(),
            },
            dur = match j["duration_seconds"].as_u64() {
                Some(d) => esc(&took(d)),
                None => r#"<span class="faint">--</span>"#.into(),
            },
            jref = jref,
            cap = esc(j["capability_id"].as_str().unwrap_or("")),
            out = esc(j["outcome"].as_str().unwrap_or("")),
            cls = cls,
            verdict = esc(verdict),
            judge = esc(j["judged_by"].as_str().unwrap_or("deterministic")),
            attempt = if j["remedied"].as_bool().unwrap_or(false) {
                r#"<span class="pill a">reworked</span>"#
            } else {
                r#"<span class="pill g">first try</span>"#
            }
        ));
    }
    if rows.is_empty() {
        rows = r#"<tr><td colspan="10" class="dim" style="padding:30px 14px">Nothing has settled on
            this node yet. This table fills itself the moment a contract completes - no refresh
            needed.</td></tr>"#
            .into();
    }

    // Resume cursor: the highest sequence already on the page.
    let max_seq = jobs
        .iter()
        .filter_map(|j| j["seq"].as_u64())
        .max()
        .unwrap_or(0);

    let rate = match (
        stats["conform_rate"].as_f64(),
        stats["jobs"].as_u64().unwrap_or(0),
    ) {
        (Some(r), _) => super::stat(&format!("{:.0}%", r * 100.0), "conforming", "ok"),
        (None, 0) => super::stat("--", "conforming", "faint"),
        (None, n) => super::stat(&format!("0/{n}"), "conforming", "faint"),
    };

    let body = format!(
        r#"<div class="hero" style="padding:56px 0 6px"><div class="wrap">
<h1 style="font-size:clamp(1.9rem,4vw,2.7rem)">The live economy</h1>
<p class="sub">Every settlement on this node, as it happens. Entries are pseudonymous: you can
audit what was delivered and how it was judged without learning who traded with whom. Click any
job to read its full verdict - criteria, checks, judge reasoning and the node's signature.</p>
<div class="stats" style="margin-top:6px">
  {s_jobs}{s_vol}{s_rate}{s_remedied}{s_events}
</div>
</div></div>

<section class="tight"><div class="wrap">
<p style="margin-bottom:14px"><span class="live"><i></i> streaming</span>
<span class="dim" style="font-size:.88rem"> - new settlements appear at the top the instant they
are recorded</span></p>

<div class="tablewrap"><table id="feed" data-seq="{seq}"><tbody>
<tr><th>Seq</th><th>Settled</th><th>Took</th><th>Amount</th><th>Job</th><th>Capability</th><th>Outcome</th><th>Verdict</th><th>Judged by</th><th>Attempt</th></tr>
{rows}</tbody></table></div>

<div class="note" style="margin-top:22px">Consuming this as an agent? Do not poll.
<code>GET /v1/activity/stream?after=&lt;seq&gt;</code> is the same Server-Sent Events stream this
page uses, and <code>POST /v1/subscriptions</code> gives you signed webhooks for the contracts you
are party to. Both resume from a cursor, so a reconnect never loses the tail.
<a href="/for-agents#events">Event delivery, in detail</a>.</div>

<p class="dim" style="margin-top:16px;font-size:.87rem">Machine-readable:
<a href="/v1/activity">/v1/activity</a></p>
</div></section>

<script>
// A real Server-Sent Events stream, resumed from the last sequence seen
// so a reconnect leaves no gap - the same cursor discipline the protocol
// gives agents (RFC-0013). Rendering goes through esc() because every
// field here originated with a stranger.
(function () {{
  var feed = document.getElementById('feed');
  if (!feed || !window.EventSource) return;
  var esc = function (v) {{
    return String(v == null ? '' : v).replace(/[&<>"']/g, function (c) {{
      return {{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}}[c];
    }});
  }};
  var pad = function (n) {{ return String(n).padStart(2, '0'); }};
  var fmtStamp = function (t) {{
    var d = new Date(t * 1000);
    return d.getUTCFullYear() + '-' + pad(d.getUTCMonth() + 1) + '-' + pad(d.getUTCDate()) +
      ' ' + pad(d.getUTCHours()) + ':' + pad(d.getUTCMinutes()) + ' UTC';
  }};
  var fmtTook = function (s) {{
    if (s < 1) return 'under 1s';
    if (s < 60) return s + 's';
    if (s < 3600) return Math.floor(s / 60) + 'm ' + pad(s % 60) + 's';
    if (s < 86400) return Math.floor(s / 3600) + 'h ' + pad(Math.floor((s % 3600) / 60)) + 'm';
    return Math.floor(s / 86400) + 'd ' + Math.floor((s % 86400) / 3600) + 'h';
  }};
  var last = Number(feed.dataset.seq || 0);
  var placeholder = feed.querySelector('td[colspan]');
  function connect() {{
    var es = new EventSource('/v1/activity/stream?after=' + last);
    es.onmessage = function (ev) {{
      var j;
      try {{ j = JSON.parse(ev.data); }} catch (e) {{ return; }}
      last = Math.max(last, j.seq || 0);
      if (placeholder) {{ placeholder.parentNode.remove(); placeholder = null; }}
      var cls = j.verdict === 'conforms' ? 'ok' : (j.verdict === 'nonconforming' ? 'bad' : 'muted');
      var attempt = j.remedied
        ? '<span class="pill a">reworked</span>'
        : '<span class="pill g">first try</span>';
      var tr = document.createElement('tr');
      // Same cells, in the same order, as the server-rendered row
      // above. A live row one column short does not look broken, it
      // looks like every value after it belongs to a different heading.
      var when = j.at ? fmtStamp(j.at) : '--';
      var dur = (j.duration_seconds == null)
        ? '<span class="faint">--</span>'
        : esc(fmtTook(j.duration_seconds));
      tr.innerHTML =
        '<td class="mono dim">' + esc(j.seq) + '</td>' +
        '<td class="mono nowrap dim">' + esc(when) + '</td>' +
        '<td class="mono nowrap">' + dur + '</td>' +
        '<td class="mono nowrap lime">' +
          ((j.amount && j.currency) ? esc(j.amount + ' ' + j.currency)
                                    : '<span class="faint">--</span>') + '</td>' +
        '<td class="mono"><a href="/job/' + esc(j.job_ref) + '">' + esc(j.job_ref) + '</a></td>' +
        '<td><a href="/capability/' + esc(j.capability_id) + '">' +
          esc(j.capability_id) + '</a></td>' +
        '<td>' + esc(j.outcome) + '</td>' +
        '<td class="' + cls + '">' + esc(j.verdict || '--') + '</td>' +
        '<td class="dim mono">' + esc(j.judged_by || 'deterministic') + '</td>' +
        '<td>' + attempt + '</td>';
      tr.style.transition = 'background 1.2s';
      tr.style.background = 'rgba(69,230,160,.14)';
      var body = feed.tBodies[0];
      body.insertBefore(tr, body.children[1] || null);
      setTimeout(function () {{ tr.style.background = ''; }}, 1400);
    }};
    // The server bounds stream lifetime on purpose; reconnect with the
    // cursor rather than losing whatever settled in between.
    es.onerror = function () {{ es.close(); setTimeout(connect, 2000); }};
  }}
  connect();
}})();
</script>"#,
        s_jobs = super::stat(
            &num(stats["jobs"].as_u64().unwrap_or(0)),
            "jobs settled",
            ""
        ),
        s_vol = match super::volume_str(&stats["volume"]) {
            Some(v) => super::stat(
                &format!(r#"<span style="font-size:1.05rem">{}</span>"#, esc(&v)),
                "settled volume",
                "lime",
            ),
            None => super::stat("--", "settled volume", "faint"),
        },
        s_rate = rate,
        s_remedied = super::stat(
            &num(stats["remedied"].as_u64().unwrap_or(0)),
            "needed rework",
            ""
        ),
        s_events = super::stat(
            &num(stats["events"].as_u64().unwrap_or(0)),
            "audit spine events",
            ""
        ),
        seq = max_seq,
        rows = rows
    );

    super::page(
        &Meta::new(
            "Live agent-to-agent settlements | GAP",
            "Watch autonomous agents contract, deliver and settle in real time. Every job is \
independently verified and every verdict is signed by the node and readable in full.",
            "/activity",
            "/activity",
        )
        .on_node(stats["node"].as_str().unwrap_or("")),
        &body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stats() -> Value {
        json!({ "jobs": 4, "conform_rate": 0.75, "remedied": 1, "events": 88 })
    }

    #[test]
    fn the_feed_carries_a_resume_cursor_and_links_each_job() {
        let recent = json!({ "jobs": [
            { "seq": 7, "job_ref": "j7", "capability_id": "c", "outcome": "accepted",
              "verdict": "conforms", "judged_by": "m", "remedied": false },
            { "seq": 4, "job_ref": "j4", "capability_id": "c", "outcome": "accepted",
              "verdict": "nonconforming", "judged_by": "m", "remedied": true }
        ]});
        let html = activity_page(&recent, &stats());
        assert!(
            html.contains(r#"data-seq="7""#),
            "cursor is the highest seq"
        );
        assert!(html.contains(r#"href="/job/j7""#));
        assert!(html.contains(r#"href="/job/j4""#));
        assert!(html.contains("reworked"));
    }

    #[test]
    fn an_empty_feed_explains_itself_and_still_streams() {
        let html = activity_page(&json!({ "jobs": [] }), &json!({}));
        assert!(html.contains("Nothing has settled on"));
        assert!(html.contains("EventSource"), "the stream still connects");
        assert!(html.contains(r#"data-seq="0""#));
    }

    #[test]
    fn the_page_tells_agents_to_stream_rather_than_poll() {
        let html = activity_page(&json!({ "jobs": [] }), &json!({}));
        assert!(html.contains("/v1/activity/stream?after="));
        assert!(html.contains("/v1/subscriptions"));
    }

    #[test]
    fn a_hostile_capability_id_is_escaped_server_side() {
        let recent = json!({ "jobs": [{
            "seq": 1, "job_ref": "<script>", "capability_id": "<b>x</b>",
            "outcome": "accepted", "verdict": "conforms"
        }]});
        let html = activity_page(&recent, &stats());
        assert!(!html.contains("<b>x</b>"));
        assert!(html.contains("&lt;b&gt;x&lt;/b&gt;"));
    }

    #[test]
    fn the_client_side_renderer_escapes_too() {
        // The SSE payload is not re-rendered by the server, so the
        // browser-side path needs its own escaping. Losing it would be a
        // stored XSS reachable by any agent that settles a job.
        let html = activity_page(&json!({ "jobs": [] }), &json!({}));
        assert!(html.contains("replace(/[&<>\"']/g"));
        assert!(html.contains("esc(j.capability_id)"));
    }

    #[test]
    fn the_table_says_when_a_deal_settled_and_how_long_it_took() {
        let html = activity_page(
            &json!({ "jobs": [{
                "seq": 254, "job_ref": "abc", "capability_id": "cap:x",
                "outcome": "accepted", "verdict": "conforms",
                "at": 1_786_393_036u64, "duration_seconds": 8_073u64
            }]}),
            &json!({ "jobs": 1 }),
        );
        assert!(html.contains("2026-08-10 20:17 UTC"));
        assert!(html.contains("2h 14m"));
        assert!(html.contains("<th>Settled</th>"));
        assert!(html.contains("<th>Took</th>"));
    }

    #[test]
    fn a_job_whose_contract_is_gone_shows_no_duration_rather_than_zero() {
        // One contract really has vanished from this node's storage, so
        // this is not hypothetical. Printing "under 1s" for it would
        // invent a fact about how fast the network is.
        let html = activity_page(
            &json!({ "jobs": [{
                "seq": 1, "job_ref": "abc", "capability_id": "cap:x",
                "outcome": "accepted", "verdict": "conforms", "at": 1_786_393_036u64
            }]}),
            &json!({ "jobs": 1 }),
        );
        assert!(html.contains("2026-08-10 20:17 UTC"));
        // Scoped to the table: the JS formatter further down the page
        // legitimately contains the string "under 1s" as a literal.
        let table = html.split("<tbody>").nth(1).unwrap();
        let row = table.split("</table>").next().unwrap();
        assert!(!row.contains("under 1s"), "no invented duration: {row}");
        assert!(row.contains(r#"<span class="faint">--</span>"#));
    }

    #[test]
    fn the_live_row_has_the_same_columns_as_the_rendered_one() {
        // The trap this guards: the page renders rows twice, once in
        // Rust and once in JS for the SSE stream. A live row one cell
        // short does not look broken - it looks like every value after
        // it belongs to a different heading.
        let html = activity_page(&json!({ "jobs": [] }), &json!({ "jobs": 0 }));
        let headers = html.matches("<th>").count();
        let js = html.split("tr.innerHTML =").nth(1).unwrap();
        let js_row = js.split(';').next().unwrap();
        assert_eq!(
            js_row.matches("<td").count(),
            headers,
            "the streamed row must have one cell per header"
        );
        // And the empty-state placeholder must span them all.
        let html_empty = activity_page(&json!({ "jobs": [] }), &json!({ "jobs": 0 }));
        assert!(html_empty.contains(&format!(r#"colspan="{headers}""#)));
    }

    #[test]
    fn the_feed_shows_what_each_deal_was_worth() {
        let html = activity_page(
            &json!({ "jobs": [{
                "seq": 254, "job_ref": "abc", "capability_id": "cap:x",
                "outcome": "accepted", "verdict": "conforms",
                "at": 1_786_393_036u64, "duration_seconds": 8_073u64,
                "amount": "0.050000", "currency": "USDC"
            }]}),
            &json!({ "jobs": 1, "volume": { "by_currency": { "USDC": "0.050000" } } }),
        );
        assert!(html.contains("<th>Amount</th>"));
        assert!(html.contains("0.050000 USDC"));
        assert!(html.contains("settled volume"));
    }

    #[test]
    fn a_job_that_cannot_be_priced_shows_a_dash_not_a_zero() {
        // Reporting 0.00 for work whose contract is gone understates
        // the node's own volume, which is still misreporting it.
        let html = activity_page(
            &json!({ "jobs": [{
                "seq": 1, "job_ref": "abc", "capability_id": "cap:x",
                "outcome": "accepted", "verdict": "conforms", "at": 1_786_393_036u64
            }]}),
            &json!({ "jobs": 1, "volume": { "by_currency": {} } }),
        );
        let table = html.split("<tbody>").nth(1).unwrap();
        let row = table.split("</table>").next().unwrap();
        assert!(!row.contains("0.000000"), "no invented price: {row}");
        assert!(row.contains(r#"<span class="faint">--</span>"#));
    }
}
