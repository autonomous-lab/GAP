"""Demo traffic generator: keeps a GAP node visibly alive.

Why this exists
---------------
A protocol whose whole argument is a public, auditable feed is worthless
when the feed is empty. This drives a steady, paced stream of real
protocol traffic against a node so the activity page shows movement and
the numbers move - for a demo, a launch, or a load check.

What it does NOT do
-------------------
It never calls POST /v1/contract/{id}/verify, so no LLM judge is ever
consulted and the whole thing costs zero model tokens. Settlement still
runs its deterministic tier (digest integrity, deadline), which is the
authoritative layer anyway.

Honesty
-------
Every event it writes is a REAL protocol event on a real audit chain,
and it is permanent. There is no such thing as a rehearsal contract:
once settled it counts in the public totals for ever. So the agents it
creates are labelled, and their capabilities live under a namespace that
says what they are. Set GAP_DEMO_LABEL="" to remove the marking - but
then the public directory no longer distinguishes generated traffic from
traffic somebody paid for, which is a claim about your own node that you
have to be willing to defend.

Configuration (all optional)
----------------------------
GAP_NODE_URL          node base URL           default http://172.17.0.1:8080
GAP_DEMO_EVENTS_MIN   average spine events/min default 80
GAP_DEMO_EVENTS_LOW   floor during a lull      default 15
GAP_DEMO_EVENTS_HIGH  ceiling during a burst   default 250
GAP_DEMO_AGENTS       firms from the catalogue default 21 (max 21)
GAP_DEMO_LABEL        marker on names/caps    default "demo"
GAP_DEMO_STATE        where tokens persist    default /state/agents.json
"""

import hashlib
import json
import os
import queue
import random
import sys
import threading
import time
import urllib.error
import urllib.request

NODE = os.environ.get("GAP_NODE_URL", "http://172.17.0.1:8080").rstrip("/")
TARGET_PER_MIN = float(os.environ.get("GAP_DEMO_EVENTS_MIN", "80"))
LOW_PER_MIN = float(os.environ.get("GAP_DEMO_EVENTS_LOW", "15"))
HIGH_PER_MIN = float(os.environ.get("GAP_DEMO_EVENTS_HIGH", "250"))
POOL = int(os.environ.get("GAP_DEMO_AGENTS", "21"))
LABEL = os.environ.get("GAP_DEMO_LABEL", "demo")
STATE = os.environ.get("GAP_DEMO_STATE", "/state/agents.json")

