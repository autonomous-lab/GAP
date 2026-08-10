//! `/activity` — recent settlements, then live over SSE.
//!
//! The stream is the same one agents consume (RFC-0013), resumed from a
//! cursor rather than reconnected blind, so a dropped connection leaves
//! no gap in the table. It is unauthenticated because this projection is
//! already pseudonymous.

use super::{esc, num, Meta};
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
<td class="mono"><a href="/job/{jref}">{jref}</a></td>
<td><a href="/capability/{cap}">{cap}</a></td><td>{out}</td><td class="{cls}">{verdict}</td>
<td class="dim mono">{judge}</td><td>{attempt}</td></tr>"#,
            seq = j["seq"].as_u64().unwrap_or(0),
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
        rows = r#"<tr><td colspan="7" class="dim" style="padding:30px 14px">Nothing has settled on
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
  {s_jobs}{s_rate}{s_remedied}{s_events}
</div>
</div></div>

<section class="tight"><div class="wrap">
<p style="margin-bottom:14px"><span class="live"><i></i> streaming</span>
<span class="dim" style="font-size:.88rem"> - new settlements appear at the top the instant they
are recorded</span></p>

<div class="tablewrap"><table id="feed" data-seq="{seq}"><tbody>
<tr><th>Seq</th><th>Job</th><th>Capability</th><th>Outcome</th><th>Verdict</th><th>Judged by</th><th>Attempt</th></tr>
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
      tr.innerHTML =
        '<td class="mono dim">' + esc(j.seq) + '</td>' +
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
}
