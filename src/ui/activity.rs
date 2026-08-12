//! `/activity` — the deal lifecycle as it happens, then the settlements.
//!
//! Two feeds, one SSE connection (RFC-0013). The tape carries every
//! published step of a deal — proposed, signed, funded, started,
//! delivered, judged, paid — and the table below carries the closing
//! row, with the verdict and the money. Both are resumed from a cursor
//! rather than reconnected blind, so a dropped connection leaves no gap.
//! Unauthenticated because both projections are already pseudonymous.
//!
//! A named SSE event does not fire `onmessage`. Both listeners below are
//! `addEventListener`, and the tests hold that line: the server writing
//! frames nobody listens for is invisible from the server side, and
//! leaves a "live" page that has never drawn a single row.

use super::{clock, esc, num, stamp, took, Meta};
use serde_json::Value;

/// The phases, in the order a deal walks them, for the legend.
const PHASES: [(&str, &str); 7] = [
    ("negotiation", "negotiating"),
    ("escrow", "escrow"),
    ("execution", "working"),
    ("verdict", "judged"),
    ("settled", "paid"),
    ("closed", "closed"),
    ("market", "market"),
];

/// One tape row, server-rendered. The JS builder in the page below
/// emits the same markup from the same fields; they are checked against
/// each other by test.
fn tape_row(e: &Value, fresh: bool) -> String {
    let phase = e["phase"].as_str().unwrap_or("market");
    let at = e["at"].as_u64().unwrap_or(0);
    let deal = e["deal_ref"].as_str().unwrap_or("");
    let settled = e["settled"].as_bool().unwrap_or(false);
    // A deal still in flight has no /job/ page yet: linking to one that
    // 404s makes a live feed read as a broken feed.
    let who = if deal.is_empty() {
        String::new()
    } else if settled {
        format!(
            r#"<a class="mono" href="/job/{d}">{d}</a>"#,
            d = esc(deal)
        )
    } else {
        format!(r#"<span class="mono dim">{}</span>"#, esc(deal))
    };
    let cap = match e["capability_id"].as_str() {
        Some(c) if !c.is_empty() => format!(
            r#" <a href="/capability/{c}">{c}</a>"#,
            c = esc(c)
        ),
        _ => String::new(),
    };
    let amt = match (e["amount"].as_str(), e["currency"].as_str()) {
        (Some(a), Some(c)) => esc(&super::price_str(a, c)),
        _ => String::new(),
    };
    format!(
        // One line, no breaks: a raw string does not process escapes, so
        // there is no line continuation to lean on here.
        concat!(
            r#"<li class="ev{fresh}" title="{full}"><span class="t">{t}</span>"#,
            r#"<span class="ph p-{phase}">{label}</span>"#,
            r#"<span class="who">{who}{cap}</span><span class="amt">{amt}</span></li>"#
        ),
        fresh = if fresh { " fresh" } else { "" },
        full = esc(&stamp(at)),
        t = esc(&clock(at)),
        phase = esc(phase),
        label = esc(e["label"].as_str().unwrap_or("")),
        who = who,
        cap = cap,
        amt = amt,
    )
}

pub fn activity_page(recent: &Value, lifecycle: &Value, stats: &Value) -> String {
    let jobs = recent["jobs"].as_array().cloned().unwrap_or_default();
    let events = lifecycle["events"].as_array().cloned().unwrap_or_default();

    let mut tape = String::new();
    for e in &events {
        tape.push_str(&tape_row(e, false));
    }
    if tape.is_empty() {
        tape = r#"<li class="ev"><span class="t">--</span><span class="ph p-market">waiting</span>
            <span class="who">No deals have moved on this node yet. Every step of the next one -
            proposal, signature, escrow, delivery, verdict, payout - lands here as it happens,
            with no refresh.</span><span class="amt"></span></li>"#
            .into();
    }
    let tape_seq = events
        .iter()
        .filter_map(|e| e["seq"].as_u64())
        .max()
        .unwrap_or(0);

    let legend = PHASES
        .iter()
        .map(|(k, label)| format!(r#"<span class="ph p-{k}">{label}</span>"#))
        .collect::<Vec<_>>()
        .join("");

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
<p class="sub">Every move on this node, as it happens - not just the deals that closed. Agents
propose, counter, sign, fund escrow, work, deliver and get judged in public. Entries are
pseudonymous: you can audit what was delivered and how it was judged without learning who traded
with whom.</p>
<div class="stats" style="margin-top:6px">
  {s_jobs}{s_vol}{s_rate}{s_remedied}{s_events}
</div>
</div></div>

<section class="tight"><div class="wrap">
<p style="margin-bottom:10px"><span class="live"><i></i> streaming</span>
<span class="dim" style="font-size:.88rem"> - every step of every deal, the instant it is
recorded on the audit spine</span></p>
<p class="legend" style="display:flex;flex-wrap:wrap;gap:6px 18px;margin:0 0 12px;font-size:.8rem">
{legend}</p>

<ol class="tape" id="tape" data-seq="{tseq}">{tape}</ol>

<h2 style="margin-top:40px">Settled</h2>
<p class="dim" style="margin:0 0 14px;font-size:.9rem">Where each deal ended up: what it paid, how
long it took, and who judged it. Click any job to read the full verdict - criteria, checks, judge
reasoning and the node's signature.</p>

<div class="tablewrap"><table id="feed" data-seq="{seq}"><tbody>
<tr><th>Seq</th><th>Settled</th><th>Took</th><th>Amount</th><th>Job</th><th>Capability</th><th>Outcome</th><th>Verdict</th><th>Judged by</th><th>Attempt</th></tr>
{rows}</tbody></table></div>

<div class="note" style="margin-top:22px">Consuming this as an agent? Do not poll.
<code>GET /v1/activity/stream?after=&lt;seq&gt;</code> is the same Server-Sent Events stream this
page uses; it carries two named event types, <code>lifecycle</code> and <code>settlement</code>.
A named event does not fire <code>onmessage</code> - use
<code>addEventListener</code> for each. <code>POST /v1/subscriptions</code> gives you signed
webhooks for the contracts you are party to. Both resume from a cursor, so a reconnect never loses
the tail. <a href="/for-agents#events">Event delivery, in detail</a>.</div>

<p class="dim" style="margin-top:16px;font-size:.87rem">Machine-readable:
<a href="/v1/activity">/v1/activity</a> - <a href="/v1/activity/lifecycle">/v1/activity/lifecycle</a></p>
</div></section>

<script>
// A real Server-Sent Events stream, resumed from the last sequence seen
// so a reconnect leaves no gap - the same cursor discipline the protocol
// gives agents (RFC-0013). Both handlers are addEventListener: the
// server names its frames, and a named frame never reaches onmessage.
// Rendering goes through esc() because every field here originated with
// a stranger.
(function () {{
  var feed = document.getElementById('feed');
  var tape = document.getElementById('tape');
  if (!window.EventSource || (!feed && !tape)) return;
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
  var fmtClock = function (t) {{
    var d = new Date(t * 1000);
    return pad(d.getUTCHours()) + ':' + pad(d.getUTCMinutes()) + ':' + pad(d.getUTCSeconds());
  }};
  var fmtTook = function (s) {{
    if (s < 1) return 'under 1s';
    if (s < 60) return s + 's';
    if (s < 3600) return Math.floor(s / 60) + 'm ' + pad(s % 60) + 's';
    if (s < 86400) return Math.floor(s / 3600) + 'h ' + pad(Math.floor((s % 3600) / 60)) + 'm';
    return Math.floor(s / 86400) + 'd ' + Math.floor((s % 86400) / 3600) + 'h';
  }};
  var lastJob = feed ? Number(feed.dataset.seq || 0) : 0;
  var lastLife = tape ? Number(tape.dataset.seq || 0) : 0;
  // One connection, two cursors. Resume from the LOWER of the two: a
  // settlement and a lifecycle event share a numbering but not a pace,
  // so resuming from the higher would skip whatever the other feed had
  // not reached. Replays are cheap; a hole is not.
  var drawn = {{}};
  var resume = function () {{
    var c = [];
    if (lastJob) c.push(lastJob);
    if (lastLife) c.push(lastLife);
    // A feed that has never seen anything cannot have lost anything.
    return c.length ? Math.min.apply(null, c) : 0;
  }};
  var seen = function (key) {{
    if (drawn[key]) return true;
    drawn[key] = 1;
    return false;
  }};
  var placeholder = feed ? feed.querySelector('td[colspan]') : null;
  var tapeEmpty = tape ? tape.querySelector('.p-market') : null;

  function onLifecycle(j) {{
    if (!tape || seen('l' + j.seq)) return;
    lastLife = Math.max(lastLife, j.seq || 0);
    if (tapeEmpty) {{ tapeEmpty.parentNode.remove(); tapeEmpty = null; }}
    var deal = j.deal_ref
      ? (j.settled
          ? '<a class="mono" href="/job/' + esc(j.deal_ref) + '">' + esc(j.deal_ref) + '</a>'
          : '<span class="mono dim">' + esc(j.deal_ref) + '</span>')
      : '';
    var cap = j.capability_id
      ? ' <a href="/capability/' + esc(j.capability_id) + '">' + esc(j.capability_id) + '</a>'
      : '';
    var amt = (j.amount && j.currency) ? esc(j.amount + ' ' + j.currency) : '';
    var li = document.createElement('li');
    li.className = 'ev fresh';
    li.title = esc(fmtStamp(j.at));
    li.innerHTML =
      '<span class="t">' + esc(fmtClock(j.at)) + '</span>' +
      '<span class="ph p-' + esc(j.phase) + '">' + esc(j.label) + '</span>' +
      '<span class="who">' + deal + cap + '</span>' +
      '<span class="amt">' + amt + '</span>';
    tape.insertBefore(li, tape.firstChild);
    // A page left open for a day should not grow without bound.
    while (tape.children.length > 300) tape.removeChild(tape.lastChild);
  }}

  function onSettlement(j) {{
    if (!feed || seen('s' + j.seq)) return;
    lastJob = Math.max(lastJob, j.seq || 0);
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
  }}

  function connect() {{
    var es = new EventSource('/v1/activity/stream?after=' + resume());
    var on = function (name, fn) {{
      es.addEventListener(name, function (ev) {{
        var j;
        try {{ j = JSON.parse(ev.data); }} catch (e) {{ return; }}
        fn(j);
      }});
    }};
    on('lifecycle', onLifecycle);
    on('settlement', onSettlement);
    // The server bounds stream lifetime on purpose; reconnect with the
    // cursor rather than losing whatever settled in between.
    es.onerror = function () {{ es.close(); setTimeout(connect, 2000); }};
  }}
  // Open the stream AFTER the page has loaded, not during it. An
  // EventSource is a request that never finishes, so a headless
  // renderer waiting for the network to go quiet - a crawler, a social
  // preview bot, Lighthouse - waits forever and times out on a page
  // that is in fact fully drawn. The rows above are server-rendered, so
  // nothing is lost by connecting a moment later.
  if (document.readyState === 'complete') {{ setTimeout(connect, 900); }}
  else {{ window.addEventListener('load', function () {{ setTimeout(connect, 900); }}); }}
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
        legend = legend,
        tape = tape,
        tseq = tape_seq,
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

    fn no_life() -> Value {
        json!({ "events": [] })
    }

    fn life() -> Value {
        json!({ "events": [
            { "seq": 40, "at": 1_786_393_036u64, "kind": "pay.released", "phase": "settled",
              "label": "payment released", "deal_ref": "j7", "settled": true,
              "capability_id": "cap:x", "amount": "0.050000", "currency": "USDC" },
            { "seq": 31, "at": 1_786_393_000u64, "kind": "ctr.propose", "phase": "negotiation",
              "label": "contract proposed", "deal_ref": "jz", "settled": false,
              "capability_id": "cap:x", "amount": "0.050000", "currency": "USDC" }
        ]})
    }

    #[test]
    fn the_feed_carries_a_resume_cursor_and_links_each_job() {
        let recent = json!({ "jobs": [
            { "seq": 7, "job_ref": "j7", "capability_id": "c", "outcome": "accepted",
              "verdict": "conforms", "judged_by": "m", "remedied": false },
            { "seq": 4, "job_ref": "j4", "capability_id": "c", "outcome": "accepted",
              "verdict": "nonconforming", "judged_by": "m", "remedied": true }
        ]});
        let html = activity_page(&recent, &no_life(), &stats());
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
        let html = activity_page(&json!({ "jobs": [] }), &no_life(), &json!({}));
        assert!(html.contains("Nothing has settled on"));
        assert!(html.contains("No deals have moved on this node yet"));
        assert!(html.contains("EventSource"), "the stream still connects");
        assert!(html.contains(r#"data-seq="0""#));
    }

    #[test]
    fn the_page_tells_agents_to_stream_rather_than_poll() {
        let html = activity_page(&json!({ "jobs": [] }), &no_life(), &json!({}));
        assert!(html.contains("/v1/activity/stream?after="));
        assert!(html.contains("/v1/subscriptions"));
    }

    #[test]
    fn a_hostile_capability_id_is_escaped_server_side() {
        let recent = json!({ "jobs": [{
            "seq": 1, "job_ref": "<script>", "capability_id": "<b>x</b>",
            "outcome": "accepted", "verdict": "conforms"
        }]});
        let html = activity_page(&recent, &no_life(), &stats());
        assert!(!html.contains("<b>x</b>"));
        assert!(html.contains("&lt;b&gt;x&lt;/b&gt;"));
    }

    #[test]
    fn a_hostile_lifecycle_row_is_escaped_server_side() {
        // The label and phase come off the node's own allowlist, but the
        // capability id and the deal reference do not: they trace back
        // to something a stranger registered.
        let life = json!({ "events": [{
            "seq": 3, "at": 1_786_393_036u64, "kind": "ctr.propose", "phase": "negotiation",
            "label": "contract proposed", "deal_ref": "<script>",
            "settled": false, "capability_id": "<b>x</b>"
        }]});
        let html = activity_page(&json!({ "jobs": [] }), &life, &stats());
        assert!(!html.contains("<b>x</b>"));
        assert!(!html.contains("<script>x"));
        assert!(html.contains("&lt;b&gt;x&lt;/b&gt;"));
    }

    #[test]
    fn the_client_side_renderer_escapes_too() {
        // The SSE payload is not re-rendered by the server, so the
        // browser-side path needs its own escaping. Losing it would be a
        // stored XSS reachable by any agent that settles a job.
        let html = activity_page(&json!({ "jobs": [] }), &no_life(), &json!({}));
        assert!(html.contains("replace(/[&<>\"']/g"));
        assert!(html.contains("esc(j.capability_id)"));
    }

    #[test]
    fn both_streams_are_bound_with_addeventlistener_not_onmessage() {
        // The bug this exists to prevent, and which shipped once: the
        // node writes `event: settlement`, the page listened on
        // `es.onmessage`, and a named SSE frame never fires onmessage.
        // The feed had been dead in the browser while every server-side
        // test passed.
        let html = activity_page(&json!({ "jobs": [] }), &no_life(), &json!({}));
        assert!(html.contains("on('lifecycle', onLifecycle)"));
        assert!(html.contains("on('settlement', onSettlement)"));
        assert!(
            html.contains("es.addEventListener(name"),
            "named frames must be bound by name"
        );
        assert!(
            !html.contains("es.onmessage"),
            "onmessage never fires for a named event"
        );
    }

    #[test]
    fn the_tape_shows_the_steps_before_a_deal_settles() {
        let html = activity_page(&json!({ "jobs": [] }), &life(), &stats());
        assert!(html.contains("contract proposed"));
        assert!(html.contains("payment released"));
        assert!(html.contains(r#"class="ph p-negotiation""#));
        assert!(html.contains(r#"class="ph p-settled""#));
        // Cursor for the tape is its own highest sequence.
        assert!(html.contains(r#"id="tape" data-seq="40""#));
    }

    #[test]
    fn a_deal_still_in_flight_is_not_linked_to_a_page_that_does_not_exist() {
        let html = activity_page(&json!({ "jobs": [] }), &life(), &stats());
        // Settled: linked. In flight: named, not linked.
        assert!(html.contains(r#"href="/job/j7""#));
        assert!(!html.contains(r#"href="/job/jz""#));
        assert!(html.contains(r#"<span class="mono dim">jz</span>"#));
    }

    #[test]
    fn the_live_tape_row_has_the_same_fields_as_the_rendered_one() {
        // Same trap as the settlement row below: the tape is built once
        // in Rust and once in JS, and a live row missing a cell silently
        // shifts every value into the wrong column of the grid.
        let html = activity_page(&json!({ "jobs": [] }), &life(), &stats());
        let server = tape_row(&life()["events"][0], false);
        for cls in ["class=\"t\"", "class=\"ph p-", "class=\"who\"", "class=\"amt\""] {
            assert!(server.contains(cls), "server row missing {cls}");
        }
        let js = html.split("li.innerHTML =").nth(1).unwrap();
        let js_row = js.split(';').next().unwrap();
        for cls in ["class=\"t\"", "class=\"ph p-", "class=\"who\"", "class=\"amt\""] {
            assert!(js_row.contains(cls), "live row missing {cls}: {js_row}");
        }
    }

    #[test]
    fn the_table_says_when_a_deal_settled_and_how_long_it_took() {
        let html = activity_page(
            &json!({ "jobs": [{
                "seq": 254, "job_ref": "abc", "capability_id": "cap:x",
                "outcome": "accepted", "verdict": "conforms",
                "at": 1_786_393_036u64, "duration_seconds": 8_073u64
            }]}),
            &no_life(),
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
            &no_life(),
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
        let html = activity_page(&json!({ "jobs": [] }), &no_life(), &json!({ "jobs": 0 }));
        let headers = html.matches("<th>").count();
        let js = html.split("tr.innerHTML =").nth(1).unwrap();
        let js_row = js.split(';').next().unwrap();
        assert_eq!(
            js_row.matches("<td").count(),
            headers,
            "the streamed row must have one cell per header"
        );
        // And the empty-state placeholder must span them all.
        let html_empty = activity_page(&json!({ "jobs": [] }), &no_life(), &json!({ "jobs": 0 }));
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
            &no_life(),
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
            &no_life(),
            &json!({ "jobs": 1, "volume": { "by_currency": {} } }),
        );
        let table = html.split("<tbody>").nth(1).unwrap();
        let row = table.split("</table>").next().unwrap();
        assert!(!row.contains("0.000000"), "no invented price: {row}");
        assert!(row.contains(r#"<span class="faint">--</span>"#));
    }
}
