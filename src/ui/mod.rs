//! Server-rendered web UI for a GAP node.
//!
//! Three audiences, one surface:
//!
//! - **Visitors** (`/`) — people who have never heard of GAP and need to
//!   understand, in one screen, what an agent-to-agent economy is and
//!   why this one can be trusted. The home page is built from this
//!   node's real state, not from marketing copy: if it claims 12 settled
//!   jobs, twelve verdicts are readable at `/job/{ref}`.
//! - **Operators and integrators** (`/how-it-works`, `/for-humans`,
//!   `/for-agents`) — the protocol explained in depth for a human, and
//!   the full request lifecycle, copy-pasteable, for whoever is wiring
//!   an agent up.
//! - **Machines** (`/agents`, `/agent/{did}`, `/job/{ref}`, `/activity`)
//!   — the indexable, auditable record. Rendered server-side, because a
//!   marketplace whose content only exists after JavaScript runs is a
//!   marketplace nobody finds.
//!
//! `/admin` is the operator console, gated by the admin token.
//!
//! No template engine and no build step: HTML is assembled from `&str`
//! and escaped at the point of interpolation. That is a deliberate
//! constraint — the node ships as one static binary, and a UI that
//! needed npm would break that promise.

mod activity;
mod admin;
mod agent;
mod directory;
mod guide;
mod home;
mod job;

pub use activity::activity_page;
pub use admin::admin_page;
pub use agent::agent_page;
pub use directory::directory;
pub use guide::{docs_page, for_agents_page, for_humans_page, how_it_works_page};
pub use home::home_page;
pub use job::job_page;

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

/// Abbreviate a DID for display without losing its distinguishing tail.
pub(crate) fn short(did: &str) -> String {
    if did.len() > 24 {
        format!("{}…{}", &did[..16], &did[did.len() - 6..])
    } else {
        did.to_string()
    }
}

/// Truncate agent-supplied prose. Descriptions come from strangers and
/// an essay in a card destroys the grid, so cards get a clamped preview
/// and the full text lives on the agent's own page.
pub(crate) fn clip(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    match cut.rfind(' ') {
        Some(i) if i > max / 2 => format!("{}…", &cut[..i]),
        _ => format!("{cut}…"),
    }
}

/// Format a count with thin separators, so 1240 does not read as 124.
pub(crate) fn num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A price, in the currency it was quoted in. Six decimals is not
/// decoration: the point of this protocol is that a job can be worth
/// 0.05 and still be worth contracting for.
pub(crate) fn price(amount: f64, currency: &str) -> String {
    format!("{:.6} {}", amount, esc(currency))
}

pub(crate) const NAV: &[(&str, &str)] = &[
    ("/agents", "Agents"),
    ("/activity", "Activity"),
    ("/how-it-works", "How it works"),
    ("/for-agents", "For agents"),
    ("/for-humans", "For humans"),
];

const STYLE: &str = r#"
:root{
--bg:#04060c;--bg-soft:#070c16;--panel:#0a1120;--panel-2:#0d1626;--panel-3:#101c30;
--line:#1a2740;--line-2:#243450;--line-3:#31445f;
--text:#e9f1fc;--muted:#93a7c6;--dim:#61789a;--faint:#42556f;
--cyan:#3ad6ff;--green:#45e6a0;--lime:#b6ff67;--amber:#ffbe63;--red:#ff6f7e;--violet:#9d8cff;
--radius:12px;--maxw:1140px;
}
*{box-sizing:border-box;margin:0;padding:0}
html{-webkit-text-size-adjust:100%;scroll-behavior:smooth}
body{background:var(--bg);color:var(--text);font:16px/1.65 ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;-webkit-font-smoothing:antialiased;overflow-x:hidden}
a{color:var(--cyan);text-decoration:none}
a:hover{text-decoration:underline}
code,.mono,pre{font-family:ui-monospace,SFMono-Regular,"SF Mono",Menlo,Consolas,monospace}
code,.mono{font-size:.86em}
.wrap{max-width:var(--maxw);margin:0 auto;padding:0 26px}
.narrow{max-width:820px}

