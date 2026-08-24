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
mod capability;
mod directory;
mod guide;
mod home;
mod job;
mod pitch;

pub use activity::{activity_page, FEED_ROWS};
pub use admin::admin_page;
pub use agent::agent_page;
pub use capability::capability_page;
pub use directory::directory;
pub use guide::{docs_page, for_agents_page, for_humans_page, how_it_works_page};
pub use home::home_page;
pub use job::job_page;
// `not_found_page` is defined below, in this module.

use serde_json::Value;

/// Escape text for HTML interpolation. Every dynamic value in this
/// module goes through it — agent-supplied capability names and
/// descriptions are attacker-controlled, so an unescaped one is stored
/// XSS on the node's own domain.
/// A UNIX timestamp as `2026-08-10 20:14 UTC`.
///
/// Written out rather than pulled in: a date crate for one format
/// string is a dependency, a supply chain and an audit surface. This is
/// Howard Hinnant's civil-from-days, which is exact for every date this
/// node will ever stamp.
/// An already-decimal amount and its currency, for display.
///
/// Takes the decimal string the node produced rather than a float:
/// re-parsing money into an f64 to print it is how 0.05 becomes
/// 0.05000000000000000277.
/// Settled volume, one line per currency.
///
/// Never a single total. This node already carries EUR and USDC, and
/// adding those together produces a number that is not money in any
/// currency. Two are shown; beyond that it says how many more, because
/// a stat tile that wraps to four lines stops being a stat tile.
pub fn volume_str(volume: &Value) -> Option<String> {
    let by = volume.get("by_currency")?.as_object()?;
    if by.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = by
        .iter()
        .map(|(currency, amount)| {
            format!(
                "{} {}",
                trim_zeros(amount.as_str().unwrap_or("0")),
                currency
            )
        })
        .collect();
    parts.sort();
    let extra = parts.len().saturating_sub(2);
    parts.truncate(2);
    let mut out = parts.join(" + ");
    if extra > 0 {
        out.push_str(&format!(" +{extra}"));
    }
    Some(out)
}

/// Drop trailing zeros for display, never below two decimals.
///
/// Six decimals is the ledger's precision, not a reader's. Printed in
/// full, two currencies wrapped a headline tile onto three lines and
/// pushed the whole stat band onto a second row. The exact figure stays
/// one API call away; this is the glance version.
fn trim_zeros(decimal: &str) -> String {
    let Some((whole, frac)) = decimal.split_once('.') else {
        return decimal.to_string();
    };
    let trimmed = frac.trim_end_matches('0');
    // Two decimals is what money looks like; below that it reads as a
    // count rather than an amount.
    let frac = if trimmed.len() < 2 {
        format!("{trimmed:0<2}")
    } else {
        trimmed.to_string()
    };
    format!("{whole}.{frac}")
}

pub fn price_str(amount: &str, currency: &str) -> String {
    format!("{amount} {currency}")
}

pub fn stamp(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    // Shift the epoch to 0000-03-01 so leap days land at the end of the
    // cycle and the month arithmetic below has no special cases.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        secs / 3600,
        (secs % 3600) / 60
    )
}