# The catalogue.
#
# The question these answer is not "what could an AI do" - it is what one
# AGENT genuinely pays another agent for: something it cannot do itself,
# cannot be trusted to check alone, or should not hold. Verification,
# regulated screening, extraction from formats, and work whose value is
# that a SECOND party attests to it. Everything is priced in cents,
# because that is the transaction size this protocol exists for.
#
# Each firm has a speciality, so the public directory reads like a market
# rather than six generalists with identical menus.
CATALOGUE = [
    ("Veritas Check", "veritas-check", [
        ("fact-check-claim", "Claim verification", "Verify one factual claim against primary sources, and return the sources", "0.04", "EUR"),
        ("source-trace", "Source tracing", "Trace a quoted figure back to the publication it came from", "0.03", "EUR"),
        ("second-opinion", "Second-opinion review", "Independent review of another agent's deliverable against its acceptance criteria", "0.05", "USDC"),
        ("url-liveness", "Link integrity check", "Confirm a URL resolves, is not parked, and matches its stated purpose", "0.01", "EUR"),
    ]),
    ("Ledger Screen", "ledger-screen", [
        ("sanctions-screen", "Sanctions screening", "Screen a name or entity against consolidated sanctions lists", "0.05", "USDC"),
        ("pii-sweep", "PII sweep", "Flag personal data in a document before it leaves your boundary", "0.03", "EUR"),
        ("policy-conformance", "Policy conformance check", "Check a text against a supplied policy and cite the breaching lines", "0.04", "EUR"),
        ("retention-review", "Data retention review", "Assess a described data flow for retention and lawful-basis gaps", "0.06", "EUR"),
    ]),
    ("Orchard Data", "orchard-data", [
        ("invoice-extract", "Invoice extraction", "Pull line items, totals, tax and dates from an invoice into JSON", "0.03", "EUR"),
        ("table-from-pdf", "Table recovery from PDF", "Recover a table from a PDF as clean CSV, with a confidence score per column", "0.03", "EUR"),
        ("company-enrich", "Company enrichment", "From a domain, return legal name, registry id, country and trading status", "0.04", "USDC"),
        ("address-normalise", "Address normalisation", "Validate and normalise a postal address to the local standard", "0.01", "EUR"),
    ]),
    ("Meridian Language", "meridian-language", [
        ("translate-technical", "Technical translation", "Translate technical text with your glossary honoured verbatim", "0.03", "EUR"),
        ("ui-localise", "UI localisation", "Localise UI strings within a character budget per key", "0.04", "EUR"),
        ("tone-neutralise", "Tone neutralisation", "Rewrite a message to remove the heat while keeping every fact", "0.02", "EUR"),
        ("reading-level", "Reading-level rewrite", "Rewrite to a target reading level without dropping a single claim", "0.02", "EUR"),
    ]),
    ("Fathom Research", "fathom-research", [
        ("market-briefing", "Market briefing", "Briefing on one named market, every figure sourced and dated", "0.05", "USDC"),
        ("transcript-decisions", "Decisions from a transcript", "Turn a meeting transcript into decisions, owners and deadlines", "0.03", "EUR"),
        ("competitor-delta", "Competitor change report", "What changed on a competitor's public pages since a given date", "0.04", "USDC"),
        ("literature-scan", "Literature scan", "The ten most-cited recent papers on a topic, one line of findings each", "0.05", "EUR"),
    ]),
    ("Pike and Co", "pike-and-co", [
        ("regex-from-spec", "Regex from examples", "A regex that matches your examples and rejects your counter-examples, with tests", "0.02", "USDC"),
        ("sql-from-question", "SQL from a question", "A tested SQL query against the schema you supply", "0.03", "USDC"),
        ("spec-to-tests", "Spec to test cases", "Turn a written spec into executable test cases", "0.05", "USDC"),
        ("schema-validate", "Schema validation", "Validate a payload against a JSON Schema and explain every failure", "0.01", "EUR"),
    ]),
    ("Halden Studio", "halden-studio", [
        ("diagram-from-text", "Diagram from a description", "A clean architecture diagram from a written description", "0.04", "EUR"),
        ("social-pack", "Social pack from an article", "Five posts from one article, per-platform length, no hashtag padding", "0.03", "EUR"),
        ("alt-text", "Alt text for images", "Accurate alt text for a set of images, ready for a screen reader", "0.01", "EUR"),
    ]),
    ("Beacon SEO", "beacon-seo", [
        ("technical-seo-audit", "Technical SEO audit", "Crawl a site and rank what is actually costing you rankings, worst first", "0.06", "EUR"),
        ("keyword-gap", "Keyword gap analysis", "Queries your competitors rank for and you do not, with volume and difficulty", "0.05", "USDC"),
        ("schema-markup", "Structured data markup", "Valid JSON-LD for a page, tested against the rich-result rules", "0.03", "EUR"),
        ("core-web-vitals", "Core Web Vitals report", "Field and lab metrics for a URL, with the three changes that move them most", "0.04", "EUR"),
        ("internal-linking", "Internal linking plan", "Where to link from, to what, and why, for one target page", "0.03", "EUR"),
    ]),
    ("Ironwood Security", "ironwood-security", [
        ("dependency-audit", "Dependency vulnerability audit", "Known CVEs in a lockfile, ranked by whether the vulnerable path is reachable", "0.06", "USDC"),
        ("secrets-scan", "Secrets scan", "Find committed credentials in a repository, with the commit that introduced each", "0.05", "USDC"),
        ("attack-surface-review", "Attack surface review", "External exposure of a domain YOU OWN: open services, stale subdomains, leaked hosts", "0.08", "USDC"),
        ("security-headers", "Security header check", "Grade a site's headers and give the exact configuration that fixes each gap", "0.02", "EUR"),
        ("threat-model", "Threat model from a design", "Turn an architecture description into ranked threats and the control for each", "0.07", "USDC"),
    ]),
    ("Trawler Collect", "trawler-collect", [
        ("product-extract", "Product page extraction", "Title, price, availability and specs from a product URL, as clean JSON", "0.02", "EUR"),
        ("price-watch", "Price monitoring", "Track a set of URLs and report every price change with a timestamp", "0.03", "EUR"),
        ("listing-crawl", "Listing page crawl", "Paginate a listing and return every item as a structured feed", "0.05", "EUR"),
        ("sitemap-inventory", "Sitemap inventory", "Every indexable URL of a site, with status code and last-modified", "0.03", "USDC"),
        ("contact-discovery", "Public contact discovery", "Published contact routes for a company, from its own site and registries", "0.04", "USDC"),
    ]),
    # --- the moat tier -------------------------------------------------
    #
    # Everything below needs something a model cannot hold: licensed
    # data, running infrastructure, credentials, a presence in the
    # physical world, or a history somebody had to start recording months
    # ago. That is the argument for a market rather than a bigger model,
    # and it is why these are priced above the text work.
    ("Sable OSINT", "sable-osint", [
        ("breach-exposure", "Breach exposure check", "Which of these addresses appear in known breach corpora, and when", "0.35", "USDC"),
        ("domain-history", "Domain history", "Ownership, DNS and TLS history of a domain since it was first observed", "0.40", "USDC"),
        ("archive-diff", "Archived page diff", "What a page said on a given date, from archived captures, against today", "0.25", "USDC"),
        ("infra-fingerprint", "Infrastructure fingerprint", "Hosting, CDN, mail and analytics a company actually runs, from passive data", "0.30", "USDC"),
    ]),
    ("Cardinal Screening", "cardinal-screening", [
        ("director-check", "Directorship check", "Where a named person holds directorships, from official company registries", "0.60", "EUR"),
        ("court-record-check", "Court record search", "Publicly filed litigation involving an entity, by jurisdiction, with case numbers", "0.90", "EUR"),
        ("adverse-media", "Adverse media screen", "Negative coverage of an entity, deduplicated, dated and sourced", "0.45", "EUR"),
        ("beneficial-owner", "Beneficial ownership trace", "The ownership chain of a company up to the natural persons behind it", "1.20", "EUR"),
    ]),
    ("Anchor Registry", "anchor-registry", [
        ("company-filing-pull", "Statutory filing retrieval", "The latest statutory filing for a company, as filed, not as summarised", "0.50", "EUR"),
        ("vat-validate", "VAT number validation", "Check a VAT number live against the issuing authority, with the response kept", "0.05", "EUR"),
        ("trademark-clearance", "Trademark clearance", "Conflicting marks for a name in one class and territory, with citations", "0.95", "EUR"),
        ("insolvency-watch", "Insolvency watch", "Standing alert on insolvency proceedings for a list of entities", "0.30", "EUR"),
    ]),
    ("Steadfast Infra", "steadfast-infra", [
        ("managed-postgres", "Managed Postgres instance", "A provisioned database with backups on, connection string returned", "1.50", "USDC"),
        ("object-bucket", "Object storage bucket", "S3-compatible storage with a scoped key and a byte budget you set", "0.40", "USDC"),
        ("egress-proxy", "Regional egress", "Issue your requests from a chosen country, from a stable address", "0.20", "USDC"),
        ("gpu-burst", "GPU burst", "One hour of accelerator time for a job you hand over, artefact returned", "2.50", "USDC"),
    ]),
    ("Relay Networks", "relay-networks", [
        ("number-lease", "Phone number lease", "A real number in a chosen country, inbound messages relayed to you", "0.75", "USDC"),
        ("inbound-webhook", "Durable inbound endpoint", "A public URL that queues what it receives and replays it on demand", "0.15", "USDC"),
        ("domain-warmup", "Sending domain warm-up", "Reputation warm-up for a new sending domain, with a deliverability report", "1.10", "EUR"),
        ("dns-propagation", "DNS propagation check", "How a record resolves from resolvers on five continents, right now", "0.10", "EUR"),
    ]),
    ("Keystone Trust", "keystone-trust", [
        ("kyb-verify", "Business identity verification", "Verify a business to a stated assurance level, with the evidence retained", "1.80", "EUR"),
        ("document-timestamp", "Document timestamping", "Timestamp a document hash with a recognised authority, token returned", "0.20", "EUR"),
        ("certificate-issue", "TLS certificate issuance", "Issue and install a certificate for a domain you control", "0.30", "USDC"),
        ("identity-attest", "Reusable identity attestation", "A verified-identity attestation an agent can present again elsewhere", "0.85", "USDC"),
    ]),
    ("Halcyon Monitor", "halcyon-monitor", [
        ("uptime-history", "Verified uptime history", "Availability of an endpoint over months, measured rather than claimed", "0.35", "USDC"),
        ("cert-expiry-watch", "Certificate expiry watch", "Watch a fleet of domains and warn before any certificate lapses", "0.15", "USDC"),
        ("dependency-drift", "Dependency drift report", "What moved in your dependency tree since a release, and what it pulled in", "0.25", "USDC"),
        ("price-history", "Price history", "Price of a product over time, from observations started before you asked", "0.30", "EUR"),
    ]),
    ("Ferry Logistics", "ferry-logistics", [
        ("postal-dispatch", "Physical letter dispatch", "Print, frank and post a letter, with tracking and proof of posting", "1.40", "EUR"),
        ("device-farm-test", "Real device test run", "Run a flow on real handsets in a chosen market, with video of each run", "1.75", "USDC"),
        ("premises-check", "Business premises check", "Confirm a business operates at an address, with dated photographic evidence", "2.20", "EUR"),
        ("sample-purchase", "Sample purchase", "Buy one unit of a product and report what actually arrived", "2.00", "EUR"),
    ]),
    ("Quorum Experts", "quorum-experts", [
        ("native-review", "Native-speaker review", "An in-market native speaker checks register and idiom, not just meaning", "0.65", "EUR"),
        ("licensed-opinion", "Licensed professional review", "A licensed professional reviews a document in their own jurisdiction", "2.40", "EUR"),
        ("assistive-audit", "Assistive technology audit", "A screen-reader user walks your flow and reports where it breaks", "1.60", "USDC"),
        ("terminology-mapping", "Controlled vocabulary mapping", "Map free text to a controlled clinical or industrial vocabulary", "0.55", "EUR"),
    ]),
    ("Northwind Ops", "northwind-ops", [
        ("vendor-shortlist", "Vendor shortlist", "Shortlist providers for a requirement, scored, with the trade-offs stated", "0.45", "USDC"),
        ("runbook-author", "Runbook from a postmortem", "Turn an incident write-up into a runbook a tired person can follow at 3am", "0.35", "USDC"),
        ("alert-triage", "First-line alert triage", "Triage an alert, close it or escalate it, with the reasoning attached", "0.10", "USDC"),
        ("capacity-plan", "Capacity forecast", "What your load will need in ninety days, from your own metrics", "0.50", "USDC"),
    ]),
    ("Bridge Ops", "bridge-ops", [
        ("criteria-draft", "Acceptance-criteria drafting", "Turn a loose brief into machine-checkable acceptance criteria", "0.03", "USDC"),
        ("cost-estimate", "Task cost estimate", "Estimate what a task should cost before you commission it", "0.01", "USDC"),
        ("supplier-report", "Supplier reputation report", "Reputation report on a provider agent, built from its public job history", "0.02", "USDC"),
        ("change-watch", "Change watch", "Watch a URL and report the first material change", "0.02", "EUR"),
    ]),
]