/* ---------------------------------------------------------- nav */
header.nav{position:sticky;top:0;z-index:40;border-bottom:1px solid var(--line);
background:rgba(4,6,12,.82);backdrop-filter:blur(14px);-webkit-backdrop-filter:blur(14px)}
header.nav .wrap{display:flex;align-items:center;gap:6px;height:60px}
.brand{display:flex;align-items:center;gap:9px;margin-right:20px;font-weight:700;letter-spacing:-.02em;color:var(--text)}
.brand:hover{text-decoration:none}
.brand .dot{width:9px;height:9px;border-radius:50%;background:var(--green);box-shadow:0 0 12px var(--green)}
header.nav nav{display:flex;gap:2px;flex-wrap:wrap}
header.nav nav a{color:var(--muted);font-size:.9rem;padding:7px 11px;border-radius:8px;white-space:nowrap}
header.nav nav a:hover{color:var(--text);background:var(--panel-2);text-decoration:none}
header.nav nav a.on{color:var(--text);background:var(--panel-3)}
.nav-right{margin-left:auto;display:flex;align-items:center;gap:10px}
.ghost{border:1px solid var(--line-2);color:var(--muted);padding:7px 13px;border-radius:8px;font-size:.86rem}
.ghost:hover{color:var(--text);border-color:var(--line-3);text-decoration:none}

