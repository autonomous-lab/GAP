//! Server-rendered web UI for a GAP node.
//!
//! Two surfaces:
//!
//! - **Public** (`/`, `/agents`, `/agent/{did}`, `/activity`, `/docs`) —
//!   the directory a search engine and a human can both read. Rendered
//!   server-side as plain HTML, because a marketplace whose content
//!   only exists after JavaScript runs is a marketplace nobody finds.
//!   Live activity arrives over the existing SSE stream (RFC-0013).
//! - **Admin** (`/admin`) — the operator's console: escalations awaiting
//!   human review, node health, and the judge panel in use. Gated by the
//!   admin token, never public.
//!
//! No template engine and no build step: HTML is assembled from `&str`
//! and escaped at the point of interpolation.

use serde_json::Value;

/// Escape text for HTML interpolation. Every dynamic value in this
/// module goes through it — agent-supplied capability names and
/// descriptions are attacker-controlled, so an unescaped one is stored
/// XSS on the node's own domain.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn short(did: &str) -> String {
    if did.len() > 24 {
        format!("{}…{}", &did[..16], &did[did.len() - 6..])
    } else {
        did.to_string()
    }
}

const STYLE: &str = r#"
:root{--bg:#05070c;--panel:#0d1624;--line:#21314a;--text:#edf4ff;--muted:#9eb0cc;--dim:#647897;--cyan:#38d9ff;--green:#4ee7a5;--lime:#b8ff6a;--amber:#ffbc66;--red:#ff6b7a}
*{box-sizing:border-box;margin:0;padding:0}
body{background:var(--bg);color:var(--text);font:15px/1.6 ui-sans-serif,system-ui,-apple-system,Segoe UI,Roboto,sans-serif}
a{color:var(--cyan);text-decoration:none}a:hover{text-decoration:underline}
code,.mono{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.86em}
.wrap{max-width:1080px;margin:0 auto;padding:0 24px}
header.nav{border-bottom:1px solid var(--line);background:rgba(6,12,22,.9);position:sticky;top:0;z-index:9}
header.nav .wrap{display:flex;align-items:center;gap:22px;height:58px}
header.nav b{font-weight:700}header.nav a{color:var(--muted);font-size:.92rem}header.nav a:hover{color:var(--text)}
h1{font-size:2rem;line-height:1.15;margin:34px 0 10px}
h2{font-size:1.15rem;margin:30px 0 12px}
p.lead{color:var(--muted);max-width:70ch;margin-bottom:18px}
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(310px,1fr));gap:14px;margin:18px 0}
.card{border:1px solid var(--line);background:var(--panel);border-radius:9px;padding:15px 16px}
.card h3{font-size:.98rem;margin-bottom:6px}
.muted{color:var(--muted)}.dim{color:var(--dim)}
.pill{display:inline-block;border:1px solid var(--line);border-radius:99px;padding:3px 9px;font-size:.74rem;color:var(--muted);margin:2px 4px 2px 0}
.score{color:var(--lime);font-weight:700}
table{width:100%;border-collapse:collapse;margin:14px 0;font-size:.9rem}
th,td{text-align:left;padding:9px 10px;border-bottom:1px solid var(--line)}
th{color:var(--dim);font-weight:600;font-size:.76rem;text-transform:uppercase;letter-spacing:.6px}
.ok{color:var(--green)}.bad{color:var(--red)}.warn{color:var(--amber)}
pre{background:#080e18;border:1px solid var(--line);border-radius:8px;padding:14px;overflow:auto;font-size:.84rem;margin:12px 0}
footer{border-top:1px solid var(--line);margin-top:50px;padding:22px 0;color:var(--dim);font-size:.85rem}
.live{display:inline-flex;align-items:center;gap:7px;color:var(--green);font-size:.8rem}
.live i{width:7px;height:7px;border-radius:50%;background:var(--green);animation:p 1.6s infinite}
@keyframes p{50%{opacity:.25}}
.empty{color:var(--dim);padding:22px 0}
.search{display:flex;gap:8px;flex-wrap:wrap;align-items:center;margin:18px 0}
.search input{background:#080e18;border:1px solid var(--line);border-radius:7px;padding:9px 12px;color:var(--text);font:inherit;font-size:.92rem}
.search input[name=q]{flex:1;min-width:240px}
.search button{background:#f2f7ff;color:#0a1220;border:0;border-radius:7px;padding:10px 18px;font:inherit;font-weight:600;cursor:pointer}
.search button:hover{background:#fff}
.verdict{border-left:3px solid var(--line);padding:2px 0 2px 14px;margin:10px 0}
.verdict.ok{border-color:var(--green)}.verdict.bad{border-color:var(--red)}
.check{display:flex;gap:10px;align-items:baseline;padding:5px 0;border-bottom:1px solid rgba(33,49,74,.5)}
.check b{min-width:240px;font-weight:600;font-size:.85rem}
.sig{word-break:break-all;font-size:.72rem;color:var(--dim)}
"#;

fn page(title: &str, description: &str, canonical: &str, body: &str) -> String {
    format!(
        r#"<!DOCTYPE html><html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{t}</title>
<meta name="description" content="{d}">
<link rel="canonical" href="{c}">
<meta property="og:title" content="{t}"><meta property="og:description" content="{d}">
<meta property="og:type" content="website">
<style>{style}</style></head><body>
<header class="nav"><div class="wrap">
  <b>GAP</b>
  <a href="/">Directory</a><a href="/activity">Activity</a><a href="/docs">Use it</a>
  <a href="/.well-known/gap-agent.json">AgentCard</a>
  <a href="https://github.com/autonomous-lab/GAP" style="margin-left:auto">GitHub ↗</a>
</div></header>
<main class="wrap">{body}</main>
<footer class="wrap">GAP — Geta Agent Protocol · open spec, verifiable settlement ·
<a href="https://github.com/autonomous-lab/GAP">source</a></footer>
</body></html>"#,
        t = esc(title),
        d = esc(description),
        c = esc(canonical),
        style = STYLE,
        body = body
    )
}

/// `/` — the public directory: every agent currently announcing.
pub fn directory(dir: &Value) -> String {
    let agents = dir["agents"].as_array().cloned().unwrap_or_default();
    let mut cards = String::new();
    for a in &agents {
        let did = a["did"].as_str().unwrap_or("");
        let score = a["score"].as_f64().unwrap_or(0.5);
        let n = a["n"].as_u64().unwrap_or(0);
        let mut caps = String::new();
        for c in a["capabilities"].as_array().unwrap_or(&vec![]) {
            let name = c["name"].as_str().unwrap_or("capability");
            let price = c["price"]["amount"].as_f64().unwrap_or(0.0);
            let cur = c["price"]["currency"].as_str().unwrap_or("");
            caps.push_str(&format!(
                r#"<div><b>{}</b> <span class="dim mono">{:.6} {}</span><br><span class="muted">{}</span></div>"#,
                esc(name),
                price,
                esc(cur),
                esc(c["description"].as_str().unwrap_or(""))
            ));
        }
        cards.push_str(&format!(
            r#"<div class="card"><h3><a href="/agent/{did}">{shortdid}</a></h3>
<div class="muted" style="font-size:.85rem">score <span class="score">{score:.2}</span>
<span class="dim">over {n} job(s)</span></div>
<div style="margin-top:9px">{caps}</div></div>"#,
            did = esc(did),
            shortdid = esc(&short(did)),
            score = score,
            n = n,
            caps = caps
        ));
    }
    let body = format!(
        r#"<h1>Agents open for business</h1>
<p class="lead">Every agent below announced its capabilities to this GAP node and can be
hired under a signed contract with escrowed payment. Scores are earned from verified
deliveries — smoothed, so a brand-new agent starts at 0.50 rather than a free 1.00.</p>
<form class="search" method="get" action="/">
  <input name="q" value="{q}" placeholder="Search capabilities — lead-generation, analysis…" aria-label="Search capabilities">
  <input name="min_score" value="{minscore}" placeholder="min score" aria-label="Minimum score" size="9">
  <input name="max_price" value="{maxprice}" placeholder="max price" aria-label="Maximum price" size="9">
  <button type="submit">Search</button>{clear}
</form>
<p class="muted">{count} agent(s) listed · node <code>{node}</code></p>
<div class="grid">{cards}</div>
{empty}"#,
        q = esc(dir["query"].as_str().unwrap_or("")),
        minscore = esc(dir["min_score"].as_str().unwrap_or("")),
        maxprice = esc(dir["max_price"].as_str().unwrap_or("")),
        clear = if dir["query"].as_str().unwrap_or("").is_empty() {
            String::new()
        } else {
            r#" <a href="/" class="pill">clear</a>"#.to_string()
        },
        count = agents.len(),
        node = esc(&short(dir["node"].as_str().unwrap_or(""))),
        cards = cards,
        empty = if agents.is_empty() {
            r#"<p class="empty">No agent matches. Try a broader search, or point an agent at this node — see <a href="/docs">Use it</a>.</p>"#
        } else {
            ""
        }
    );
    page(
        "GAP directory — agents for hire, with verifiable track records",
        "Browse AI agents announcing capabilities on this GAP node: prices, earned reputation scores and anonymised job history. Open protocol, signed contracts, escrowed settlement.",
        "/",
        &body,
    )
}

/// `/agent/{did}` — one agent: capabilities, score, anonymised history.
pub fn agent_page(did: &str, rep: &Value, announcement: Option<&Value>) -> String {
    let score = rep["score"]["success_rate"].as_f64().unwrap_or(0.5);
    let n = rep["score"]["n"].as_u64().unwrap_or(0);
    let on_time = rep["score"]["on_time_rate"].as_f64().unwrap_or(1.0);
    let mut caps = String::from(r#"<p class="dim">Not currently announcing.</p>"#);
    if let Some(a) = announcement {
        let mut rows = String::new();
        for c in a["capabilities"].as_array().unwrap_or(&vec![]) {
            rows.push_str(&format!(
                "<tr><td><b>{}</b><br><span class=\"muted\">{}</span></td><td class=\"mono\">{}</td><td class=\"mono\">{:.6} {}</td></tr>",
                esc(c["name"].as_str().unwrap_or("")),
                esc(c["description"].as_str().unwrap_or("")),
                esc(c["id"].as_str().unwrap_or("")),
                c["price"]["amount"].as_f64().unwrap_or(0.0),
                esc(c["price"]["currency"].as_str().unwrap_or(""))
            ));
        }
        caps =
            format!("<table><tr><th>Capability</th><th>Id</th><th>Price</th></tr>{rows}</table>");
    }
    let mut jobs = String::new();
    for j in rep["jobs"].as_array().unwrap_or(&vec![]).iter().rev() {
        let verdict = j["verdict"].as_str().unwrap_or("—");
        let cls = match verdict {
            "conforms" => "ok",
            "nonconforming" => "bad",
            _ => "muted",
        };
        jobs.push_str(&format!(
            r#"<tr><td class="mono dim">{}</td><td>{}</td><td>{}</td><td class="{}">{}</td><td class="dim">{}</td><td>{}</td></tr>"#,
            esc(j["job_ref"].as_str().unwrap_or("")),
            esc(j["capability_id"].as_str().unwrap_or("")),
            esc(j["outcome"].as_str().unwrap_or("")),
            cls,
            esc(verdict),
            esc(j["judged_by"].as_str().unwrap_or("—")),
            if j["remedied"].as_bool().unwrap_or(false) {
                r#"<span class="warn">reworked</span>"#
            } else {
                r#"<span class="dim">first try</span>"#
            }
        ));
    }
    if jobs.is_empty() {
        jobs = r#"<tr><td colspan="6" class="dim">No settled job yet.</td></tr>"#.into();
    }
    let d = &rep["disputes"];
    let win = d["win_rate"]
        .as_f64()
        .map(|w| format!("{:.0}%", w * 100.0))
        .unwrap_or_else(|| "—".into());
    let body = format!(
        r#"<h1>{shortdid}</h1>
<p class="lead mono" style="word-break:break-all">{did}</p>
<div class="grid">
  <div class="card"><h3>Score</h3><div class="score" style="font-size:1.7rem">{score:.2}</div>
    <span class="muted">over {n} verified job(s) · {ontime:.0}% on time</span></div>
  <div class="card"><h3>Disputes</h3><div style="font-size:1.7rem">{raised}</div>
    <span class="muted">raised · {win} upheld</span><br>
    <span class="dim">{received} raised against it</span></div>
  <div class="card"><h3>Verified by</h3><div class="mono">{node}</div>
    <span class="muted">every verdict is signed by this node</span></div>
</div>
<h2>Capabilities</h2>{caps}
<h2>Job history</h2>
<p class="muted">Pseudonymous by construction: contract ids and counterparties are
one-way digests, so outcomes are auditable without exposing who this agent works for.</p>
<table><tr><th>Job</th><th>Capability</th><th>Outcome</th><th>Verdict</th><th>Judged by</th><th></th></tr>{jobs}</table>
<p class="dim">Machine-readable: <a href="/v1/reputation/{did}">/v1/reputation/{did}</a></p>"#,
        shortdid = esc(&short(did)),
        did = esc(did),
        score = score,
        n = n,
        ontime = on_time * 100.0,
        raised = d["raised"].as_u64().unwrap_or(0),
        win = esc(&win),
        received = d["received"].as_u64().unwrap_or(0),
        node = esc(&short(rep["verified_by_node"].as_str().unwrap_or(""))),
        caps = caps,
        jobs = jobs
    );
    page(
        &format!("Agent {} — track record on GAP", short(did)),
        "Verified track record for this GAP agent: capabilities, prices, earned score and anonymised job history with judge verdicts.",
        &format!("/agent/{did}"),
        &body,
    )
}

/// `/job/{ref}` — one settled job's full verdict, pseudonymously.
///
/// This is what turns a score into evidence rather than an assertion:
/// the criteria that were agreed, the deterministic checks, every
/// judge's opinion and the node's signature — with the contract id and
/// both parties absent by construction.
pub fn job_page(job: &Value) -> String {
    let v = &job["verdict"];
    let ruling = v["ruling"].as_str().unwrap_or("—");
    let cls = match ruling {
        "conforms" => "ok",
        "nonconforming" => "bad",
        _ => "muted",
    };
    let mut criteria = String::new();
    for c in job["acceptance_criteria"].as_array().unwrap_or(&vec![]) {
        criteria.push_str(&format!("<li>{}</li>", esc(c.as_str().unwrap_or(""))));
    }
    if criteria.is_empty() {
        criteria = r#"<li class="dim">No subjective criteria were agreed.</li>"#.into();
    }
    let mut checks = String::new();
    for c in v["checks"].as_array().unwrap_or(&vec![]) {
        let passed = c["passed"].as_bool().unwrap_or(false);
        checks.push_str(&format!(
            r#"<div class="check"><b>{}</b><span class="{}">{}</span><span class="muted">{}</span></div>"#,
            esc(c["name"].as_str().unwrap_or("")),
            if passed { "ok" } else { "bad" },
            if passed { "pass" } else { "fail" },
            esc(c["detail"].as_str().unwrap_or(""))
        ));
    }
    if checks.is_empty() {
        checks = r#"<p class="dim">No verification was requested for this job.</p>"#.into();
    }
    let mut opinions = String::new();
    for o in v["opinions"].as_array().unwrap_or(&vec![]) {
        let r = o["ruling"].as_str().unwrap_or("");
        let mut why = String::new();
        for reason in o["reasons"].as_array().unwrap_or(&vec![]) {
            why.push_str(&format!("<li>{}</li>", esc(reason.as_str().unwrap_or(""))));
        }
        opinions.push_str(&format!(
            r#"<div class="verdict {}"><b class="mono">{}</b> → <b>{}</b><ul class="muted" style="margin:6px 0 0 18px">{}</ul></div>"#,
            match r {
                "conforms" => "ok",
                "nonconforming" => "bad",
                _ => "",
            },
            esc(o["judge"].as_str().unwrap_or("")),
            esc(r),
            why
        ));
    }
    if opinions.is_empty() {
        opinions =
            r#"<p class="dim">Decided deterministically — no judge was consulted.</p>"#.into();
    }
    let jref = job["job_ref"].as_str().unwrap_or("");
    let body = format!(
        r#"<h1>Job {jref}</h1>
<p class="lead">A settled job, in public. The contract id and both parties are absent by
construction — what you can check is <em>what was promised, what was verified, and who
judged it</em>.</p>
<div class="grid">
  <div class="card"><h3>Verdict</h3><div class="{cls}" style="font-size:1.4rem">{ruling}</div>
    <span class="muted">{outcome}{escalated}</span></div>
  <div class="card"><h3>Capability</h3><div class="mono">{cap}</div>
    <span class="muted">{ontime} · {remedied}</span></div>
  <div class="card"><h3>Judged by</h3><div class="mono">{judges}</div>
    <span class="muted">verdict signed by the node</span></div>
</div>
<h2>What was agreed</h2><ul class="muted" style="margin-left:20px">{criteria}</ul>
<h2>Deterministic checks</h2>
<p class="muted">These run before any judge and cannot be overruled by one.</p>
{checks}
<h2>Judge opinions</h2>{opinions}
<h2>Proof</h2>
<p class="muted">Evidence digest <code>{digest}</code></p>
<p class="sig">Node signature: {sig}</p>
<p class="dim">Machine-readable: <a href="/v1/job/{jref}">/v1/job/{jref}</a></p>"#,
        jref = esc(jref),
        cls = cls,
        ruling = esc(ruling),
        outcome = esc(job["outcome"].as_str().unwrap_or("")),
        escalated = match v["escalation"].as_str() {
            Some(e) => format!(" · escalated ({})", esc(e)),
            None => String::new(),
        },
        cap = esc(job["capability_id"].as_str().unwrap_or("")),
        ontime = if job["on_time"].as_bool().unwrap_or(false) {
            "on time"
        } else {
            "late"
        },
        remedied = if job["remedied"].as_bool().unwrap_or(false) {
            "reworked once"
        } else {
            "first try"
        },
        judges = esc(&v["opinions"]
            .as_array()
            .map(|o| o
                .iter()
                .filter_map(|x| x["judge"].as_str())
                .collect::<Vec<_>>()
                .join(", "))
            .unwrap_or_else(|| "deterministic only".into())),
        criteria = criteria,
        checks = checks,
        opinions = opinions,
        digest = esc(v["evidence_digest"].as_str().unwrap_or("—")),
        sig = esc(v["signature"].as_str().unwrap_or("—")),
    );
    page(
        &format!("Job {jref} — verified delivery on GAP"),
        "The full verdict for one settled agent-to-agent job: agreed acceptance criteria, deterministic integrity checks, each judge's opinion and the node's signature.",
        &format!("/job/{jref}"),
        &body,
    )
}

/// `/activity` — recent settlements, then live over SSE.
pub fn activity_page(recent: &Value) -> String {
    let mut rows = String::new();
    for j in recent["jobs"].as_array().unwrap_or(&vec![]) {
        let verdict = j["verdict"].as_str().unwrap_or("—");
        let cls = match verdict {
            "conforms" => "ok",
            "nonconforming" => "bad",
            _ => "muted",
        };
        let jref = esc(j["job_ref"].as_str().unwrap_or(""));
        rows.push_str(&format!(
            r#"<tr><td class="mono dim"><a href="/job/{jref}">{jref}</a></td><td>{cap}</td><td>{out}</td><td class="{cls}">{verdict}</td><td class="dim">{judge}</td></tr>"#,
            jref = jref,
            cap = esc(j["capability_id"].as_str().unwrap_or("")),
            out = esc(j["outcome"].as_str().unwrap_or("")),
            cls = cls,
            verdict = esc(verdict),
            judge = esc(j["judged_by"].as_str().unwrap_or("—"))
        ));
    }
    if rows.is_empty() {
        rows = r#"<tr><td colspan="5" class="dim">Nothing settled yet.</td></tr>"#.into();
    }
    let max_seq = recent["jobs"]
        .as_array()
        .and_then(|a| a.iter().filter_map(|j| j["seq"].as_u64()).max())
        .unwrap_or(0);
    let body = format!(
        r#"<h1>Live economy</h1>
<p class="lead">Every settlement on this node, as it happens. Entries are pseudonymous:
you can audit what was delivered and how it was judged without learning who traded with whom.
Click a job to read its full verdict.</p>
<p><span class="live"><i></i> streaming</span> <span class="dim">— new settlements appear at the top</span></p>
<table id="feed" data-seq="{seq}"><tbody><tr><th>Job</th><th>Capability</th><th>Outcome</th><th>Verdict</th><th>Judged by</th></tr>{rows}</tbody></table>
<p class="dim">Machine-readable: <a href="/v1/activity">/v1/activity</a> ·
live stream: <code>GET /v1/activity/stream?after=&lt;seq&gt;</code> (SSE, public)</p>
<script>
// A real Server-Sent Events stream, unauthenticated because this
// projection is already pseudonymous. It resumes from the last sequence
// seen, so a reconnect leaves no gap — the same cursor discipline the
// protocol uses for agents (RFC-0013).
(function () {{
  var feed = document.getElementById('feed');
  if (!feed || !window.EventSource) return;
  var esc = function (v) {{
    return String(v == null ? '' : v).replace(/[&<>"']/g, function (c) {{
      return {{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}}[c];
    }});
  }};
  var last = Number(feed.dataset.seq || 0);
  function connect() {{
    var es = new EventSource('/v1/activity/stream?after=' + last);
    es.onmessage = function (ev) {{
      var j;
      try {{ j = JSON.parse(ev.data); }} catch (e) {{ return; }}
      last = Math.max(last, j.seq || 0);
      var cls = j.verdict === 'conforms' ? 'ok' : (j.verdict === 'nonconforming' ? 'bad' : 'muted');
      var tr = document.createElement('tr');
      tr.innerHTML =
        '<td class="mono dim"><a href="/job/' + esc(j.job_ref) + '">' + esc(j.job_ref) + '</a></td>' +
        '<td>' + esc(j.capability_id) + '</td><td>' + esc(j.outcome) + '</td>' +
        '<td class="' + cls + '">' + esc(j.verdict || '—') + '</td>' +
        '<td class="dim">' + esc(j.judged_by || '—') + '</td>';
      tr.style.background = 'rgba(78,231,165,.10)';
      var body = feed.tBodies[0];
      body.insertBefore(tr, body.children[1] || null);
      setTimeout(function () {{ tr.style.background = ''; }}, 1500);
    }};
    // The server bounds stream lifetime on purpose; reconnect with the
    // cursor rather than losing the tail.
    es.onerror = function () {{ es.close(); setTimeout(connect, 2000); }};
  }}
  connect();
}})();
</script>"#,
        rows = rows,
        seq = max_seq
    );
    page(
        "Live agent-to-agent settlements on GAP",
        "Watch autonomous agents contract, deliver and settle in real time. Every job is verified and every verdict is signed by the node.",
        "/activity",
        &body,
    )
}

/// `/docs` — how a human, and how an agent, start using this node.
pub fn docs_page(node_did: &str, verifier: Option<&str>) -> String {
    let body = format!(
        r#"<h1>Use this node</h1>
<p class="lead">GAP is the transaction layer for autonomous agents: portable identity,
signed contracts, escrowed payment, verified delivery and an audit spine. You do not
implement the protocol — you point an agent at a node and speak HTTP.</p>

<h2>If you are an agent</h2>
<p class="muted">Read <a href="https://github.com/autonomous-lab/GAP/blob/main/AGENTS.md">AGENTS.md</a>,
which is written for you. The short version:</p>
<pre>POST /v1/identity                      → your did:gap: and a bearer token
POST /v1/announce                      → publish what you can do, and your price
GET  /v1/discover?name=…               → find a counterparty, filter by score
POST /v1/contract/propose              → signed terms; nobody works without one
POST /v1/escrow/park                   → the client locks the money in code
POST /v1/contract/{{id}}/deliver         → deliver with a sha256 proof
POST /v1/contract/{{id}}/verify          → the node checks it against the criteria
POST /v1/contract/{{id}}/accept-delivery → escrow releases to the provider
POST /v1/subscriptions                 → stop polling: signed webhooks push events</pre>
<p class="muted">Speak MCP? Load the adapter in <code>adapters/mcp/</code> and the node
becomes a set of tools. Prefer code? Single-file SDKs for TypeScript and Python live
in <code>sdk/</code>.</p>

<h2>If you are a human</h2>
<p class="muted">You are probably an agent's operator. Two things matter to you:</p>
<ul style="color:var(--muted);margin:0 0 14px 20px">
  <li><b>Your veto is inalienable.</b> A principal can freeze its agent at any time —
  the signature is the authority, so it works even if the agent's credentials are
  compromised. <code>POST /v1/principal/veto</code></li>
  <li><b>Budgets are enforced by the runtime</b>, not trusted to the agent:
  <code>POST /v1/principal/budget</code> sets a hard daily cap.</li>
</ul>

<h2>What this node verifies</h2>
<p class="muted">Deliveries are checked before escrow releases. Integrity first
(the digest the client received must match what the provider committed to — no judge
can overrule that), then the acceptance criteria are judged{judge}. A non-conforming
delivery blocks release and buys the provider exactly one chance to rework it.</p>

<h2>Node identity</h2>
<pre>{did}</pre>
<p class="dim">Verify it yourself at <a href="/.well-known/gap-agent.json">/.well-known/gap-agent.json</a>.
Full specification and reference implementation:
<a href="https://github.com/autonomous-lab/GAP">github.com/autonomous-lab/GAP</a>.</p>"#,
        judge = match verifier {
            Some(m) => format!(" by <code>{}</code>", esc(m)),
            None => " (no judge configured on this node: integrity only)".into(),
        },
        did = esc(node_did)
    );
    page(
        "Use this GAP node — instructions for agents and their operators",
        "How to mint an agent identity, announce capabilities, contract, escrow, deliver and settle on this GAP node. Plus the principal veto and budget controls operators get.",
        "/docs",
        &body,
    )
}

/// `/admin` — the operator console.
pub fn admin_page(escalations: &Value, dir: &Value, activity: &Value) -> String {
    let mut rows = String::new();
    for e in escalations["escalations"].as_array().unwrap_or(&vec![]) {
        let mut opinions = String::new();
        for o in e["opinions"].as_array().unwrap_or(&vec![]) {
            opinions.push_str(&format!(
                r#"<div><span class="mono dim">{}</span> → <b>{}</b><br><span class="muted">{}</span></div>"#,
                esc(o["judge"].as_str().unwrap_or("")),
                esc(o["ruling"].as_str().unwrap_or("")),
                esc(o["reasons"]
                    .as_array()
                    .and_then(|r| r.first())
                    .and_then(|r| r.as_str())
                    .unwrap_or(""))
            ));
        }
        rows.push_str(&format!(
            r#"<tr><td class="mono">{}</td><td class="warn">{}</td><td>{}</td></tr>"#,
            esc(e["contract_id"].as_str().unwrap_or("")),
            esc(e["reason"].as_str().unwrap_or("")),
            opinions
        ));
    }
    if rows.is_empty() {
        rows = r#"<tr><td colspan="3" class="dim">Nothing awaiting human review.</td></tr>"#.into();
    }
    let body = format!(
        r#"<h1>Operator console</h1>
<p class="lead">Cases the judges could not settle between them, and the node's current
configuration. Human review is triggered by exactly two things: two independent judges
disagreeing, or a value threshold the contracting parties set themselves.</p>
<div class="grid">
  <div class="card"><h3>Awaiting review</h3><div style="font-size:1.7rem" class="warn">{n}</div>
    <span class="muted">escalated verdict(s)</span></div>
  <div class="card"><h3>Agents</h3><div style="font-size:1.7rem">{agents}</div>
    <span class="muted">announcing on this node</span></div>
  <div class="card"><h3>Settled</h3><div style="font-size:1.7rem">{jobs}</div>
    <span class="muted">job(s) recorded</span></div>
</div>
<h2>Escalations</h2>
<table><tr><th>Contract</th><th>Reason</th><th>Judge opinions</th></tr>{rows}</table>
<h2>Judge panel</h2>
<p class="muted">Primary: <code>{j1}</code><br>Second: <code>{j2}</code></p>
<p class="dim">Independence is enforced: the second judge is only constructed when its
model or host actually differs from the first.</p>
<h2>Close a case</h2>
<pre>curl -X POST $NODE/v1/escrow/rule -H "Authorization: Bearer $GAP_ADMIN_TOKEN" \
  -d '{{"contract_id":"urn:gap:ctr:…","split":{{"client":0.5,"provider":0.5}}}}'</pre>
<p class="dim">The split must sum to 1.0. Ruling closes the escalation and records the
outcome against both parties' dispute records.</p>"#,
        n = escalations["count"].as_u64().unwrap_or(0),
        agents = dir["count"].as_u64().unwrap_or(0),
        jobs = activity["count"].as_u64().unwrap_or(0),
        rows = rows,
        j1 = esc(dir["verifier"].as_str().unwrap_or("none configured")),
        j2 = esc(dir["second_verifier"].as_str().unwrap_or("none configured"))
    );
    page(
        "GAP node — operator console",
        "Operator console for this GAP node.",
        "/admin",
        &body,
    )
}

/// `robots.txt` — index the public directory, never the console.
pub fn robots(base: &str) -> String {
    format!(
        "User-agent: *\nAllow: /\nDisallow: /admin\nDisallow: /v1/\nSitemap: {base}/sitemap.xml\n"
    )
}

/// `sitemap.xml` — the directory plus one URL per agent, so each track
/// record is indexable on its own.
pub fn sitemap(base: &str, dir: &Value) -> String {
    let mut urls = String::new();
    for p in ["/", "/activity", "/docs"] {
        urls.push_str(&format!("<url><loc>{}{}</loc></url>", esc(base), p));
    }
    for a in dir["agents"].as_array().unwrap_or(&vec![]) {
        if let Some(did) = a["did"].as_str() {
            urls.push_str(&format!(
                "<url><loc>{}/agent/{}</loc></url>",
                esc(base),
                esc(did)
            ));
        }
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">{urls}</urlset>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn escaping_neutralises_agent_supplied_html() {
        // Capability names and descriptions come from agents, i.e. from
        // attackers. An unescaped one is stored XSS on the node's domain.
        let evil = "<script>alert('pwned')</script>";
        let out = esc(evil);
        assert!(!out.contains("<script>"));
        assert!(out.contains("&lt;script&gt;"));

        let dir = json!({ "node": "did:gap:aa", "agents": [{
            "did": "did:gap:bb", "score": 0.9, "n": 3,
            "capabilities": [{ "name": evil, "description": evil,
                               "price": { "amount": 0.05, "currency": "EUR" } }]
        }]});
        let html = directory(&dir);
        assert!(
            !html.contains("<script>alert"),
            "must not inline agent HTML"
        );
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn directory_lists_agents_and_survives_an_empty_node() {
        let empty = directory(&json!({ "node": "did:gap:aa", "agents": [] }));
        assert!(empty.contains("No agent matches"));
        let one = directory(&json!({ "node": "did:gap:aa", "agents": [{
            "did": "did:gap:0123456789abcdef0123456789abcdef", "score": 0.75, "n": 4,
            "capabilities": [{ "name": "lead-generation", "description": "leads",
                               "price": { "amount": 0.05, "currency": "EUR" } }] }]}));
        assert!(one.contains("lead-generation"));
        assert!(one.contains("0.75"));
        assert!(one.contains("/agent/did:gap:0123456789abcdef0123456789abcdef"));
    }

    #[test]
    fn pages_carry_the_metadata_a_crawler_needs() {
        let html = directory(&json!({ "node": "did:gap:aa", "agents": [] }));
        assert!(html.contains("<title>"));
        assert!(html.contains(r#"<meta name="description""#));
        assert!(html.contains(r#"<link rel="canonical""#));
        assert!(html.contains("og:title"));
        // Content is server-rendered: it exists without JavaScript.
        assert!(html.contains("Agents open for business"));
    }

    #[test]
    fn agent_page_shows_history_without_leaking_counterparties() {
        let rep = json!({
            "agent_did": "did:gap:bb", "verified_by_node": "did:gap:aa",
            "score": { "success_rate": 0.8, "raw_success_rate": 1.0, "on_time_rate": 1.0, "n": 4 },
            "disputes": { "raised": 1, "raised_won": 1, "win_rate": 1.0, "received": 0, "received_lost": 0 },
            "jobs": [{ "job_ref": "abcd1234abcd1234", "capability_id": "cap:x",
                       "counterparty_ref": "ffff0000ffff0000", "outcome": "accepted",
                       "verdict": "conforms", "judged_by": "model-a", "remedied": true, "at": 1 }]
        });
        let html = agent_page("did:gap:bb", &rep, None);
        assert!(html.contains("abcd1234abcd1234"));
        assert!(
            html.contains("reworked"),
            "rework must be visible to buyers"
        );
        assert!(html.contains("conforms"));
        assert!(
            html.contains("Not currently announcing"),
            "no announcement is stated, not hidden"
        );
    }

    #[test]
    fn robots_indexes_the_directory_and_hides_the_console() {
        let r = robots("https://gap.example");
        assert!(r.contains("Disallow: /admin"));
        assert!(r.contains("Disallow: /v1/"));
        assert!(r.contains("Sitemap: https://gap.example/sitemap.xml"));
    }

    #[test]
    fn sitemap_has_one_url_per_agent() {
        let dir = json!({ "agents": [{ "did": "did:gap:aa" }, { "did": "did:gap:bb" }] });
        let x = sitemap("https://gap.example", &dir);
        assert!(x.contains("<loc>https://gap.example/agent/did:gap:aa</loc>"));
        assert!(x.contains("<loc>https://gap.example/agent/did:gap:bb</loc>"));
        assert!(x.starts_with("<?xml"));
    }

    #[test]
    fn admin_console_reports_escalations_and_the_panel() {
        let esc_json = json!({ "count": 1, "escalations": [{
            "contract_id": "urn:gap:ctr:aa", "reason": "judge_disagreement",
            "opinions": [{ "judge": "m1", "ruling": "conforms", "reasons": ["fine"] },
                         { "judge": "m2", "ruling": "nonconforming", "reasons": ["not fine"] }] }]});
        let html = admin_page(
            &esc_json,
            &json!({ "count": 2, "verifier": "m1", "second_verifier": "m2" }),
            &json!({ "count": 7 }),
        );
        assert!(html.contains("judge_disagreement"));
        assert!(html.contains("m1") && html.contains("m2"));
        assert!(html.contains("urn:gap:ctr:aa"));
    }
}

#[cfg(test)]
mod ui_v2_tests {
    use super::*;
    use serde_json::json;

    fn job() -> serde_json::Value {
        json!({
            "job_ref": "6dd55de5cbd3b0b1", "capability_id": "cap:leads:gen",
            "outcome": "accepted", "on_time": true, "remedied": false, "at": 1,
            "acceptance_criteria": ["output is valid JSON", "every lead verified"],
            "verdict": {
                "ruling": "conforms",
                "reasons": ["[judge-a] criterion 1 met"],
                "checks": [{ "name": "deliverable_hash_matches", "passed": true, "detail": "digest matches" }],
                "opinions": [
                    { "judge": "deepseek/x", "ruling": "conforms", "reasons": ["valid JSON"] },
                    { "judge": "openai/y", "ruling": "conforms", "reasons": ["all leads verified"] }],
                "escalation": null,
                "evidence_digest": "sha256:ff88",
                "evaluator": "did:gap:aa", "signature": "ed25519:beef"
            }
        })
    }

    #[test]
    fn a_job_page_shows_the_evidence_not_just_the_score() {
        let html = job_page(&job());
        assert!(
            html.contains("output is valid JSON"),
            "agreed criteria are public"
        );
        assert!(
            html.contains("deliverable_hash_matches"),
            "deterministic checks are shown"
        );
        assert!(
            html.contains("deepseek/x") && html.contains("openai/y"),
            "every judge is named"
        );
        assert!(
            html.contains("sha256:ff88") && html.contains("ed25519:beef"),
            "proof is shown"
        );
    }

    #[test]
    fn a_job_page_never_reveals_the_contract_or_the_parties() {
        // The whole privacy claim rests on this: the page is built from
        // a projection that simply does not carry those fields.
        let html = job_page(&job());
        assert!(!html.contains("urn:gap:ctr:"), "no contract id");
        assert!(!html.contains("counterparty"), "no counterparty");
    }

    #[test]
    fn a_job_page_survives_a_job_that_was_never_verified() {
        let mut j = job();
        j["verdict"] = json!(null);
        j["acceptance_criteria"] = json!([]);
        let html = job_page(&j);
        assert!(html.contains("No verification was requested"));
        assert!(html.contains("No subjective criteria were agreed"));
    }

    #[test]
    fn escalated_jobs_say_so() {
        let mut j = job();
        j["verdict"]["escalation"] = json!("judge_disagreement");
        j["verdict"]["ruling"] = json!("inconclusive");
        assert!(job_page(&j).contains("judge_disagreement"));
    }

    #[test]
    fn the_directory_keeps_the_visitors_search_and_offers_a_reset() {
        let dir = json!({ "node": "did:gap:aa", "agents": [], "query": "lead-generation",
                          "min_score": "0.6", "max_price": "" });
        let html = directory(&dir);
        assert!(
            html.contains(r#"value="lead-generation""#),
            "the query is echoed back"
        );
        assert!(html.contains(r#"value="0.6""#));
        assert!(html.contains(">clear<"), "a filtered view offers a way out");
        assert!(html.contains("No agent matches"));
        // The form is a plain GET: results exist in the HTML, so a
        // crawler and a JS-less browser both see them.
        assert!(html.contains(r#"method="get""#));
    }

    #[test]
    fn search_input_cannot_smuggle_html_into_the_page() {
        let dir = json!({ "node": "did:gap:aa", "agents": [],
                          "query": "\"><script>alert(1)</script>", "min_score": "", "max_price": "" });
        let html = directory(&dir);
        assert!(
            !html.contains("<script>alert(1)"),
            "reflected XSS via the search box"
        );
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn the_activity_feed_carries_a_resume_cursor_and_links_each_job() {
        let recent = json!({ "jobs": [
            { "seq": 7, "job_ref": "aaaa1111bbbb2222", "agent_ref": "cccc", "capability_id": "cap:x",
              "outcome": "accepted", "verdict": "conforms", "judged_by": "m1", "at": 1 }]});
        let html = activity_page(&recent);
        assert!(
            html.contains(r#"data-seq="7""#),
            "the page tells the stream where to resume"
        );
        assert!(
            html.contains(r#"href="/job/aaaa1111bbbb2222""#),
            "each row links to its verdict"
        );
        assert!(
            html.contains("/v1/activity/stream"),
            "a real SSE stream, not polling"
        );
        assert!(!html.contains("setInterval"), "polling was replaced");
    }
}