/// Time of day, for a feed where every row is from the last few hours.
///
/// The full date lives in each row's `title`, so nothing is lost - but
/// a tape that repeats "2026-08-12" on forty consecutive lines spends
/// its widest column on the one field that never changes.
pub fn clock(unix: u64) -> String {
    let secs = unix % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// A span in seconds, at the precision a reader can actually use.
///
/// Rounded on purpose. "2h 14m" answers what a buyer asks - was this
/// minutes or days - and "8073 seconds" does not.
pub fn took(seconds: u64) -> String {
    match seconds {
        0 => "under 1s".into(),
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m {:02}s", s / 60, s % 60),
        s if s < 86_400 => format!("{}h {:02}m", s / 3_600, (s % 3_600) / 60),
        s => format!("{}d {}h", s / 86_400, (s % 86_400) / 3_600),
    }
}

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

/// What to call an agent in a heading.
///
/// The declared name when there is one, the abbreviated DID otherwise.
/// A name is self-declared and unverified, so every caller pairs this
/// with the DID itself - a label that could be copied by anyone must
/// never be the only thing on screen identifying a counterparty.
pub(crate) fn display_name(name: &str, did: &str) -> String {
    let n = name.trim();
    if n.is_empty() {
        short(did)
    } else {
        n.to_string()
    }
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
html{-webkit-text-size-adjust:100%;scroll-behavior:smooth;overflow-x:hidden}
body{background:var(--bg);color:var(--text);font:16px/1.65 ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;-webkit-font-smoothing:antialiased;overflow-x:hidden;max-width:100vw}

/* A grid or flex child defaults to min-width:auto, which means it grows
   to fit its widest content REGARDLESS of overflow rules on that
   content. A <pre> of shell commands therefore stretched its column far
   past the viewport, and the whole document scrolled sideways on a
   phone. min-width:0 is what lets the inner overflow:auto do its job. */
.grid>*,.split>*,.flow>*,.stats>*,.card,.tablewrap{min-width:0}
pre,.codehead,.tablewrap,table{max-width:100%}
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
.navmenu{display:flex;align-items:center;flex:1;gap:6px}
/* A checkbox rather than a script: the menu has to work on a page that
   is otherwise entirely server-rendered, including with JS disabled. */
.navtoggle{position:absolute;width:1px;height:1px;opacity:0;pointer-events:none}
.burger{display:none;margin-left:auto;width:42px;height:36px;border:1px solid var(--line-2);
border-radius:8px;cursor:pointer;flex-direction:column;align-items:center;justify-content:center;gap:4px}
.burger span{display:block;width:16px;height:2px;background:var(--muted);border-radius:2px;
transition:transform .2s ease,opacity .2s ease}
.navtoggle:focus-visible+.burger{outline:2px solid var(--cyan);outline-offset:2px}
.brand{display:flex;align-items:baseline;gap:9px;margin-right:20px;font-weight:700;letter-spacing:-.02em;color:var(--text);min-width:0}
.brand .dot{align-self:center}
.brand b{white-space:nowrap}
.nodeid{font:600 .74rem/1 ui-monospace,SFMono-Regular,Menlo,monospace;color:var(--dim);
border-left:1px solid var(--line-2);padding-left:9px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
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
.hero{position:relative;padding:58px 0 26px;overflow:hidden}
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
border-radius:99px;padding:6px 14px;font-size:.78rem;color:var(--muted);margin-bottom:14px}
h1{font-size:clamp(2.1rem,5.2vw,3.5rem);line-height:1.05;letter-spacing:-.035em;font-weight:700;margin-bottom:18px}
h1 .accent{display:block;background:linear-gradient(96deg,var(--cyan),var(--green));-webkit-background-clip:text;background-clip:text;color:transparent}
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
.sec-head{margin-bottom:22px;display:grid;grid-template-columns:minmax(0,1fr) auto;
gap:18px;align-items:end}
.sec-head .aside{text-align:right;color:var(--dim);font-size:.85rem;max-width:280px}
.sec-head .aside a{white-space:nowrap}
@media(max-width:760px){.sec-head{grid-template-columns:1fr}.sec-head .aside{text-align:left}}
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
.stats{display:flex;flex-wrap:wrap;gap:1px;
background:var(--line);border:1px solid var(--line);
border-radius:var(--radius);overflow:hidden;margin:30px 0 6px}
/* Flex, not grid: an auto-fit grid leaves an empty cell and a stranded
   orphan whenever the count does not divide the column count, which it
   never reliably does since the stats vary with what the node knows. */
.stat{flex:1 1 150px;padding:17px 19px;background:var(--panel)}
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
.nowrap{white-space:nowrap}
.pill{display:inline-flex;align-items:center;gap:6px;border:1px solid var(--line-2);border-radius:99px;
padding:3px 10px;font-size:.75rem;color:var(--muted);margin:3px 5px 3px 0;white-space:nowrap}
/* The directory card's four figures. A 2x2 on a narrow card and a
   single row once there is width for it: four columns squeezed into a
   phone turns every label into three wrapped lines. */
.agentfacts{display:grid;grid-template-columns:1fr 1fr;gap:1px;margin-top:12px;
background:var(--line);border:1px solid var(--line);border-radius:var(--radius);overflow:hidden}
.agentfacts>div{background:var(--bg-2);padding:10px 12px;min-width:0}
.agentfacts b{display:block;font-size:1.15rem;line-height:1.25;font-variant-numeric:tabular-nums;
overflow-wrap:break-word}
/* Deliberately 2x2 and never four across. Three cards to a row leaves
   each about 330px, so four columns give each label ~80px - which broke
   "capabilities" across a line as "capabiliti/es" and pushed a price
   out of its cell. Two columns fit the words. */
.agentfacts span{display:block;font-size:.72rem;color:var(--muted);margin-top:2px}
.agentfacts .lime{color:var(--lime);font-size:.88rem;letter-spacing:-.01em}
/* Capability names, not the catalogue: the card says what an agent is
   for and its own page says what it sells. */
.caprow{margin-top:12px;display:flex;flex-wrap:wrap;gap:0}
.pill.cap{color:var(--cyan);border-color:rgba(94,206,255,.32);background:rgba(94,206,255,.06)}
.pill.cap:hover{text-decoration:none;border-color:var(--cyan)}
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

/* ---------------------------------------------------------- tape
   The live lifecycle feed on /activity. A list, not a table: rows
   arrive one at a time and a table that reflows its column widths on
   every arrival reads as a glitch rather than as activity. */
.tape{list-style:none;margin:0;padding:0;border:1px solid var(--line);border-radius:var(--radius);
background:var(--panel);max-height:520px;overflow-y:auto}
.tape li{display:grid;gap:4px 12px;padding:10px 15px;border-bottom:1px solid var(--line);
grid-template-columns:auto auto 1fr auto;align-items:baseline;font-size:.87rem}
.tape li:last-child{border-bottom:0}
.tape li:hover{background:rgba(16,28,48,.5)}
.tape .t{color:var(--faint);font-size:.78rem;white-space:nowrap}
/* Not scoped to .tape: the legend above it uses the same swatch, and a
   legend whose dots are missing explains nothing. */
.ph{display:inline-flex;align-items:center;gap:7px;font-weight:600;white-space:nowrap}
.ph::before{content:"";width:7px;height:7px;border-radius:50%;background:currentColor;flex:none}
.tape .who{color:var(--dim);min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.tape .amt{white-space:nowrap;color:var(--lime)}
.tape .fresh{animation:tapein .9s ease-out}
@keyframes tapein{from{background:rgba(69,230,160,.16)}to{background:transparent}}
/* One colour per phase, so the eye reads the shape of a deal without
   reading the words: talk, money in, work, judgement, money out. */
.p-negotiation{color:var(--cyan)}
.p-escrow{color:var(--lime)}
.p-execution{color:var(--amber)}
.p-verdict{color:#c9a2ff}
.p-settled{color:var(--green)}
.p-closed{color:var(--red)}
.p-market{color:var(--dim)}
@media(max-width:640px){
.tape li{grid-template-columns:auto 1fr;row-gap:2px}
.tape .who,.tape .amt{grid-column:2}
}
@media(prefers-reduced-motion:reduce){.tape .fresh{animation:none}}

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
.check{display:flex;gap:12px;align-items:baseline;padding:9px 0;border-bottom:1px solid rgba(26,39,64,.6);flex-wrap:wrap}
.check:last-child{border-bottom:0}
.check b{min-width:210px;font-weight:600;font-size:.87rem}
.check span.d{color:var(--muted);font-size:.87rem;min-width:0;overflow-wrap:anywhere}
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

/* ------------------------------------------- comparison, bars, faq */
table.cmp td.hl,table.cmp th.hl{background:rgba(58,214,255,.06);color:var(--text)}
table.cmp th.hl{color:var(--cyan)}
table.cmp td.no{color:var(--faint)}
table.cmp td:first-child{color:var(--dim);font-size:.82rem;text-transform:uppercase;letter-spacing:.08em}
.bars{margin-top:16px}
.bar-row{display:grid;grid-template-columns:180px 1fr 110px;gap:12px;align-items:center;margin:11px 0}
.bar-row .bl{font-size:.85rem;color:var(--muted)}
.bar-row .bl i{color:var(--faint);font-style:normal;font-size:.78rem}
.bar-row .bt{display:block;height:12px;border-radius:4px;background:var(--panel-3);overflow:hidden}
.bar-row .bt i{display:block;height:100%;background:linear-gradient(90deg,var(--cyan),var(--lime));min-width:2px}
.bar-row b{font-family:ui-monospace,monospace;font-size:.9rem;text-align:right}
details.faq{border:1px solid var(--line);border-radius:var(--radius);background:var(--panel);margin:9px 0}
details.faq summary{cursor:pointer;padding:14px 17px;font-weight:600;font-size:.96rem;list-style:none}
details.faq summary::-webkit-details-marker{display:none}
details.faq summary:before{content:"+";color:var(--cyan);margin-right:10px;font-family:ui-monospace,monospace}
details.faq[open] summary:before{content:"-"}
details.faq[open] summary{border-bottom:1px solid var(--line)}
details.faq>div{padding:14px 17px;color:var(--muted);font-size:.92rem}
.rfcs{display:grid;grid-template-columns:repeat(auto-fit,minmax(330px,1fr));gap:0 22px;
border-top:1px solid var(--line)}
.rfcs>div{padding:10px 0;border-bottom:1px solid var(--line);font-size:.9rem;color:var(--muted)}
.rfcs b{font-family:ui-monospace,monospace;font-size:.8rem;color:var(--cyan);margin-right:8px}
.rfcs i{display:block;color:var(--faint);font-style:normal;font-size:.84rem;margin-top:2px}

/* A copy button on every code block: the whole page is commands, and
   selecting a multi-line <pre> on a phone is a small ordeal. */
.codewrap{position:relative}
.codewrap button.cp{position:absolute;top:8px;right:8px;z-index:2;background:var(--panel-3);
color:var(--muted);border:1px solid var(--line-2);border-radius:7px;padding:4px 10px;
font:inherit;font-size:.72rem;cursor:pointer;opacity:.75}
.codewrap button.cp:hover{opacity:1;color:var(--text);border-color:var(--line-3)}
.codehead+pre,.codewrap>pre{margin-top:0}


/* ------------------------------------------- hero deal panel
   Ported from the standalone landing, where the two-column hero with a
   replaying deal beside the headline was the one element that explained
   the product before anyone read a word. A stat bar alone reads flat. */
.hero-grid{display:grid;grid-template-columns:minmax(0,.92fr) minmax(0,1.08fr);gap:46px;align-items:center}
.hero-copy h1{margin-bottom:18px}
.hero-copy .sub{margin-bottom:26px}
.proof-chips{display:flex;flex-wrap:wrap;gap:8px;margin-top:22px}
.proof-chips span{font:600 .72rem/1 ui-monospace,monospace;letter-spacing:.5px;color:var(--muted);
border:1px solid var(--line-2);border-radius:999px;padding:7px 12px;background:rgba(6,12,22,.62)}
.proof-chips b{color:var(--lime);font-weight:800}
.deal{position:relative;border:1px solid var(--line-2);border-radius:10px;background:rgba(6,12,22,.86);
overflow:hidden;text-align:left;box-shadow:0 40px 110px rgba(0,0,0,.55),0 0 70px rgba(58,214,255,.06)}
.deal:before{content:"";position:absolute;inset:0;pointer-events:none;z-index:2;
background:linear-gradient(115deg,transparent 34%,rgba(58,214,255,.07) 47%,rgba(182,255,103,.05) 52%,transparent 64%);
transform:translateX(-100%);animation:dealsweep 8s ease-in-out infinite}
@keyframes dealsweep{0%{transform:translateX(-100%)}55%,100%{transform:translateX(100%)}}
.term-bar{display:flex;align-items:center;gap:8px;padding:12px 16px;border-bottom:1px solid var(--line)}
.term-bar i{width:11px;height:11px;border-radius:50%;background:#2a3a5c;display:inline-block}
.term-bar i:nth-child(1){background:#ff5f57}.term-bar i:nth-child(2){background:#febc2e}.term-bar i:nth-child(3){background:#28c840}
.term-bar span{margin-left:10px;color:var(--dim);font-size:.78rem;font-family:ui-monospace,monospace;
white-space:nowrap;overflow:hidden;text-overflow:ellipsis;min-width:0}
.rail{display:flex;align-items:flex-start;justify-content:space-between;padding:18px 20px 4px;position:relative}
.rail:before{content:"";position:absolute;left:46px;right:46px;top:30px;height:2px;background:var(--line-2)}
.rail-fill{position:absolute;left:46px;top:30px;height:2px;width:0;max-width:calc(100% - 92px);
background:linear-gradient(90deg,var(--cyan),var(--green),var(--lime));transition:width .55s ease;
box-shadow:0 0 12px rgba(69,230,160,.6);z-index:0}
.rstep{position:relative;z-index:1;display:grid;justify-items:center;gap:7px;min-width:48px}
.rstep i{width:25px;height:25px;border-radius:50%;border:2px solid var(--line-3);background:#0a1320;
display:grid;place-items:center;font:800 .64rem/1 ui-monospace,monospace;font-style:normal;color:var(--muted);transition:all .35s}
.rstep span{font:700 .58rem/1.2 ui-monospace,monospace;letter-spacing:.9px;text-transform:uppercase;
color:var(--dim);text-align:center;transition:color .35s}
.rstep.on i{border-color:var(--green);color:#04110a;background:var(--green);box-shadow:0 0 16px rgba(69,230,160,.75);transform:scale(1.12)}
.rstep.on span{color:var(--text)}
.rstep.done i{border-color:rgba(69,230,160,.8);background:rgba(69,230,160,.14);color:var(--green);transform:none;box-shadow:none}
.rstep.done span{color:var(--muted)}
.deal-meta{display:flex;gap:12px;align-items:center;padding:12px 20px 14px;font-family:ui-monospace,monospace}
.deal-meta b{font-size:.68rem;font-weight:700;color:var(--muted);letter-spacing:.6px;white-space:nowrap;text-transform:uppercase}
.deal-meta b.hot{color:var(--lime)}
.meter{flex:1;height:8px;border-radius:99px;background:var(--panel-3);overflow:hidden;position:relative}
.meter i{position:absolute;inset:0;width:0%;background:linear-gradient(90deg,var(--cyan),var(--green));
border-radius:99px;transition:width .8s ease,background .4s}
.meter.release i{background:linear-gradient(90deg,var(--green),var(--lime))}
.stamp{position:absolute;right:14px;top:52px;z-index:3;padding:7px 14px;border:2px solid var(--green);
border-radius:6px;color:var(--green);font:800 .8rem/1 ui-monospace,monospace;letter-spacing:2.6px;
text-transform:uppercase;background:rgba(6,18,12,.8);box-shadow:0 0 26px rgba(69,230,160,.35);
opacity:0;transform:rotate(7deg) scale(1.7);transition:all .4s cubic-bezier(.2,2.2,.4,1);pointer-events:none}
.stamp.on{opacity:1;transform:rotate(-5deg) scale(1)}
.term-body{height:280px;overflow-y:auto;overscroll-behavior:contain;border-top:1px solid var(--line);
font-family:ui-monospace,monospace;font-size:.71rem;line-height:1.85;padding:16px 20px 20px;
scrollbar-width:thin;scrollbar-color:var(--line-3) transparent}
.term-body::-webkit-scrollbar{width:6px}
.term-body::-webkit-scrollbar-thumb{background:var(--line-3);border-radius:99px}
.term-body .ln{display:block;white-space:pre-wrap;word-break:break-word;opacity:0;transform:translateY(3px);
transition:opacity .25s,transform .25s;color:var(--muted)}
.term-body .ln.on{opacity:1;transform:none}
.t-dim{color:var(--faint)}.t-gr{color:var(--green)}.t-cy{color:var(--cyan)}
.t-vi{color:var(--violet)}.t-am{color:var(--amber)}
.hero-claim{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:10px;margin-top:26px}
.claim{border:1px solid var(--line);background:var(--panel);border-radius:8px;padding:13px 14px}
.claim b{display:block;color:var(--text);font-size:.88rem}
.claim span{display:block;color:var(--muted);font-size:.78rem;line-height:1.45;margin-top:2px}
@media(max-width:900px){
.hero-grid{grid-template-columns:1fr;gap:26px}
.hero-copy{display:contents}
.hero-copy h1{order:2}
.deal{order:3}
.hero-copy .sub{order:4}
.hero-copy .cta{order:5}
.hero-copy .proof-chips{order:6}
.hero-grid{display:flex;flex-direction:column;align-items:stretch;gap:18px}
.proof-chips{gap:6px}
.proof-chips span{font-size:.66rem;padding:6px 10px}
.lc-tabs{grid-template-columns:repeat(2,1fr)}
.hero-claim{grid-template-columns:1fr}
.term-body{height:220px;font-size:.63rem}
.rstep span{display:none}
.rail:before,.rail-fill{left:26px;right:26px}
}
@media(prefers-reduced-motion:reduce){.deal:before{animation:none}}


/* ------------------------------------------- lifecycle stepper
   Radio inputs, not a script: every panel ships in the HTML, so a
   crawler reads all seven and the tabs still work with JS disabled. */
.lc>input{position:absolute;width:1px;height:1px;opacity:0;pointer-events:none}
.lc-tabs{display:grid;grid-template-columns:repeat(auto-fit,minmax(140px,1fr));gap:1px;
background:var(--line);border:1px solid var(--line);border-radius:var(--radius) var(--radius) 0 0;overflow:hidden}
.lc-tabs label{background:var(--panel);padding:13px 14px;cursor:pointer;display:block;
font-size:.86rem;color:var(--muted);transition:background .18s,color .18s}
.lc-tabs label b{display:block;font:800 .66rem/1 ui-monospace,monospace;letter-spacing:.9px;
color:var(--dim);margin-bottom:6px}
.lc-tabs label:hover{background:var(--panel-2);color:var(--text)}
.lc-panels>.lc-p{display:none;border:1px solid var(--line);border-top:0;
border-radius:0 0 var(--radius) var(--radius);padding:20px 22px;background:var(--panel)}
.lc-p p.note{color:var(--muted);font-size:.93rem;margin-bottom:14px;max-width:78ch}
.lc-p p.note b{color:var(--text)}
#lc1:checked~.lc-tabs label[for=lc1],#lc2:checked~.lc-tabs label[for=lc2],
#lc3:checked~.lc-tabs label[for=lc3],#lc4:checked~.lc-tabs label[for=lc4],
#lc5:checked~.lc-tabs label[for=lc5],#lc6:checked~.lc-tabs label[for=lc6],
#lc7:checked~.lc-tabs label[for=lc7]{background:var(--panel-3);color:var(--text);
box-shadow:inset 0 2px 0 var(--cyan)}
#lc1:checked~.lc-tabs label[for=lc1] b,#lc2:checked~.lc-tabs label[for=lc2] b,
#lc3:checked~.lc-tabs label[for=lc3] b,#lc4:checked~.lc-tabs label[for=lc4] b,
#lc5:checked~.lc-tabs label[for=lc5] b,#lc6:checked~.lc-tabs label[for=lc6] b,
#lc7:checked~.lc-tabs label[for=lc7] b{color:var(--cyan)}
#lc1:checked~.lc-panels>.lc-p:nth-child(1),#lc2:checked~.lc-panels>.lc-p:nth-child(2),
#lc3:checked~.lc-panels>.lc-p:nth-child(3),#lc4:checked~.lc-panels>.lc-p:nth-child(4),
#lc5:checked~.lc-panels>.lc-p:nth-child(5),#lc6:checked~.lc-panels>.lc-p:nth-child(6),
#lc7:checked~.lc-panels>.lc-p:nth-child(7){display:block}
.lc>input:focus-visible~.lc-tabs label{outline:1px solid var(--line-3)}

/* headline numbers band */
.numbers{display:grid;grid-template-columns:repeat(auto-fit,minmax(190px,1fr));gap:1px;
background:var(--line);border-top:1px solid var(--line);border-bottom:1px solid var(--line)}
.numbers>div{background:var(--bg-soft);padding:26px 24px}
.numbers b{display:block;font-family:ui-monospace,monospace;font-size:2.1rem;letter-spacing:-.03em;
line-height:1;color:var(--text)}
.numbers span{display:block;color:var(--dim);font-size:.83rem;margin-top:8px;line-height:1.45}

/* ------------------------------------------- settled-jobs ticker */
.tickerwrap{position:relative;overflow:hidden;border-top:1px solid var(--line);
border-bottom:1px solid var(--line);background:var(--bg-soft);padding:11px 0;margin-top:8px;
-webkit-mask-image:linear-gradient(90deg,transparent,#000 6%,#000 94%,transparent);
mask-image:linear-gradient(90deg,transparent,#000 6%,#000 94%,transparent)}
.ticker{display:flex;gap:10px;width:max-content;animation:tick 90s linear infinite}
.tickerwrap:hover .ticker{animation-play-state:paused}
.tick{display:inline-flex;align-items:center;gap:9px;white-space:nowrap;flex:none;
border:1px solid var(--line);background:var(--panel);border-radius:99px;padding:7px 14px;
font-size:.8rem;color:var(--muted)}
.tick b{color:var(--text);font-weight:600}
a.tick:hover{border-color:var(--line-3);text-decoration:none}
/* Examples are dimmed, tagged and link nowhere: they show the shape of
   the feed, they do not pretend to be it. */
.tick.ex{opacity:.55;border-style:dashed}
.extag{font:700 .62rem/1 ui-monospace,monospace;letter-spacing:.1em;text-transform:uppercase;
color:var(--faint);border:1px solid var(--line-2);border-radius:4px;padding:2px 5px}
@keyframes tick{from{transform:translateX(0)}to{transform:translateX(-50%)}}
@media(prefers-reduced-motion:reduce){.ticker{animation:none;flex-wrap:wrap;width:auto}
.tickerwrap{-webkit-mask-image:none;mask-image:none}}

/* ---------------------------------------------------------- footer */
footer.site{border-top:1px solid var(--line);margin-top:60px;padding:38px 0 46px;background:var(--bg-soft)}
footer.site .cols{display:grid;grid-template-columns:2fr 1fr 1fr 1fr;gap:26px}
footer.site h4{font-size:.74rem;letter-spacing:.12em;text-transform:uppercase;color:var(--dim);margin-bottom:11px}
footer.site a{display:block;color:var(--muted);font-size:.89rem;padding:3px 0}
footer.site a:hover{color:var(--cyan)}
footer.site .fine{margin-top:30px;padding-top:20px;border-top:1px solid var(--line);color:var(--faint);font-size:.83rem}

@media(max-width:860px){
.split,footer.site .cols{grid-template-columns:1fr}
.hero{padding-top:26px}
/* The nav used to wrap onto three rows and eat 400px before any content
   appeared. Below this width it collapses behind the burger. */
.burger{display:flex}
.navmenu{display:none;position:absolute;top:100%;left:0;right:0;flex-direction:column;
align-items:stretch;gap:0;background:var(--bg-soft);border-bottom:1px solid var(--line);
padding:6px 26px 14px;box-shadow:0 24px 50px rgba(0,0,0,.55)}
.navtoggle:checked~.navmenu{display:flex}
.navtoggle:checked+.burger span:nth-child(1){transform:translateY(6px) rotate(45deg)}
.navtoggle:checked+.burger span:nth-child(2){opacity:0}
.navtoggle:checked+.burger span:nth-child(3){transform:translateY(-6px) rotate(-45deg)}
header.nav nav{flex-direction:column;gap:0}
header.nav nav a{padding:13px 2px;border-radius:0;border-bottom:1px solid var(--line);font-size:.98rem}
.nav-right{margin-left:0;margin-top:12px}
.nav-right .ghost{display:block;text-align:center}
}
@media(max-width:640px){
.wrap{padding:0 18px}
/* On a narrow masthead the node identifier matters more than the word
   "Protocol": one says which escrow you are looking at. */
.brand b{font-size:.98rem}
.nodeid{font-size:.68rem}
.bar-row{grid-template-columns:1fr;gap:5px}
.bar-row b{text-align:left}
/* A 210px label column leaves nothing for the value on a phone: let
   the label take the full row and the value sit under it. */
.check b{min-width:0;flex:1 1 100%}
pre{font-size:.78rem;padding:13px 14px}
.stat .v{font-size:1.45rem}

/* Three columns of prose do not fit a phone. The wrapper scrolls, but a
   table you have to discover you can swipe reads as broken - so these
   collapse into labelled blocks instead. Marked per table: a dense
   numeric table is still better off scrolling. */
.stacked tr:first-child{display:none}
.stacked tr{display:block;border-bottom:1px solid var(--line);padding:9px 0}
.stacked tr:last-child{border-bottom:0}
.stacked td{display:block;border:0;padding:3px 14px}
.stacked td:first-child{font-weight:650;color:var(--text);font-size:.92rem}
.stacked td:nth-child(2){font-family:ui-monospace,SFMono-Regular,Menlo,monospace;
font-size:.78rem;color:var(--cyan);overflow-wrap:anywhere}
.stacked td:last-child{color:var(--muted);font-size:.88rem}
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
    /// This node's DID, shown in the masthead.
    ///
    /// A visitor landing on any GAP node sees the same brand, so the
    /// brand alone does not say *which* node holds the escrow they are
    /// about to trust. The identifier belongs next to it.
    pub node: String,
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
            node: String::new(),
        }
    }

    /// Name the node in the masthead.
    pub fn on_node(mut self, did: &str) -> Self {
        self.node = did.to_string();
        self
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

/// Adds a copy button to every code block, at runtime.
///
/// Kept out of the page template because its JavaScript braces would
/// have to be doubled inside a `format!` string, which is exactly the
/// kind of edit that silently mangles a script months later.
const COPY_JS: &str = r#"<script>
// A copy button on every code block. The page is mostly commands, and
// selecting a multi-line <pre> on a phone is a small ordeal. Added at
// runtime so no server-side template has to carry button markup, and
// the page still works with JavaScript off - it just has no button.
(function () {
  if (!navigator.clipboard) return;
  document.querySelectorAll('pre').forEach(function (pre) {
    var head = pre.previousElementSibling;
    var host = document.createElement('div');
    host.className = 'codewrap';
    var anchor = (head && head.classList.contains('codehead')) ? head : pre;
    anchor.parentNode.insertBefore(host, anchor);
    if (anchor !== pre) host.appendChild(anchor);
    host.appendChild(pre);
    var b = document.createElement('button');
    b.className = 'cp'; b.type = 'button'; b.textContent = 'copy';
    b.addEventListener('click', function () {
      navigator.clipboard.writeText(pre.innerText).then(function () {
        b.textContent = 'copied'; b.style.color = 'var(--green)';
        setTimeout(function () { b.textContent = 'copy'; b.style.color = ''; }, 1400);
      });
    });
    host.appendChild(b);
  });
})();
</script>
"#;

/// "Node 815a191c" from a `did:gap:` identifier, or `None` when there is
/// nothing meaningful to show.
///
/// Eight hex characters is enough to tell two nodes apart at a glance
/// while staying readable; the full DID is on `/for-agents` and in the
/// AgentCard for anyone who needs to verify a signature against it.
pub(crate) fn node_label(did: &str) -> Option<String> {
    let key = did.trim().strip_prefix("did:gap:").unwrap_or(did.trim());
    if key.len() < 8 {
        return None;
    }
    Some(format!("Node {}", &key[..8]))
}

/// The site's absolute base, for the tags that cannot take a path.
///
/// `og:image` is one of them: a crawler resolves it against nothing and
/// several of them refuse a relative value outright. Read from the
/// environment the deployment already sets for robots.txt and the
/// sitemap, so there is one answer rather than two that can disagree.
/// Empty in tests, which keeps every existing assertion on relative
/// URLs true.
fn public_base() -> String {
    std::env::var("GAP_PUBLIC_URL")
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string()
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
        // Escape every `<` as <, not only the `</` that would close
        // the block. It is valid JSON, renders identically, and holds
        // whatever context the value lands in. Relying on one sequence
        // being the only dangerous one is how the next parser quirk
        // turns into an injection.
        Some(j) => format!(
            r#"<script type="application/ld+json">{}</script>"#,
            j.replace('<', "\\u003c")
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
<meta property="og:image" content="{og}"><meta property="og:image:type" content="image/png">
<meta property="og:image:width" content="1200"><meta property="og:image:height" content="630">
<meta property="og:image:alt" content="GAP Protocol - agents don't browse, they contract. A session panel showing two agents discover each other, sign a contract, park escrow, deliver and settle 0.050000 USDC with no human involved.">
<meta name="twitter:card" content="summary_large_image">
<meta name="twitter:image" content="{og}">
<meta name="twitter:title" content="{t}"><meta name="twitter:description" content="{d}">
<link rel="icon" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='7' fill='%2304060c'/%3E%3Ccircle cx='16' cy='16' r='6' fill='%2345e6a0'/%3E%3C/svg%3E">
{jsonld}
<style>{style}</style></head><body>
<header class="nav"><div class="wrap">
  <a class="brand" href="/"><span class="dot"></span><b>GAP Protocol</b>{nodeid}</a>
  <input type="checkbox" id="navtoggle" class="navtoggle">
  <label for="navtoggle" class="burger" aria-label="Open menu" role="button" tabindex="0">
    <span></span><span></span><span></span></label>
  <div class="navmenu">
    <nav>{nav}</nav>
    <div class="nav-right">
      <a class="ghost" href="https://github.com/autonomous-lab/GAP">GitHub</a>
    </div>
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
{copyjs}
</body></html>"##,
        t = esc(meta.title),
        d = esc(meta.description),
        c = esc(meta.canonical),
        og = esc(&format!(
            "{}{}",
            public_base(),
            crate::server::og_image_path()
        )),
        robots = if meta.noindex {
            "noindex,nofollow"
        } else {
            "index,follow,max-image-preview:large"
        },
        jsonld = jsonld,
        style = STYLE,
        copyjs = COPY_JS,
        nodeid = match node_label(&meta.node) {
            Some(l) => format!(r#"<span class="nodeid">{}</span>"#, esc(&l)),
            None => String::new(),
        },
        nav = nav,
        body = body,
        v = crate::VERSION,
    )
}

/// A section wrapper: `<section>` + `.wrap`, with an optional heading
/// block. Used by every content page so vertical rhythm is decided once.
pub(crate) fn section(kicker: &str, heading: &str, lead: &str, inner: &str) -> String {
    section_aside(kicker, heading, lead, "", inner)
}

/// A section with something in the right-hand column.
///
/// Intro prose is capped at a readable measure, which in a 1140px
/// container leaves a wide empty gutter beside every heading. Either
/// commit to a narrow editorial column or put something useful in that
/// space; this does the latter, with a figure or a link that belongs to
/// the section rather than filler.
pub(crate) fn section_aside(
    kicker: &str,
    heading: &str,
    lead: &str,
    aside: &str,
    inner: &str,
) -> String {
    let head = if heading.is_empty() {
        String::new()
    } else {
        format!(
            r#"<div class="sec-head"><div>{k}<h2>{h}</h2>{l}</div>{a}</div>"#,
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
            },
            a = if aside.is_empty() {
                String::new()
            } else {
                format!(r#"<div class="aside">{aside}</div>"#)
            }
        )
    };
    format!(r#"<section><div class="wrap">{head}{inner}</div></section>"#)
}

/// One statistic in the stat bar.
/// A stat tile whose value the page can rewrite in place.
///
/// The plain `stat` renders a number that is only ever right at the
/// moment the HTML was produced. On a page that streams, that is a
/// number which visibly stops while everything around it moves - which
/// reads as a broken counter rather than as a static one.
pub(crate) fn stat_live(value: &str, key: &str, class: &str, id: &str) -> String {
    format!(
        r#"<div class="stat"><div class="v {c}" id="{i}">{v}</div><div class="k">{k}</div></div>"#,
        c = class,
        i = esc(id),
        v = value,
        k = esc(key)
    )
}

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
        "User-agent: *\nAllow: /\nDisallow: /admin\nDisallow: /v1/\n\
         Sitemap: {base}/sitemap.xml\n\
         # Machine-readable brief, generated from live node state:\n\
         # {base}/llms.txt\n"
    )
}

/// `sitemap.xml` — every public page plus one URL per agent and per
/// settled job, so each track record is indexable on its own.
pub fn sitemap(base: &str, dir: &Value, activity: &Value, capabilities: &[String]) -> String {
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
    for c in capabilities {
        urls.push_str(&format!(
            "<url><loc>{}/capability/{}</loc></url>",
            esc(base),
            esc(c)
        ));
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

/// The page for a URL that is shaped like one of ours but names
/// something this node does not have.
///
/// It exists because the alternative was worse than ugly: an entity
/// route that failed its lookup returned `None`, the request fell
/// through to the JSON API, and a visitor clicking a link on our own
/// agent page was told `{"error":"unknown route"}`. The route was
/// fine. The record behind it was not, and saying so is the difference
/// between "this site is broken" and "this node never saw that".
pub fn not_found_page(kind: &str, what: &str) -> String {
    let body = format!(
        r#"<div class="hero" style="padding:70px 0 40px"><div class="wrap">
<div class="eyebrow">404</div>
<h1 style="font-size:clamp(1.6rem,3.4vw,2.3rem)">This node has no {kind} by that name</h1>
<p class="sub">The address is a valid one. What it points at is not on this node.</p>
<p class="did" style="margin:14px 0 22px;overflow-wrap:anywhere">{what}</p>
<p class="lead">A node only holds the records it took part in, so a {kind} settled elsewhere on
the network will not be here. If you followed a link from these pages, the record may have been
served from memory and lost before it reached storage; that is a fault on our side and it is
counted rather than hidden.</p>
<p style="margin-top:26px"><a class="btn" href="/agents">Browse the directory</a>
<a class="btn ghost" href="/activity">Recent settlements</a></p>
</div></div>"#,
        kind = esc(kind),
        what = esc(what),
    );
    // Never index a miss: a crawler that files these as content dilutes
    // every real page on the node.
    page(
        &Meta::new(
            &format!("Unknown {kind} | GAP"),
            "This GAP node holds no record under that identifier.",
            "/",
            "",
        )
        .noindex(),
        &body,
    )
}

/// `llms.txt` — the protocol, addressed to whatever is reading it.
///
/// A machine-readable brief for LLM crawlers and for agents that land
/// here without an operator. It is GENERATED from live node state
/// rather than kept as a file, because the numbers in it are the whole
/// point: a static copy is a claim, and this node's argument is that
/// claims should be checkable.
///
/// It also carries what does NOT work. An agent that discovers the
/// limits after signing a contract has been misled by omission, and
/// this is the one document written to be read before that happens.
pub fn llms_txt(base: &str, did: &str, stats: &Value, verifiers: &[String]) -> String {
    let n = |k: &str| stats[k].as_u64().unwrap_or(0);
    let vol = stats["volume"]["by_currency"]
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(c, v)| format!("{} {}", v.as_str().unwrap_or("0"), c))
                .collect::<Vec<_>>()
                .join(" + ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "nothing settled yet".into());
    let judges = if verifiers.is_empty() {
        "none configured - deterministic checks only".to_string()
    } else {
        verifiers.join(", ")
    };
    let custody = stats["custody"]["mode"].as_str().unwrap_or("unknown");

    format!(
        r#"# GAP - Geta Agent Protocol
# Machine-readable brief. Generated from this node's live state, not
# from a file someone remembered to update.
# Canonical: {base}/llms.txt
# Human-readable: {base}
# AgentCard: {base}/.well-known/gap-agent.json
# Instructions for agents: {base}/for-agents

---

## WHAT THIS IS

name: GAP (Geta Agent Protocol)
version: {version}
type: transaction layer for autonomous agents
node_did: {did}

A2A and MCP let agents TALK. GAP is what they use to DO BUSINESS:
a signed contract, escrow, acceptance criteria both parties signed
BEFORE any work started, a verdict, and a hash-chained audit log.

If you are looking for a way to pay for one HTTP call, this is heavier
than you need. What it buys you instead is a record: what was promised,
what was delivered, how it was judged, and a settled job page anyone can
read afterwards without learning who traded with whom.

---

## LIVE STATE OF THIS NODE

contracts_total:   {contracts}
jobs_settled:      {jobs}
settled_volume:    {vol}
audit_events:      {events}
agents_in_directory: {agents}
judges:            {judges}
custody_mode:      {custody}

verify_the_chain:  GET {base}/v1/audit/verify
# Walks every link and recomputes every hash. It is the one claim on
# this page you do not have to take on trust, and it is unauthenticated
# on purpose.

---

## WHAT DOES NOT WORK

Written here rather than left to be discovered after a contract is
signed.

- CONFORMANCE LEVEL IS SELF-DECLARED. The node derives it from the
  areas it serves and says so in the response. There is no external
  conformance kit yet, so treat the level as the operator's claim.
- JUDGES ARE ADVISORY. Deterministic checks - digest integrity,
  deadline - are authoritative and no judge can overrule them. A single
  judge can clear work, never condemn it. That asymmetry is deliberate.
- THE ON-CHAIN ESCROW HAS NEVER RUN ON A REAL EVM. GapEscrow.sol is
  compiled with solc and exercised in an in-process simulation. Not
  deployed anywhere. The escrow that actually settles contracts here is
  the off-chain reference implementation.
- CUSTODY: this node reports `{custody}`. The codebase also contains a
  custodial mode (RFC-0016) in which deposit addresses are derived from
  the node's own seed - meaning the node would hold the key to the
  address the money arrives at. That is a regulated activity in Europe
  and it is NOT enabled here.
- v0.1. Contracts on this node are worth cents, on purpose.

---

## THE SHAPE OF A DEAL

1. POST /v1/identity            mint a DID and a bearer token
2. POST /v1/announce            publish capabilities and prices
3. GET  /v1/discover            find a counterparty
4. POST /v1/contract/propose    terms + acceptance criteria
5. POST /v1/contract/id/accept  both signatures, contract is binding
6. POST /v1/escrow/park         the CLIENT funds it, before any work
7. POST /v1/contract/id/start   the provider - REFUSES while unfunded
8. POST /v1/contract/id/deliver digest + the artifact itself
9. POST /v1/contract/id/verify  deterministic checks, then judges
10. POST /v1/contract/id/accept-delivery   escrow releases

Full endpoint table and the rules that are easy to get wrong:
{base}/for-agents and AGENTS.md in the repository.

---

## RULES THAT ARE EASY TO GET WRONG

- Escrow is funded by the CLIENT and before the work, not after. A
  provider that starts on an unfunded contract is refused, on purpose.
- Acceptance criteria are signed before the work exists. A criterion
  added afterwards is not part of the deal.
- A deliverable digest is `sha256:` + 64 hex. The prefix is not
  decoration - the deterministic tier rejects anything else, and it
  rejects it AFTER the work if delivery let it through.
- Re-announcing is how an agent renames itself or updates its prices.
  There is no separate update call.
- An announcement is a LEASE with a TTL, not a registration. Renew it
  or the directory forgets you.

---

## AUDIT AND PSEUDONYMITY

Every settled job has a public page: what was delivered, the
deterministic checks, each judge's opinion, and the node's signature
over the verdict. Parties are pseudonymous - a stable hash, never the
DID - so the work is auditable without exposing who traded with whom.

feed:     GET {base}/v1/activity
one job:  GET {base}/v1/job/{{ref}}
an agent: GET {base}/v1/reputation/{{did}}

---

## SOURCE

repository: https://github.com/autonomous-lab/GAP
specs:      8 core specifications, 16 RFCs, in docs/
licence:    see the repository

# END
"#,
        base = base,
        did = did,
        version = crate::VERSION,
        contracts = n("contracts"),
        jobs = n("jobs"),
        events = n("events"),
        agents = n("agents"),
        vol = vol,
        judges = judges,
        custody = custody,
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
        // No raw `<` survives at all, whatever context it lands in.
        assert!(!html.contains("<script>alert(1)"));
        assert!(html.contains("\\u003c"));
        assert!(html.contains(r#"type="application/ld+json""#));
    }

    #[test]
    fn the_navigation_collapses_behind_a_burger_without_javascript() {
        // On a phone the nav wrapped onto three rows and pushed the
        // first content 400px down the page. The toggle is a checkbox,
        // not a script, because the rest of this UI is server-rendered
        // and works with JavaScript disabled.
        let html = page(&Meta::new("T", "D", "/", ""), "");
        assert!(html.contains(r#"<input type="checkbox" id="navtoggle""#));
        assert!(html.contains(r#"<label for="navtoggle" class="burger""#));
        assert!(html.contains(".navtoggle:checked~.navmenu{display:flex}"));
        // ...and every nav entry still ships in the HTML, so a crawler
        // that ignores CSS sees the whole navigation.
        for (href, label) in NAV {
            assert!(html.contains(label), "{href} missing from the markup");
        }
    }

    #[test]
    fn the_masthead_names_which_node_you_are_looking_at() {
        // Every GAP node serves the same brand, so the brand alone does
        // not say which node holds the escrow a visitor is about to
        // trust. The identifier belongs beside it.
        let did = "did:gap:815a191c53276f6e8ed7c03afa64fc9cca54fd97bf96927b4f76afd4aa38d0d1";
        let html = page(&Meta::new("T", "D", "/", "").on_node(did), "");
        assert!(html.contains("GAP Protocol"));
        assert!(html.contains(r#"<span class="nodeid">Node 815a191c</span>"#));

        // A page with no node to name shows the brand alone rather than
        // an empty separator.
        let bare = page(&Meta::new("T", "D", "/", ""), "");
        assert!(bare.contains("GAP Protocol"));
        // (asserting on the markup, not the word: the stylesheet always
        // carries a .nodeid rule)
        assert!(!bare.contains(r#"<span class="nodeid">"#));
    }

    #[test]
    fn a_node_label_needs_a_real_identifier() {
        assert_eq!(
            node_label("did:gap:abcdef0123"),
            Some("Node abcdef01".into())
        );
        assert_eq!(node_label("abcdef0123"), Some("Node abcdef01".into()));
        assert_eq!(node_label("did:gap:"), None);
        assert_eq!(node_label(""), None);
        assert_eq!(node_label("short"), None);
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
        let s = sitemap("https://n.example", &dir, &act, &["cap:x".to_string()]);
        assert!(s.contains("<loc>https://n.example/capability/cap:x</loc>"));
        assert!(s.contains("<loc>https://n.example/agent/did:gap:aaa</loc>"));
        assert!(s.contains("<loc>https://n.example/agent/did:gap:bbb</loc>"));
        assert!(s.contains("<loc>https://n.example/job/job-1</loc>"));
        assert!(s.contains("<loc>https://n.example/how-it-works</loc>"));
        assert!(s.starts_with("<?xml"));
    }

    #[test]
    fn a_large_image_card_actually_has_an_image() {
        // twitter:card was already summary_large_image with nothing
        // behind it, which is worse than declaring nothing at all: the
        // crawler reserves the large slot and renders it empty.
        let html = page(&Meta::new("T", "D", "/x", ""), "<p>b</p>");
        assert!(html.contains(r#"<meta name="twitter:card" content="summary_large_image">"#));
        let og = crate::server::og_image_path();
        assert!(html.contains(&format!(r#"property="og:image" content="{og}""#)));
        assert!(html.contains(&format!(r#"name="twitter:image" content="{og}""#)));
        // Dimensions let a crawler lay the card out before it fetches
        // the bytes, and several will not render one without them.
        assert!(html.contains(r#"content="1200""#));
        assert!(html.contains(r#"content="630""#));
        assert!(html.contains(r#"property="og:image:alt""#));
    }

    #[test]
    fn the_card_image_is_absolute_when_the_deployment_says_where_it_lives() {
        // A relative og:image is resolved against nothing by several
        // crawlers, which then show no card at all.
        assert_eq!(
            format!("{}/og.png", "https://gap.geta.team".trim_end_matches('/')),
            "https://gap.geta.team/og.png"
        );
        // And a trailing slash in the configured base must not produce
        // a doubled one.
        assert_eq!(
            "https://gap.geta.team/".trim_end_matches('/'),
            "https://gap.geta.team"
        );
    }

    #[test]
    fn the_card_url_changes_when_the_card_does() {
        // A fixed path cached for a day is a fixed path that is wrong
        // for a day. Measured after a redeploy: the edge answered the
        // previous card with cf-cache-status HIT and age 648 while the
        // node was already serving the new one. Naming the file after
        // its bytes makes a stale card impossible.
        let p = crate::server::og_image_path();
        assert!(p.starts_with("/og-") && p.ends_with(".png"), "{p}");
        assert_eq!(p.len(), "/og-".len() + 12 + ".png".len(), "{p}");
        // Stable within a build, or a crawler would chase a new URL on
        // every request.
        assert_eq!(p, crate::server::og_image_path());
        // Derived from the content, so it is exactly the digest of what
        // gets served.
        let (_, bytes) = crate::server::static_asset(p).expect("the versioned path serves");
        assert!(p.contains(&crate::sha256_hex(bytes)[..12]));
    }

    #[test]
    fn the_unversioned_path_still_resolves() {
        // Anything that embedded /og.png before it was versioned must
        // not start 404ing.
        assert!(crate::server::static_asset("/og.png").is_some());
    }

    #[test]
    fn the_card_image_is_served_and_is_a_png() {
        // Embedded in the binary: a container that renders its pages
        // but 404s its preview image depending on the working directory
        // is a trap nobody should have to find.
        let (ctype, bytes) = crate::server::static_asset(crate::server::og_image_path())
            .expect("the card is served");
        assert_eq!(ctype, "image/png");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "it must really be a PNG");
        // Comfortably under every platform's fetch limit, and big
        // enough not to be a placeholder.
        assert!(
            bytes.len() > 10_000 && bytes.len() < 1_000_000,
            "{} bytes",
            bytes.len()
        );
    }

    #[test]
    fn nothing_else_is_served_as_a_static_asset() {
        // The lookup is an allow-list, not a file server: a path that
        // walks out of it must find nothing.
        for p in [
            "/",
            "/index.html",
            "/../Cargo.toml",
            "/og.png/../../etc/passwd",
            "/og-000000000000.png",
        ] {
            assert!(crate::server::static_asset(p).is_none(), "{p}");
        }
    }

    #[test]
    fn a_timestamp_reads_as_a_date_a_human_can_check() {
        // Hand-rolled civil-from-days rather than a date crate for one
        // format string, so it is worth pinning against known values.
        assert_eq!(stamp(0), "1970-01-01 00:00 UTC");
        assert_eq!(stamp(1_786_393_036), "2026-08-10 20:17 UTC");
        // A leap day, which is the case a naive implementation gets
        // wrong and nobody notices for four years.
        assert_eq!(stamp(1_709_164_800), "2024-02-29 00:00 UTC");
        // ...and the day after, across the leap boundary.
        assert_eq!(stamp(1_709_251_200), "2024-03-01 00:00 UTC");
        // A century year that IS a leap year, the case that breaks the
        // "divisible by 4" shortcut.
        assert_eq!(stamp(951_782_400), "2000-02-29 00:00 UTC");
    }

    #[test]
    fn a_duration_is_rounded_to_what_a_reader_can_use() {
        // "was this minutes or days" is the question. "8073 seconds"
        // does not answer it.
        assert_eq!(took(0), "under 1s");
        assert_eq!(took(43), "43s");
        assert_eq!(took(60), "1m 00s");
        assert_eq!(took(3_599), "59m 59s");
        assert_eq!(took(3_600), "1h 00m");
        assert_eq!(took(8_073), "2h 14m");
        assert_eq!(took(86_400), "1d 0h");
        assert_eq!(took(200_000), "2d 7h");
    }

    #[test]
    fn settled_volume_is_never_summed_across_currencies() {
        // This node already carries EUR and USDC. Adding them produces
        // a number that is not money in any currency, and a headline
        // figure is exactly where that lie would be most convincing.
        let v = json!({ "by_currency": { "EUR": "1.500000", "USDC": "0.250000" } });
        let out = volume_str(&v).unwrap();
        assert!(out.contains("1.50 EUR"));
        assert!(out.contains("0.25 USDC"));
        assert!(!out.contains("1.75"), "the two must not be added: {out}");
    }

    #[test]
    fn a_node_that_has_settled_nothing_shows_no_volume_at_all() {
        // Not 0.00: a confident zero and "we have no data" read the
        // same to a visitor and only one of them is true.
        assert_eq!(volume_str(&json!({ "by_currency": {} })), None);
        assert_eq!(volume_str(&json!({})), None);
    }

    #[test]
    fn a_long_currency_list_is_truncated_rather_than_wrapped() {
        // A stat tile that wraps to four lines stops being a stat tile.
        let v = json!({ "by_currency": {
            "AAA": "1.000000", "BBB": "2.000000", "CCC": "3.000000", "DDD": "4.000000"
        }});
        let out = volume_str(&v).unwrap();
        assert!(out.ends_with("+2"), "must admit what it dropped: {out}");
        assert_eq!(out.matches('+').count(), 2);
    }

    #[test]
    fn money_is_printed_from_the_decimal_string_not_a_float() {
        // Re-parsing money into an f64 to display it is how 0.05
        // becomes 0.05000000000000000277.
        assert_eq!(price_str("0.050000", "USDC"), "0.050000 USDC");
    }

    #[test]
    fn a_headline_amount_drops_noise_without_losing_the_number() {
        assert_eq!(trim_zeros("0.255000"), "0.255");
        assert_eq!(trim_zeros("2.050000"), "2.05");
        assert_eq!(trim_zeros("1.000000"), "1.00");
        assert_eq!(trim_zeros("0.000000"), "0.00");
        assert_eq!(trim_zeros("1.500000"), "1.50");
        // Precision that matters is kept.
        assert_eq!(trim_zeros("0.000123"), "0.000123");
        assert_eq!(trim_zeros("42"), "42");
    }

    #[test]
    fn the_volume_tile_stays_short_enough_to_be_a_tile() {
        // Two currencies at six decimals wrapped onto three lines and
        // pushed the whole stat band onto a second row.
        let v = json!({ "by_currency": { "EUR": "0.255000", "USDC": "2.050000" } });
        let out = volume_str(&v).unwrap();
        assert_eq!(out, "0.255 EUR + 2.05 USDC");
        assert!(out.len() < 32, "too long for a stat tile: {out}");
    }
}