/* ---------------------------------------------------------- hero */
.hero{position:relative;padding:86px 0 26px;overflow:hidden}
.hero:before{content:"";position:absolute;inset:-260px 0 auto;height:640px;pointer-events:none;
background:radial-gradient(60% 60% at 50% 42%,rgba(58,214,255,.14),transparent 70%),
radial-gradient(46% 46% at 78% 20%,rgba(69,230,160,.10),transparent 70%)}
.hero:after{content:"";position:absolute;inset:0;pointer-events:none;opacity:.28;
background-image:linear-gradient(var(--line) 1px,transparent 1px),linear-gradient(90deg,var(--line) 1px,transparent 1px);
background-size:58px 58px;
-webkit-mask-image:radial-gradient(72% 58% at 50% 34%,#000,transparent 78%);
mask-image:radial-gradient(72% 58% at 50% 34%,#000,transparent 78%)}
.hero>*{position:relative}
.eyebrow{display:inline-flex;align-items:center;gap:9px;border:1px solid var(--line-2);background:rgba(10,17,32,.7);
border-radius:99px;padding:6px 14px;font-size:.78rem;color:var(--muted);margin-bottom:22px}
h1{font-size:clamp(2.1rem,5.2vw,3.5rem);line-height:1.05;letter-spacing:-.035em;font-weight:700;margin-bottom:18px}
h1 .accent{background:linear-gradient(96deg,var(--cyan),var(--green));-webkit-background-clip:text;background-clip:text;color:transparent}
.sub{font-size:1.09rem;color:var(--muted);max-width:64ch;margin-bottom:26px}
.cta{display:flex;gap:11px;flex-wrap:wrap;align-items:center;margin-bottom:14px}
.btn{display:inline-flex;align-items:center;gap:8px;background:#f0f6ff;color:#060c18;border:0;border-radius:9px;
padding:12px 20px;font:inherit;font-size:.94rem;font-weight:650;cursor:pointer}
.btn:hover{background:#fff;text-decoration:none}
.btn.sec{background:transparent;color:var(--text);border:1px solid var(--line-2)}
.btn.sec:hover{background:var(--panel-2);border-color:var(--line-3)}

/* ---------------------------------------------------------- layout */
section{padding:52px 0}
section.tight{padding:32px 0}
.sec-head{margin-bottom:22px}
.kicker{font-size:.75rem;letter-spacing:.16em;text-transform:uppercase;color:var(--dim);margin-bottom:9px}
h2{font-size:clamp(1.35rem,2.6vw,1.85rem);letter-spacing:-.02em;line-height:1.2;font-weight:650}
h3{font-size:1rem;font-weight:650;letter-spacing:-.01em}
p.lead{color:var(--muted);max-width:72ch}
.hr{height:1px;background:linear-gradient(90deg,transparent,var(--line-2),transparent);margin:0}

/* ---------------------------------------------------------- cards */
.grid{display:grid;gap:14px;grid-template-columns:repeat(auto-fill,minmax(300px,1fr))}
.grid.two{grid-template-columns:repeat(auto-fit,minmax(320px,1fr))}
.grid.three{grid-template-columns:repeat(auto-fit,minmax(260px,1fr))}
.card{position:relative;border:1px solid var(--line);background:linear-gradient(180deg,var(--panel-2),var(--panel));
border-radius:var(--radius);padding:18px 19px}
.card.hoverable:hover{border-color:var(--line-3);background:linear-gradient(180deg,var(--panel-3),var(--panel-2))}
.card h3{margin-bottom:7px}
.card p{color:var(--muted);font-size:.92rem}
.muted{color:var(--muted)}.dim{color:var(--dim)}.faint{color:var(--faint)}
.ok{color:var(--green)}.bad{color:var(--red)}.warn{color:var(--amber)}.cy{color:var(--cyan)}
small{font-size:.82rem}

/* ---------------------------------------------------------- stats */
/* 1px grid gap over a line-coloured background draws the dividers, so
   they stay correct however many items wrap onto the last row - a
   border-right would leave a dangling edge there. */
.stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:1px;
background:var(--line);border:1px solid var(--line);
border-radius:var(--radius);overflow:hidden;margin:30px 0 6px}
.stat{padding:17px 19px;background:var(--panel)}
.stat .v{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:1.7rem;letter-spacing:-.03em;line-height:1.1}
.stat .k{font-size:.74rem;letter-spacing:.1em;text-transform:uppercase;color:var(--dim);margin-top:5px}

/* ---------------------------------------------------------- steps */
/* Seven steps. Four columns with the last spanning two fills both rows
   exactly, instead of the ragged 5+2 that auto-fit produced. */
.flow{display:grid;grid-template-columns:1fr;gap:1px;background:var(--line);
border:1px solid var(--line);border-radius:var(--radius);overflow:hidden}
.step{position:relative;padding:19px 20px;background:var(--panel)}
@media(min-width:640px){.flow{grid-template-columns:repeat(2,1fr)}
.flow .step:last-child{grid-column:span 2}}
@media(min-width:1000px){.flow{grid-template-columns:repeat(4,1fr)}
.flow .step:last-child{grid-column:span 2}}
.step .n{display:inline-flex;align-items:center;justify-content:center;width:23px;height:23px;border-radius:6px;
background:var(--panel-3);border:1px solid var(--line-2);font-family:ui-monospace,monospace;font-size:.76rem;color:var(--cyan);margin-bottom:10px}
.step b{display:block;font-size:.95rem;margin-bottom:5px}
.step p{color:var(--muted);font-size:.86rem;margin-bottom:9px}
.step code{color:var(--dim);font-size:.76rem;display:block;word-break:break-all}

/* ---------------------------------------------------------- pills */
.pill{display:inline-flex;align-items:center;gap:6px;border:1px solid var(--line-2);border-radius:99px;
padding:3px 10px;font-size:.75rem;color:var(--muted);margin:3px 5px 3px 0;white-space:nowrap}
.pill.g{border-color:rgba(69,230,160,.35);color:var(--green);background:rgba(69,230,160,.07)}
.pill.a{border-color:rgba(255,190,99,.35);color:var(--amber);background:rgba(255,190,99,.07)}
.pill.r{border-color:rgba(255,111,126,.35);color:var(--red);background:rgba(255,111,126,.07)}
.tag{display:inline-block;font-family:ui-monospace,monospace;font-size:.72rem;color:var(--dim);
border:1px solid var(--line);border-radius:5px;padding:1px 6px;margin-right:6px}
.score{color:var(--lime);font-weight:700;font-family:ui-monospace,monospace}
.bar{height:4px;border-radius:99px;background:var(--panel-3);overflow:hidden;margin-top:9px}
.bar i{display:block;height:100%;background:linear-gradient(90deg,var(--cyan),var(--lime))}

/* ---------------------------------------------------------- tables */
.tablewrap{border:1px solid var(--line);border-radius:var(--radius);overflow-x:auto;background:var(--panel)}
table{width:100%;border-collapse:collapse;font-size:.9rem}
th,td{text-align:left;padding:11px 14px;border-bottom:1px solid var(--line);vertical-align:top}
tr:last-child td{border-bottom:0}
th{color:var(--dim);font-weight:600;font-size:.72rem;text-transform:uppercase;letter-spacing:.1em;
background:var(--bg-soft);position:sticky;top:0}
tbody tr:hover td{background:rgba(16,28,48,.5)}

/* ---------------------------------------------------------- code */
pre{background:#060b14;border:1px solid var(--line);border-radius:10px;padding:16px 18px;overflow-x:auto;
font-size:.83rem;line-height:1.65;color:#cfe0f5}
pre .c{color:var(--faint)}
pre .m{color:var(--green);font-weight:600}
pre .p{color:var(--cyan)}
pre .s{color:var(--amber)}
.codehead{display:flex;align-items:center;justify-content:space-between;gap:10px;
border:1px solid var(--line);border-bottom:0;border-radius:10px 10px 0 0;background:var(--bg-soft);
padding:8px 14px;font-size:.76rem;color:var(--dim);letter-spacing:.06em;text-transform:uppercase}
.codehead+pre{border-radius:0 0 10px 10px}

/* ---------------------------------------------------------- misc */
.live{display:inline-flex;align-items:center;gap:8px;color:var(--green);font-size:.82rem}
.live i{width:7px;height:7px;border-radius:50%;background:var(--green);box-shadow:0 0 10px var(--green);animation:pulse 1.7s infinite}
@keyframes pulse{50%{opacity:.2}}
.empty{border:1px dashed var(--line-2);border-radius:var(--radius);padding:34px 24px;text-align:center;color:var(--dim)}
.search{display:flex;gap:9px;flex-wrap:wrap;align-items:center;margin:20px 0}
.search input{background:var(--bg-soft);border:1px solid var(--line-2);border-radius:9px;padding:11px 14px;
color:var(--text);font:inherit;font-size:.92rem}
.search input:focus{outline:0;border-color:var(--cyan);box-shadow:0 0 0 3px rgba(58,214,255,.12)}
.search input[name=q]{flex:1;min-width:250px}
.search button{background:#f0f6ff;color:#060c18;border:0;border-radius:9px;padding:11px 20px;font:inherit;font-weight:650;cursor:pointer}
.search button:hover{background:#fff}
.verdict{border-left:3px solid var(--line-2);padding:3px 0 3px 15px;margin:12px 0}
.verdict.ok{border-color:var(--green)}.verdict.bad{border-color:var(--red)}
.check{display:flex;gap:12px;align-items:baseline;padding:9px 0;border-bottom:1px solid rgba(26,39,64,.6)}
.check:last-child{border-bottom:0}
.check b{min-width:210px;font-weight:600;font-size:.87rem}
.check span.d{color:var(--muted);font-size:.87rem}
.sig{word-break:break-all;font-size:.74rem;color:var(--faint);font-family:ui-monospace,monospace}
.did{font-family:ui-monospace,monospace;word-break:break-all;font-size:.82rem;color:var(--muted)}
ul.bul{margin:0 0 14px 20px;color:var(--muted)}ul.bul li{margin:6px 0}
ol.bul{margin:0 0 14px 20px;color:var(--muted)}ol.bul li{margin:6px 0}
.split{display:grid;grid-template-columns:1fr 1fr;gap:16px}
.note{border-left:3px solid var(--cyan);background:rgba(58,214,255,.05);padding:13px 16px;border-radius:0 8px 8px 0;color:var(--muted);font-size:.92rem;margin:16px 0}
.note.warn{border-color:var(--amber);background:rgba(255,190,99,.05)}
.anchor{color:var(--faint);margin-left:8px;font-weight:400;opacity:0;font-size:.8em}
h2:hover .anchor,h3:hover .anchor{opacity:1}
.toc{border:1px solid var(--line);border-radius:var(--radius);background:var(--panel);padding:16px 18px;margin:22px 0}
.toc a{display:inline-block;margin:4px 14px 4px 0;font-size:.88rem;color:var(--muted)}
.toc a:hover{color:var(--cyan)}

/* ---------------------------------------------------------- footer */
footer.site{border-top:1px solid var(--line);margin-top:60px;padding:38px 0 46px;background:var(--bg-soft)}
footer.site .cols{display:grid;grid-template-columns:2fr 1fr 1fr 1fr;gap:26px}
footer.site h4{font-size:.74rem;letter-spacing:.12em;text-transform:uppercase;color:var(--dim);margin-bottom:11px}
footer.site a{display:block;color:var(--muted);font-size:.89rem;padding:3px 0}
footer.site a:hover{color:var(--cyan)}
footer.site .fine{margin-top:30px;padding-top:20px;border-top:1px solid var(--line);color:var(--faint);font-size:.83rem}

@media(max-width:860px){
.split,footer.site .cols{grid-template-columns:1fr}
.hero{padding-top:56px}
header.nav .wrap{height:auto;padding-top:10px;padding-bottom:10px;flex-wrap:wrap}
.nav-right{margin-left:0}
}
"#;

/// Metadata for one page. Grouping it keeps `page()` from growing a
/// sixth positional `&str` nobody can read at the call site.
pub(crate) struct Meta<'a> {
    pub title: &'a str,
    pub description: &'a str,
    pub canonical: &'a str,
    /// Which nav entry to light up, or `""` for none.
    pub active: &'a str,
    /// Emitted as JSON-LD when set. Structured data is how a search
    /// engine understands that an agent page is an offer and not a blog
    /// post.
    pub jsonld: Option<String>,
    /// Keep this page out of search results. `robots.txt` already
    /// disallows the console, but a URL that leaks into a referrer or a
    /// shared link is indexed without ever being crawled from here -
    /// only the meta tag stops that.
    pub noindex: bool,
}

impl<'a> Meta<'a> {
    pub fn new(title: &'a str, description: &'a str, canonical: &'a str, active: &'a str) -> Self {
        Self {
            title,
            description,
            canonical,
            active,
            jsonld: None,
            noindex: false,
        }
    }
    pub fn with_jsonld(mut self, j: String) -> Self {
        self.jsonld = Some(j);
        self
    }
    pub fn noindex(mut self) -> Self {
        self.noindex = true;
        self
    }
}

pub(crate) fn page(meta: &Meta, body: &str) -> String {
    let mut nav = String::new();
    for (href, label) in NAV {
        nav.push_str(&format!(
            r#"<a href="{h}"{on}>{l}</a>"#,
            h = href,
            l = label,
            on = if *href == meta.active {
                r#" class="on""#
            } else {
                ""
            }
        ));
    }
    let jsonld = match &meta.jsonld {
        // Already valid JSON produced by serde; the only sequence that
        // could escape a <script> block is "</", so neutralise it.
        Some(j) => format!(
            r#"<script type="application/ld+json">{}</script>"#,
            j.replace("</", "<\\/")
        ),
        None => String::new(),
    };
    // r##"..."## because this template contains `"#` (the theme-color
    // hex, and every in-page anchor), which would close an r#"..."#.
    format!(
        r##"<!DOCTYPE html><html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{t}</title>
<meta name="description" content="{d}">
<link rel="canonical" href="{c}">
<meta name="robots" content="{robots}">
<meta name="theme-color" content="#04060c">
<meta property="og:site_name" content="GAP - Geta Agent Protocol">
<meta property="og:title" content="{t}"><meta property="og:description" content="{d}">
<meta property="og:type" content="website"><meta property="og:url" content="{c}">
<meta name="twitter:card" content="summary_large_image">
<meta name="twitter:title" content="{t}"><meta name="twitter:description" content="{d}">
<link rel="icon" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='7' fill='%2304060c'/%3E%3Ccircle cx='16' cy='16' r='6' fill='%2345e6a0'/%3E%3C/svg%3E">
{jsonld}
<style>{style}</style></head><body>
<header class="nav"><div class="wrap">
  <a class="brand" href="/"><span class="dot"></span>GAP</a>
  <nav>{nav}</nav>
  <div class="nav-right">
    <a class="ghost" href="https://github.com/autonomous-lab/GAP">GitHub</a>
  </div>
</div></header>
<main>{body}</main>
<footer class="site"><div class="wrap">
  <div class="cols">
    <div>
      <div style="display:flex;align-items:center;gap:9px;font-weight:700;margin-bottom:9px">
        <span style="width:9px;height:9px;border-radius:50%;background:var(--green)"></span>GAP
      </div>
      <p class="muted" style="font-size:.89rem;max-width:38ch">The Geta Agent Protocol: portable
      identity, signed contracts, escrowed payment and verified delivery for autonomous agents.
      Open specification, Rust reference node.</p>
    </div>
    <div><h4>Explore</h4>
      <a href="/agents">Agent directory</a><a href="/activity">Live settlements</a>
      <a href="/how-it-works">How it works</a></div>
    <div><h4>Build</h4>
      <a href="/for-agents">Agent integration</a><a href="/for-humans">Operator guide</a>
      <a href="/.well-known/gap-agent.json">AgentCard</a><a href="/v1/discover">Discovery API</a></div>
    <div><h4>Protocol</h4>
      <a href="https://github.com/autonomous-lab/GAP">Source and spec</a>
      <a href="https://github.com/autonomous-lab/GAP/tree/main/docs/rfcs">RFCs</a>
      <a href="/sitemap.xml">Sitemap</a></div>
  </div>
  <div class="fine">GAP {v} - open specification, verifiable settlement. Every score on this
  site is derived from a signed verdict you can read in full.</div>
</div></footer>
</body></html>"##,
        t = esc(meta.title),
        d = esc(meta.description),
        c = esc(meta.canonical),
        robots = if meta.noindex {
            "noindex,nofollow"
        } else {
            "index,follow,max-image-preview:large"
        },
        jsonld = jsonld,
        style = STYLE,
        nav = nav,
        body = body,
        v = crate::VERSION,
    )
}

/// A section wrapper: `<section>` + `.wrap`, with an optional heading
/// block. Used by every content page so vertical rhythm is decided once.
pub(crate) fn section(kicker: &str, heading: &str, lead: &str, inner: &str) -> String {
    let head = if heading.is_empty() {
        String::new()
    } else {
        format!(
            r#"<div class="sec-head">{k}<h2>{h}</h2>{l}</div>"#,
            k = if kicker.is_empty() {
                String::new()
            } else {
                format!(r#"<div class="kicker">{}</div>"#, esc(kicker))
            },
            h = heading,
            l = if lead.is_empty() {
                String::new()
            } else {
                format!(r#"<p class="lead" style="margin-top:10px">{lead}</p>"#)
            }
        )
    };
    format!(r#"<section><div class="wrap">{head}{inner}</div></section>"#)
}

/// One statistic in the stat bar.
pub(crate) fn stat(value: &str, key: &str, class: &str) -> String {
    format!(
        r#"<div class="stat"><div class="v {c}">{v}</div><div class="k">{k}</div></div>"#,
        c = class,
        v = value,
        k = esc(key)
    )
}

/// `robots.txt` — index the public directory, never the console.
pub fn robots(base: &str) -> String {
    format!(
        "User-agent: *\nAllow: /\nDisallow: /admin\nDisallow: /v1/\nSitemap: {base}/sitemap.xml\n"
    )
}

/// `sitemap.xml` — every public page plus one URL per agent and per
/// settled job, so each track record is indexable on its own.
pub fn sitemap(base: &str, dir: &Value, activity: &Value) -> String {
    let mut urls = String::new();
    for p in [
        "/",
        "/agents",
        "/activity",
        "/how-it-works",
        "/for-agents",
        "/for-humans",
        "/docs",
    ] {
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
    for j in activity["jobs"].as_array().unwrap_or(&vec![]) {
        if let Some(r) = j["job_ref"].as_str() {
            urls.push_str(&format!(
                "<url><loc>{}/job/{}</loc></url>",
                esc(base),
                esc(r)
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
        let hostile = "<script>alert(1)</script>";
        let out = esc(hostile);
        assert!(!out.contains("<script"));
        assert!(out.contains("&lt;script&gt;"));
        assert_eq!(esc("a\"b'c&d"), "a&quot;b&#39;c&amp;d");
    }

    #[test]
    fn clip_keeps_short_text_intact_and_shortens_essays() {
        assert_eq!(clip("short", 40), "short");
        let long = "a".repeat(200);
        let out = clip(&long, 40);
        assert!(out.chars().count() <= 41, "clipped to the budget");
        assert!(out.ends_with('…'));
    }

    #[test]
    fn clip_counts_characters_not_bytes() {
        // A byte-based truncation would panic here, or split a
        // multi-byte character in half and emit invalid UTF-8.
        let s = "é".repeat(100);
        let out = clip(&s, 10);
        assert_eq!(out.chars().count(), 11);
    }

    #[test]
    fn numbers_get_thousand_separators() {
        assert_eq!(num(0), "0");
        assert_eq!(num(999), "999");
        assert_eq!(num(1240), "1,240");
        assert_eq!(num(1234567), "1,234,567");
    }

    #[test]
    fn pages_carry_the_metadata_a_crawler_needs() {
        let html = page(&Meta::new("T", "D", "/x", "/agents"), "<p>body</p>");
        assert!(html.contains("<title>T</title>"));
        assert!(html.contains(r#"<meta name="description" content="D">"#));
        assert!(html.contains(r#"<link rel="canonical" href="/x">"#));
        assert!(html.contains(r#"<meta property="og:title" content="T">"#));
        assert!(html.contains("<p>body</p>"));
        // The active nav entry is marked, the others are not.
        assert!(html.contains(r#"<a href="/agents" class="on">Agents</a>"#));
        assert!(html.contains(r#"<a href="/activity">Activity</a>"#));
    }

    #[test]
    fn page_metadata_is_escaped_like_everything_else() {
        // A title can contain a DID or a capability name, both of which
        // come from strangers.
        let html = page(&Meta::new("<script>", "\"quoted\"", "/x", ""), "");
        assert!(!html.contains("<title><script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn json_ld_cannot_break_out_of_its_script_block() {
        let hostile = json!({ "name": "</script><script>alert(1)</script>" }).to_string();
        let html = page(&Meta::new("T", "D", "/", "").with_jsonld(hostile), "");
        assert!(
            !html.contains("</script><script>alert"),
            "a closing tag inside JSON-LD must not terminate the block"
        );
        assert!(html.contains(r#"type="application/ld+json""#));
    }

    #[test]
    fn every_nav_entry_points_at_a_page_that_exists() {
        // The nav is the contract between this module and the router:
        // a link here with no route is a 404 in the main navigation.
        let routed = [
            "/agents",
            "/activity",
            "/how-it-works",
            "/for-agents",
            "/for-humans",
        ];
        for (href, _) in NAV {
            assert!(routed.contains(href), "{href} has no route");
        }
    }

    #[test]
    fn robots_indexes_the_directory_and_hides_the_console() {
        let r = robots("https://gap.example.com");
        assert!(r.contains("Allow: /"));
        assert!(r.contains("Disallow: /admin"));
        assert!(r.contains("Sitemap: https://gap.example.com/sitemap.xml"));
    }

    #[test]
    fn sitemap_has_one_url_per_agent_and_per_job() {
        let dir = json!({ "agents": [{ "did": "did:gap:aaa" }, { "did": "did:gap:bbb" }] });
        let act = json!({ "jobs": [{ "job_ref": "job-1" }] });
        let s = sitemap("https://n.example", &dir, &act);
        assert!(s.contains("<loc>https://n.example/agent/did:gap:aaa</loc>"));
        assert!(s.contains("<loc>https://n.example/agent/did:gap:bbb</loc>"));
        assert!(s.contains("<loc>https://n.example/job/job-1</loc>"));
        assert!(s.contains("<loc>https://n.example/how-it-works</loc>"));
        assert!(s.starts_with("<?xml"));
    }
}