def marked(text):
    """Append the demo marker, when one is configured."""
    return "{} ({})".format(text, LABEL) if LABEL else text


def cap_id(agent_slug, service):
    """`cap:demo:veritas-check:fact-check-claim`, or without the marker."""
    ns = "{}:".format(LABEL) if LABEL else ""
    return "cap:{}{}:{}".format(ns, agent_slug, service)


def call(method, path, body=None, token=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(NODE + path, data=data, method=method)
    req.add_header("content-type", "application/json")
    if token:
        req.add_header("authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return json.loads(r.read().decode() or "{}")
    except urllib.error.HTTPError as e:
        try:
            return {"error": json.loads(e.read().decode())}
        except Exception:
            return {"error": {"code": e.code}}
    except Exception as e:  # network, timeout
        return {"error": {"message": str(e)}}


class Envelope:
    """A target rate that drifts, dips and occasionally bursts.

    A constant rate is the one thing a real marketplace never has. Held
    flat, the feed reads as a metronome - which is worse than a quiet
    feed, because it says "generated" to anyone who watches it for a
    minute.

    The walk is mean-reverting rather than uniform-random: it wanders
    around the average, is pulled back when it strays, and every so often
    commits to a lull or a rush and stays there for a while. Independent
    coin flips per tick would produce noise, not weather.
    """

    def __init__(self, base, low, high):
        self.base = base
        self.low = low
        self.high = high
        self.cur = float(base)
        self.until = 0.0
        self.mode = "normal"
        self.target = float(base)
        self.last = time.monotonic()

    def rate(self, now=None):
        # The clock is injectable so the shape of the walk can be
        # simulated over an hour without waiting an hour for it.
        now = time.monotonic() if now is None else now
        dt = now - self.last
        if dt < 1.0:
            return self.cur
        self.last = now

        if now >= self.until:
            # Commit to a stretch: mostly ordinary, sometimes a lull,
            # occasionally a rush. Durations are long enough to be seen.
            roll = random.random()
            if roll < 0.12:
                self.mode = "lull"
                self.target = random.uniform(self.low, self.base * 0.5)
                self.until = now + random.uniform(45, 150)
            elif roll < 0.24:
                self.mode = "burst"
                self.target = random.uniform(self.base * 1.8, self.high)
                self.until = now + random.uniform(20, 70)
            else:
                self.mode = "normal"
                # Centred slightly BELOW the average on purpose: bursts
                # are far from the mean and lulls are not, so a normal
                # band centred on the average lands the hour above it.
                self.target = random.uniform(self.base * 0.55, self.base * 1.2)
                self.until = now + random.uniform(60, 180)

        # Ease towards the target instead of stepping onto it, so the
        # transitions are visible as a ramp rather than a cliff.
        self.cur += (self.target - self.cur) * min(dt / 8.0, 1.0)
        self.cur += random.gauss(0, self.cur * 0.04)
        self.cur = max(self.low, min(self.high, self.cur))
        return self.cur


class Pacer:
    """Space actions to hit a moving target, without bursting inside it.

    A dashboard fed by an even trickle looks alive; one fed by clumps
    looks scripted. The clumping that IS wanted comes from the envelope
    above, which is a property of the market rather than of the client.
    """

    # Measured on a scratch node: 31 actions produced 41 spine events,
    # because accept-delivery alone writes pay.released, exe.accept and
    # a reputation update. The target is expressed in EVENTS because
    # that is what the dashboard counts, so the action rate is derived.
    EVENTS_PER_ACTION = 1.32

    def __init__(self, envelope):
        self.envelope = envelope
        self.next_at = time.monotonic()
        self.lock = threading.Lock()

    def wait(self):
        with self.lock:
            now = time.monotonic()
            actions_per_min = max(self.envelope.rate() / self.EVENTS_PER_ACTION, 0.1)
            gap = 60.0 / actions_per_min
            if self.next_at < now:
                self.next_at = now
            delay = self.next_at - now
            self.next_at += gap * random.uniform(0.75, 1.25)
        if delay > 0:
            time.sleep(delay)


def load_agents():
    """Registered once, reused for ever, and topped up in place.

    Agents cannot be un-announced from a public directory in any way
    that removes what they already did, so re-registering the whole pool
    on every start would leave a permanent trail of duplicate firms. Only
    the ones missing from the saved state are created.
    """
    saved = []
    if os.path.exists(STATE):
        try:
            with open(STATE) as f:
                saved = json.load(f)
        except Exception as e:
            print("[demo] state unreadable ({}), starting a fresh pool".format(e), flush=True)
            saved = []
    have = {a.get("firm") or a.get("name") for a in saved}

    agents = list(saved)
    for firm, slug, services in CATALOGUE[:POOL]:
        if firm in have:
            continue
        ident = call("POST", "/v1/identity", {"name": marked(firm)})
        if "token" not in ident:
            print("[demo] identity refused for {}: {}".format(firm, ident), flush=True)
            continue
        caps = [{
            "id": cap_id(slug, service),
            # Required by the node: Capability::name has no serde
            # default, so omitting it empties the whole list and the
            # announce is refused with "capabilities required".
            "name": title,
            "description": desc,
            "price": {"amount": amount, "currency": currency, "model": "fixed"},
            "autonomy": ["execute-notify"],
        } for service, title, desc, amount, currency in services]
        ann = call("POST", "/v1/announce", {
            "name": marked(firm),
            "description": "{}. Generated traffic on this node.".format(
                services[0][2]) if LABEL else firm,
            "capabilities": caps,
        }, ident["token"])
        if "error" in ann:
            print("[demo] announce refused for {}: {}".format(firm, ann["error"]), flush=True)
            continue
        agents.append({
            "did": ident["did"], "token": ident["token"],
            "firm": firm, "name": firm, "capabilities": caps,
        })
        print("[demo] registered {}".format(firm), flush=True)

    if agents != saved:
        os.makedirs(os.path.dirname(STATE), exist_ok=True)
        with open(STATE, "w") as f:
            json.dump(agents, f)
    print("[demo] pool of {} firms, {} capabilities".format(
        len(agents), sum(len(a["capabilities"]) for a in agents)), flush=True)
    return agents[:POOL]


def run_deal(agents, pacer, stats):
    """One deal, start to finish, on one of several paths."""
    client, provider = random.sample(agents, 2)
    cap = random.choice(provider["capabilities"])
    amount = cap["price"]["amount"]
    deadline = int(time.time()) + random.randint(1800, 7200)

    pacer.wait()
    out = call("POST", "/v1/contract/propose", {
        "provider": provider["did"],
        "capability_id": cap["id"],
        "terms": {
            "input": {"brief": cap["description"]},
            "deliverable": {"type": "text"},
            "acceptance_criteria": [],
            "deadline": deadline,
            "price": dict(cap["price"]),
            "autonomy": "execute-notify",
        },
        "escrow": True,
    }, client["token"])
    cid = out.get("contract_id")
    if not cid:
        stats["failed"] += 1
        return
    stats["propose"] += 1

    roll = random.random()

    # The provider declines outright. A market where nobody ever says no
    # is not a market.
    if roll < 0.06:
        pacer.wait()
        call("POST", "/v1/contract/{}/reject".format(cid),
             {"reason": "capacity"}, provider["token"])
        stats["rejected"] += 1
        return

    pacer.wait()
    call("POST", "/v1/contract/{}/accept".format(cid), {}, provider["token"])
    stats["accepted"] += 1

    # Signed, then the buyer thinks better of it before funding.
    if roll < 0.11:
        pacer.wait()
        call("POST", "/v1/contract/{}/cancel".format(cid),
             {"reason": "no longer needed"}, client["token"])
        stats["cancelled"] += 1
        return

    pacer.wait()
    parked = call("POST", "/v1/escrow/park",
                  {"contract_id": cid, "amount": amount}, client["token"])
    if "error" in parked:
        stats["failed"] += 1
        return
    stats["parked"] += 1

    pacer.wait()
    call("POST", "/v1/contract/{}/start".format(cid),
         {"plan": "retrieve, draft, check"}, provider["token"])
    stats["started"] += 1

    if random.random() < 0.5:
        pacer.wait()
        call("POST", "/v1/contract/{}/progress".format(cid),
             {"step": 1, "note": "draft ready, checking sources"}, provider["token"])
        stats["progress"] += 1

    content = "Result for {} at {}.".format(cap["id"], int(time.time()))
    digest = hashlib.sha256(content.encode()).hexdigest()
    pacer.wait()
    delivered = call("POST", "/v1/contract/{}/deliver".format(cid), {
        "deliverable_hash": "sha256:" + digest,
        "artifact": {"media_type": "text/plain", "encoding": "utf8", "content": content},
    }, provider["token"])
    if "error" in delivered:
        stats["failed"] += 1
        return
    stats["delivered"] += 1

    # Some buyers go quiet. The node's own expiry sweep resolves those
    # after a day by accepting on their behalf and paying the provider,
    # which is worth showing rather than hiding.
    if random.random() < 0.07:
        stats["left_hanging"] += 1
        return

    pacer.wait()
    settled = call("POST", "/v1/contract/{}/accept-delivery".format(cid), {}, client["token"])
    if "error" in settled:
        stats["failed"] += 1
        return
    stats["settled"] += 1


def main():
    print("[demo] node={} rate={}/min (lull {} .. burst {}) pool={} label={!r}".format(
        NODE, TARGET_PER_MIN, LOW_PER_MIN, HIGH_PER_MIN, POOL, LABEL), flush=True)
    health = call("GET", "/health")
    if health.get("status") != "ok":
        print("[demo] node not healthy: {}".format(health), flush=True)
        sys.exit(1)

    agents = load_agents()
    if len(agents) < 2:
        print("[demo] need at least two agents, got {}".format(len(agents)), flush=True)
        sys.exit(1)

    envelope = Envelope(TARGET_PER_MIN, LOW_PER_MIN, HIGH_PER_MIN)
    pacer = Pacer(envelope)
    stats = {k: 0 for k in (
        "propose", "accepted", "rejected", "cancelled", "parked", "started",
        "progress", "delivered", "settled", "left_hanging", "failed")}
    work = queue.Queue()

    def worker():
        while True:
            work.get()
            try:
                run_deal(agents, pacer, stats)
            except Exception as e:
                stats["failed"] += 1
                print("[demo] deal failed: {}".format(e), flush=True)
            finally:
                work.task_done()

    # Several deals in flight at once, so the feed interleaves phases
    # instead of marching one contract through at a time.
    for _ in range(8):
        threading.Thread(target=worker, daemon=True).start()

    started = time.monotonic()
    last_report = started
    while True:
        if work.qsize() < 8:
            work.put(1)
        time.sleep(0.2)
        now = time.monotonic()
        if now - last_report >= 60:
            mins = (now - started) / 60.0
            total = sum(v for k, v in stats.items() if k != "failed")
            print("[demo] {:.0f} min | {} actions ({:.1f}/min) | {} {:.0f}/min | {}".format(
                mins, total, total / max(mins, 0.01), envelope.mode, envelope.cur,
                " ".join("{}={}".format(k, v) for k, v in stats.items() if v)), flush=True)
            last_report = now


if __name__ == "__main__":
    main()
