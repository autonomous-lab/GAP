# GAP Cloud — Instructions for Agents

GAP Cloud is an API-first backend for agent-built applications: projects, KV,
objects, SQLite, functions, static sites, custom domains and realtime.
Contract commerce, escrow, discovery and arbitration are archived and are not
available on the Cloud server.

## Start here

Create an identity with `POST /v1/identity`. The response includes an API
`token`; keep it server-side and use `Authorization: Bearer <token>`.
Create a project with `POST /v1/cloud/projects`, then use its returned
`project_id` in the examples below. Existing identities and project tokens
remain valid. Never expose the owner bearer in browser code.

## Runtime services — use GAP as your backend

If you need state or execution but do not want to operate infrastructure, create
an owner-scoped cloud project with `POST /v1/cloud/projects`. The node provides:

- KV: 64 KiB per value, 25 MiB per project;
- objects: 1 MiB per object, 100 MiB per project;
- private static hosting: Basic Auth mandatory, 3 MiB per file, 100 MiB total,
  5,000 files, 5 retained versions, 20 requests/second and 1 GiB per rolling
  30-day period;
- SQLite: parameterized queries, one 100 MiB database per project;
- JavaScript functions: 1 MiB per version, 100 MiB total, executed in the
  separately constrained sandbox container;
- realtime free tier: 25 simultaneous connections, 25 active channels, messages
  up to 64 KiB, 30 messages/minute/connection, 300/minute/project, 24-hour
  retention and 25 MiB of persisted messages. An operator-funded credit balance
  can pay for controlled overages up to the hard safety limits documented below.

All management routes require your normal agent bearer, and knowing a project
identifier grants no access. Do not attempt `ATTACH`, `PRAGMA`, arbitrary network
access or filesystem access: the runtime refuses them by design.

Functions have a 30-second execution timeout.
The 30-second budget covers the entire invocation after admission, including
worker replays and waiting for storage/HTTP capabilities; it does not reset
after each call. The queue wait described below is separate. Each invocation
may dispatch up to **128 capability calls total**, including at most **32 HTTP
calls**. SQL, KV, objects and realtime-token issuance share the total budget;
failed calls also count, while replaying an already completed call does not.
Limit checks happen before dispatching the excess operation. Prefer SQL batches
and retry at the application level only when safe: earlier successful writes
are not rolled back if a later call hits a limit or times out.

The sandbox is allocated 1 CPU,
512 MiB and 256 PIDs, with at most 4 simultaneous invocations per project and
16 globally. A bounded queue holds 32 requests for at most 30 seconds. When it
cannot accept an invocation, GAP returns HTTP `429` with
`{"error":{"code":"sandbox_busy","message":"sandbox is busy"}}`; retry with
exponential backoff and jitter rather than treating this as a malformed request.

Set these once for every example below:

```bash
export NODE=https://gap.geta.team
export TOKEN=gat_your_agent_bearer
```

### Projects — create and list

```bash
# Create. Keep the returned project_id; it is used by every other route.
curl -sX POST "$NODE/v1/cloud/projects" \
  -H "Authorization: Bearer $TOKEN"
# -> {"project_id":"prj_...","owner_did":"did:gap:...","status":"active",...}

export PROJECT=prj_returned_above

# List only the projects owned by this bearer.
curl -s "$NODE/v1/cloud/projects" \
  -H "Authorization: Bearer $TOKEN"
# -> {"projects":[...]}
```

### KV — put and get

Values use standard base64. `expires_at` is an optional Unix timestamp.

```bash
curl -sX PUT "$NODE/v1/cloud/projects/$PROJECT/kv/session-42" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"value_base64":"eyJzdGF0dXMiOiJhY3RpdmUifQ==","expires_at":1893456000}'
# -> {"stored":true}

curl -s "$NODE/v1/cloud/projects/$PROJECT/kv/session-42" \
  -H "Authorization: Bearer $TOKEN"
# -> {"found":true,"value_base64":"eyJzdGF0dXMiOiJhY3RpdmUifQ=="}
# A missing or expired key returns {"found":false}.
```

### Objects — put and get

```bash
curl -sX PUT "$NODE/v1/cloud/projects/$PROJECT/objects/report.json" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"content_base64":"eyJvayI6dHJ1ZX0=","media_type":"application/json"}'
# -> {"stored":true,"digest":"sha256:..."}

curl -s "$NODE/v1/cloud/projects/$PROJECT/objects/report.json" \
  -H "Authorization: Bearer $TOKEN"
# -> {"found":true,"content_base64":"eyJvayI6dHJ1ZX0=",
#     "media_type":"application/json","digest":"sha256:..."}
```

### Private static site — configure, deploy and activate

Static hosting is intentionally private-only. There is no public mode: every
request below `/sites/{project}/` requires the configured HTTP Basic
credential. The owner bearer manages releases but is never used by visitors.

Configure the site first. Passwords must contain 12–128 bytes; GAP stores an
Argon2id hash and never returns the password or hash. On later updates, omit
`password` to keep the existing credential.

```bash
curl -sX PUT "$NODE/v1/cloud/projects/$PROJECT/site" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"enabled":true,"entrypoint":"index.html","spa_fallback":true,
       "auth":{"mode":"basic","username":"visitor",
               "password":"replace-with-at-least-12-bytes"}}'

# Read configuration, retained versions, active version, URL and exact quotas.
curl -s "$NODE/v1/cloud/projects/$PROJECT/site" \
  -H "Authorization: Bearer $TOKEN"
```

Create a draft version, upload each file as standard base64, then activate the
completed version. MIME types come from a server-side extension allowlist; an
upload cannot choose its own `Content-Type`.

```bash
VERSION=$(curl -sX POST \
  "$NODE/v1/cloud/projects/$PROJECT/site/versions" \
  -H "Authorization: Bearer $TOKEN" | jq -r .version)

curl -sX PUT \
  "$NODE/v1/cloud/projects/$PROJECT/site/versions/$VERSION/files/index.html" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"content_base64":"PCFkb2N0eXBlIGh0bWw+PGh0bWw+PGJvZHk+PGgxPkhlbGxvPC9oMT48L2JvZHk+PC9odG1sPg=="}'

# Inspect the draft manifest or remove one draft file.
curl -s "$NODE/v1/cloud/projects/$PROJECT/site/versions/$VERSION/files" \
  -H "Authorization: Bearer $TOKEN"
curl -sX DELETE \
  "$NODE/v1/cloud/projects/$PROJECT/site/versions/$VERSION/files/obsolete.css" \
  -H "Authorization: Bearer $TOKEN"

curl -sX POST \
  "$NODE/v1/cloud/projects/$PROJECT/site/versions/$VERSION/activate" \
  -H "Authorization: Bearer $TOKEN"

# The browser receives 401 + WWW-Authenticate until credentials are supplied.
curl -u 'visitor:replace-with-at-least-12-bytes' \
  "$NODE/sites/$PROJECT/"
```

An activated version is immutable and activation fails unless its entrypoint
exists. Create the next version for an update; activation switches every path
atomically. Delete only inactive versions:

```bash
curl -sX DELETE \
  "$NODE/v1/cloud/projects/$PROJECT/site/versions/1" \
  -H "Authorization: Bearer $TOKEN"
```

Allowed assets are HTML, CSS, JavaScript modules, JSON, text, XML, SVG, common
web images and web fonts. GAP rejects hidden/path-traversal names, executables,
oversized files, invalid UTF-8 in text assets and forbidden control bytes.
CSS, JS and MJS uploads have no content judgement: minified bundles and encoded
content are accepted. Static uploads do not use the AI function judge. Other
text assets retain the heuristic scan for excessive padding/obfuscation,
embedded credentials, `<base>` overrides and meta refreshes. Acceptance is not
a security certification: never ship real secrets in browser assets. Server
functions retain their separate static scan and AI security review.
Every HTML response on the GAP-owned `/sites/{project}/` URL receives a
non-removable "Hosted by GAP - private agent project" banner. Those responses
use `private, no-store`, `nosniff`,
`noindex, nofollow, noarchive`, no referrer, same-origin resource policy and a
media-compatible CSP. Browser fetch/XHR connections may use any HTTPS origin,
and WebSockets may use any WSS origin. Video/audio may use HTTPS or `blob:` URLs;
embedded frames may use HTTPS, and workers may use same-origin or `blob:` URLs.
This supports external players and HLS/MSE playback. Bundle player libraries
(such as hls.js) as uploaded JavaScript files: external script CDNs, inline
JavaScript and `eval` remain blocked. Plugins, embedding the GAP page inside
another page, and cross-origin form submission also remain blocked.
Images may use HTTPS, `data:` or `blob:` URLs; insecure HTTP resources remain
blocked. `Referrer-Policy: no-referrer` prevents the private
site URL and credentials from being sent as an image request referrer. Put
configuration and application code in uploaded `.js` files rather than inline
`<script>` elements.

These permissions apply to browsers only: function sandbox egress restrictions
are unchanged. External servers must still allow CORS for fetch-based players;
their framing policies and the browser's codecs/DRM also still apply. CSP does
not guarantee playback or remove advertising inside external players.
Sites under `/sites/` share the `gap.geta.team` origin and browser storage;
neither Basic Auth nor this CSP isolates localStorage between project paths.
Never put owner bearers there. Use a dedicated custom origin per project for
browser-storage isolation.

Free projects receive 3 MiB (3,145,728 bytes) per file, 100 MiB across retained versions, 5,000
files, 5 versions, 20 requests/second and 1 GiB per rolling 30-day period.
Delete an inactive release to reclaim both its storage and version slot.

The upload API uses base64: a full-size file encodes to 4 MiB, plus JSON
overhead, within the default 5 MiB HTTP request limit. Object storage and
function-version limits remain 1 MiB; this increase is for static site files.

### Custom site domains

A free project may attach up to three domains. A verified custom domain can be
public, while the GAP-owned `/sites/{project}/` address always keeps Basic Auth.
Use an ASCII hostname; encode internationalized names as Punycode.

```bash
# Register the hostname and choose public or basic access.
DOMAIN=$(curl -sX POST \
  "$NODE/v1/cloud/projects/$PROJECT/site/domains" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"hostname":"movies.example.com","access":"public"}')

echo "$DOMAIN" | jq .dns
# Add the returned TXT record verbatim. Then point the hostname at the
# returned target with A/AAAA, or with a CNAME when one is supplied. Cloudflare
# orange-cloud proxying is supported, including Flexible SSL; DNS-only gives
# Caddy end-to-end TLS directly, while Full (strict) is preferred when proxied.

curl -sX POST \
  "$NODE/v1/cloud/projects/$PROJECT/site/domains/movies.example.com/verify" \
  -H "Authorization: Bearer $TOKEN"

# List status, access mode and verification details.
curl -s "$NODE/v1/cloud/projects/$PROJECT/site/domains" \
  -H "Authorization: Bearer $TOKEN"

# Detach immediately. Caddy will refuse future certificate issuance and GAP
# stops routing the hostname even if an old certificate remains cached.
curl -sX DELETE \
  "$NODE/v1/cloud/projects/$PROJECT/site/domains/movies.example.com" \
  -H "Authorization: Bearer $TOKEN"
```

The TXT record is a project-specific ownership proof. DNS pointing alone is
not enough: otherwise one agent could claim somebody else's hostname that was
already aimed at GAP. Verification activates the exact hostname only; wildcard
domains and IP literals are rejected. Caddy's internal `ask` endpoint also
requires a shared secret and returns success only for an active mapping.

Cloudflare-proxied domains are accepted without an HTTPS redirect loop even in
Flexible mode. GAP's Caddy edge honours Cloudflare's HTTPS `CF-Visitor` signal
exclusively from Cloudflare's published IP ranges, so a direct caller cannot
spoof that exception. Full (strict) is still recommended because it also
encrypts the Cloudflare-to-origin connection.

Custom-domain pages are served from `/`, preserve SPA fallback, omit the
GAP private-project banner, retain the same upload scan/rate/bandwidth controls,
and use the same media-compatible CSP described above, including HTTPS/WSS
connections to GAP or external services and HTTPS embedded players.
Public domains may be indexed and cache for at most 60 seconds; `basic` domains
keep `noindex` and `private, no-store`.

The verified hostname also exposes project-bound, same-origin aliases:

```text
ANY https://movies.example.com/_gap/functions/{function}/{path...}
WS  wss://movies.example.com/_gap/realtime
```

The function alias is equivalent to
`https://gap.geta.team/functions/{project}/{function}/{path...}`, including its
`public`, scoped-token or owner authentication policy, method, query and body.
The project id is taken exclusively from the verified hostname and must not be
included in the alias URL. Management endpoints remain on `gap.geta.team` and
still require the owner bearer.

```js
const categories = await fetch('/_gap/functions/movix/categories').then(r => r.json());
const wsScheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
const socket = new WebSocket(`${wsScheme}//${location.host}/_gap/realtime`);
```

The WebSocket wire protocol and token format are unchanged. During
authentication GAP verifies that the token's signed `project_id` matches the
project attached to the hostname. Removing the domain, disabling its site or
suspending its project disables both aliases immediately. `/_gap/` is reserved
by the platform and cannot be shadowed by the site's SPA fallback.

### SQLite — execute and query

Use `execute` for schema changes and mutations, `query` for rows. Always bind
untrusted input through `params`; never concatenate it into SQL. A binary
parameter is encoded as `{"blob_base64":"..."}`.

```bash
curl -sX POST "$NODE/v1/cloud/projects/$PROJECT/database/execute" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"sql":"CREATE TABLE messages(id INTEGER PRIMARY KEY, body TEXT NOT NULL)","params":[]}'
# -> {"affected_rows":0,...}

curl -sX POST "$NODE/v1/cloud/projects/$PROJECT/database/execute" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"sql":"INSERT INTO messages(body) VALUES (?)","params":["hello"]}'
# -> {"affected_rows":1,...}

curl -sX POST "$NODE/v1/cloud/projects/$PROJECT/database/query" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"sql":"SELECT id, body FROM messages WHERE id > ? ORDER BY id","params":[0]}'
# -> {"columns":["id","body"],"rows":[[1,"hello"]],"truncated":false,...}
```

Only one statement is accepted per call. GAP refuses client-managed
transactions, `ATTACH`, `DETACH`, `PRAGMA`, temporary schemas and virtual
tables; retrying those statements will not make them valid.

Each SQL call accepts at most **1,000 bound parameters**, through both the
management API and function bindings `gap.db.query` / `gap.db.execute`.
The combined parameter payload is capped at **4 MiB**: UTF-8 bytes for text,
decoded bytes for blobs, 8 bytes for numbers/booleans and 0 for null. The
existing 1 MiB limit per text/blob parameter still applies. JSON/base64
transport overhead and sandbox message limits may impose a lower practical
batch size. Oversized batches are rejected before SQL execution; errors report
the received parameter count or cumulative byte size and the allowed maximum.

For bulk inserts, use bound placeholders and batches of at most
`Math.floor(1000 / parametersPerRow)` rows (subtract any statement-level
parameters first), also respecting the byte budget. For example, 10 values per
row permit up to 100 rows per call. Do not interpolate values into SQL to evade
the limit. SQL text remains limited to 32 KiB; query results to 100 rows, 50
columns and 4 MiB. The 250 ms SQL execution budget and 100 MiB project database
quota are unchanged. A function still has a bounded capability-call budget, so
do not split an import into arbitrarily many tiny calls in one invocation.

### Functions — deploy, activate, invoke and delete

The deployed `source` is a JavaScript function expression. It receives the
JSON request as its first argument and returns a JSON-serializable result.

```bash
curl -sX POST "$NODE/v1/cloud/projects/$PROJECT/functions/greet" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"runtime":"javascript","source":"async (request, gap) => ({ message: `Hello ${request.name}` })"}'
# -> {"name":"greet","version":1,"runtime":"javascript","digest":"sha256:...",
#     "ruling":"approved_with_constraints","security_review":{"judge":"...",
#     "static_findings":[],"reasons":["..."]},...}

curl -sX POST "$NODE/v1/cloud/projects/$PROJECT/functions/greet/activate" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"version":1}'
# -> {"active":true,"version":1}

curl -sX POST "$NODE/v1/cloud/projects/$PROJECT/functions/greet/invoke" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"request":{"name":"Ada"}}'
# -> {"result":{"message":"Hello Ada"},"version":1,"digest":"sha256:..."}

# Deploying again creates version 2 but leaves version 1 active.
curl -sX POST "$NODE/v1/cloud/projects/$PROJECT/functions/greet" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"runtime":"javascript","source":"async (request) => ({ message: `Hi ${request.name}` })"}'
# -> {"name":"greet","version":2,"active":false,...}

# Delete that inactive version. Deleting active version 1 would be refused.
curl -sX DELETE \
  "$NODE/v1/cloud/projects/$PROJECT/functions/greet/versions/2" \
  -H "Authorization: Bearer $TOKEN"
# -> {"deleted":true,"name":"greet","version":2}

# Delete the function and every version, including the active one.
curl -sX DELETE "$NODE/v1/cloud/projects/$PROJECT/functions/greet" \
  -H "Authorization: Bearer $TOKEN"
# -> {"deleted":true,"name":"greet"}
# Repeating the same DELETE is safe and returns {"deleted":false,...}.
```

Deploying creates a new immutable version; it does not switch production.
Before storage, a deterministic security gate rejects environment access,
unbrokered networking, process/module loading, dynamic code, prototype attacks,
excessive obfuscation or padding, and looped/fan-out `gap.http` calls. The source
is then assessed by the configured security judges for DDoS, abusive scraping,
secret extraction, exfiltration, open-proxy behaviour, sandbox escape and
vulnerability exploitation. A positive verdict from the first available judge
approves immediately. A negative verdict requires independent confirmation;
disagreement, uncertainty, or an unavailable confirmation fails closed to
`needs_review`. `rejected` and `needs_review` versions cannot be activated.

Security judges run outside the node's shared state lock, so publication does
not freeze sites or API reads. One function publication runs at a time per
node; concurrent attempts return HTTP `429` with error code `publication_busy`.
Retry with exponential backoff and jitter. Ownership, project status and
storage quotas are checked again before the reviewed version is saved.

Activate the exact reviewed version explicitly. The sandbox exposes no process
environment, filesystem handle, database path, project bearer or arbitrary
network access.
Deleting source releases its function-storage quota immediately. Prefer the
version endpoint for cleanup; use the function endpoint when the deployed name
itself is no longer needed.

### Function bindings, HTTP egress and browser routes

Functions may call project storage without receiving its owner token:

```javascript
async (request, gap) => {
  await gap.kv.put("last-search", request.query);
  const cached = await gap.kv.get("last-search");
  await gap.db.execute("CREATE TABLE IF NOT EXISTS hits(q TEXT)");
  await gap.db.execute("INSERT INTO hits(q) VALUES(?)", [cached]);
  const rows = await gap.db.query("SELECT q FROM hits ORDER BY rowid DESC LIMIT 10");
  await gap.objects.put("hits.json", JSON.stringify(rows), "application/json");
  const object = await gap.objects.get("hits.json");
  return { rows, object };
}
```

Outbound HTTP is brokered by GAP, limited to HTTPS `GET`/`POST`, a 30-second
timeout, 3 MiB responses and the headers `Accept`, `Content-Type`, `Cookie` and
`User-Agent`. Configure exact hosts first; redirects, private/link-local
addresses and unlisted hosts are refused:

There is no separate capability-grant endpoint: the project's `/egress`
allowlist is the grant for `gap.http`. An `approved_with_constraints` release
ruling means the function must remain inside these runtime constraints; it does
not disable `http.request`.

```bash
curl -sX PUT "$NODE/v1/cloud/projects/$PROJECT/egress" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"hosts":["witozo.com"]}'
curl -s "$NODE/v1/cloud/projects/$PROJECT/egress" -H "Authorization: Bearer $TOKEN"
```

```javascript
async (request, gap) => gap.http.get("https://witozo.com/films", {
  headers: { "User-Agent": "Mozilla/5.0", "Cookie": "g=true" }
})
```

Expose a function as a browser endpoint. `public` needs no credential;
`token` accepts a scoped token valid for 60 minutes; `private` accepts only the
owner bearer. Never embed the owner bearer in a site:

```bash
curl -sX PUT "$NODE/v1/cloud/projects/$PROJECT/functions/greet/http" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"auth":"token","cors_origins":["*"]}'

INVOKE_TOKEN=$(curl -sX POST \
  "$NODE/v1/cloud/projects/$PROJECT/functions/greet/tokens" \
  -H "Authorization: Bearer $TOKEN" | jq -r .token)

curl -s "$NODE/functions/$PROJECT/greet/categories?q=recent" \
  -H "Authorization: Bearer $INVOKE_TOKEN"
```

The handler receives `{method,path,query,body}`. GAP answers CORS preflights;
the public route is `/functions/{project}/{function}/{path...}`.

### Scheduled functions

The initial cron subset supports minute intervals `*/N * * * *`, from 1 to
1440 minutes. Create/update by id, list, and delete:

```bash
curl -sX PUT "$NODE/v1/cloud/projects/$PROJECT/schedules/refresh-cache" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"function":"refresh","cron":"*/15 * * * *","request":{"source":"cron"}}'
curl -s "$NODE/v1/cloud/projects/$PROJECT/schedules" -H "Authorization: Bearer $TOKEN"
curl -sX DELETE "$NODE/v1/cloud/projects/$PROJECT/schedules/refresh-cache" \
  -H "Authorization: Bearer $TOKEN"
```

### Realtime token — issue from a trusted backend

```bash
curl -sX POST "$NODE/v1/cloud/projects/$PROJECT/realtime/tokens" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"channels":["room:customer-42"],
       "permissions":["subscribe","publish"],
       "subject":"visitor:8f31"}'
# -> {"token":"base64url-claims.hmac-signature","expires_at":...}
```

The returned token lasts 60 minutes. `permissions` may contain `subscribe`,
`publish`, or both. Omitting it grants both for backward compatibility. An
empty `channels` array grants every channel in the project; do not issue that
scope to public clients.

A GAP function is itself a trusted token backend without ever receiving the
owner bearer or realtime signing secret. Prefer this native capability over
storing a bearer in KV or attempting to pass `Authorization` through
`gap.http` (that header remains forbidden):

```javascript
async (request, gap) => {
  // Authenticate/authorize request.user in your application logic first.
  return await gap.realtime.issueToken({
    channels: [`room:${request.room}`],
    permissions: ["subscribe", "publish"],
    subject: `visitor:${request.user}`,
    expires_in: 600
  });
}
```

`channels` is mandatory and must contain 1–25 explicit scopes for tokens
issued by functions. `permissions` defaults to both permissions, `subject` is
optional, and `expires_in` must be between 60 and 3600 seconds. GAP injects the
project scope, signs internally and audits the issuance.

### WebSocket — every client action

Open `wss://gap.geta.team/v1/realtime`, or `wss://your-domain/_gap/realtime` on
a verified custom domain. The wire protocol is JSON. Authenticate
within five seconds, then subscribe before publishing to a channel.

```json
{"action":"authenticate","token":"TOKEN_RETURNED_ABOVE"}
```

```json
{"type":"authenticated","project_id":"prj_...","subject":"visitor:8f31",
 "permissions":["subscribe","publish"],"expires_at":1893456000}
```

Subscribe and optionally replay up to 100 persisted messages after a known
sequence cursor:

```json
{"action":"subscribe","channel":"room:customer-42","after":1042}
{"type":"subscribed","channel":"room:customer-42"}
```

Publish an ephemeral message, or set `persist` to retain it for at most 24
hours:

```json
{"action":"publish","channel":"room:customer-42",
 "payload":{"kind":"status","value":"ready"},"persist":true}
```

Subscribers receive:

```json
{"type":"message","channel":"room:customer-42","seq":1043,
 "payload":{"kind":"status","value":"ready"},"created_at":1893452400,
 "replay":false}
```

Replayed messages carry `"replay":true`. Ephemeral messages have `"seq":null`.
Unsubscribe without closing the socket:

```json
{"action":"unsubscribe","channel":"room:customer-42"}
{"type":"unsubscribed","channel":"room:customer-42"}
```

Protocol or quota failures arrive as `{"type":"error","error":"..."}`. A
client must stop or back off on errors such as `hard message rate exceeded`, renew
after `token expired`, and never reconnect in a tight loop.

### Realtime credits — controlled overage

The free limits remain available with a zero balance. Beyond them, GAP debits:

- 1 credit per extra connection when opened, then per started connection-hour;
- 1 credit when a channel beyond the first 25 becomes active;
- 1 credit per client action beyond either free per-minute rate;
- 1 credit per additional started 64 KiB payload chunk;
- 1 credit per additional started MiB of retained messages beyond 25 MiB.

One action that crosses several boundaries is charged atomically: it either
receives all required credits or none. Credits never bypass the hard safety
limits: 100 connections, 100 channels, 256 KiB per payload, 300 actions/minute
per connection, 3,000/minute per project and 100 MiB persisted. Retention stays
at 24 hours. Exhaustion returns a protocol error; a paid connection that cannot
renew its hourly credit is closed with code `4402`.

The owner can inspect its balance, aggregate spend by reason and the latest 100
top-ups:

```bash
curl -s "$NODE/v1/cloud/projects/$PROJECT/realtime/credits" \
  -H "Authorization: Bearer $TOKEN"
```

Only the GAP operator can top up. `idempotency_key` is mandatory, scoped to the
project and safe to replay with exactly the same amount and note:

```bash
curl -sX POST "$NODE/v1/admin/cloud/projects/$PROJECT/realtime/credits" \
  -H "Authorization: Bearer $GAP_ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"amount":10000,"idempotency_key":"manual-2026-001","note":"manual grant"}'
```

Top-ups are persisted in ClickHouse and written to the GAP audit spine. The
realtime sidecar alone may debit the account through its internal authenticated
route; project owners cannot forge or refund consumption.

### Realtime for a static site

Your browser connects to `wss://gap.geta.team/v1/realtime`, but it must never
receive the permanent project bearer. Put
[`sdk/realtime-token-handler.js`](./sdk/realtime-token-handler.js) in a
server-side or edge function; authenticate the visitor there and return only a
60-minute token with explicit channels, permissions and a `subject`:

```json
{
  "channels": ["room:customer-42"],
  "permissions": ["subscribe", "publish"],
  "subject": "visitor:8f31"
}
```

Use `subscribe` alone for read-only visitors. Prefer a narrow channel per room,
contract or tenant; an empty channel list means every channel in the project and
is unsuitable for public clients. Browser integration is the dependency-free
[`sdk/realtime.js`](./sdk/realtime.js), which renews through your token provider,
reconnects and restores subscriptions.

## Compose — experimental

**Opt-in on public or private nodes; operator approval required on gap.geta.team.**
Compose always requires an operator-preapproved agent and an exclusive,
GAP-managed microVM bound to your project (or a legacy operator-provisioned guest).
You cannot self-approve or choose the worker SSH target. On a public node, create an identity normally
and ask the operator to approve its exact DID for Compose. Without that approval,
ordinary Cloud services still work but every Compose operation is denied.
On a private node, operator-only identity creation and general node approval
are required in addition to the separate Compose approval.
See [operator setup](./runtime/compose/README.md). Use the node where your
operator has configured Compose; do not assume the public deployment enables it.

One Compose stack is supported per project. Docker Engine/Compose run inside
your guest: builds, `.env`, includes, guest bind mounts, guest Docker socket,
`privileged` and guest `network_mode: host` are allowed. They do not refer to the
GAT host. No host control socket or owner bearer is passed to the guest.
Guest features still depend on its kernel/devices. No extra commercial stack
resource quotas or GAP egress filtering are applied; existing Cloud API quotas
remain unchanged. Unrestricted guest networking can reach internal services;
without resource safeguards workloads can affect the host's availability.

Operator command (on the node host):

```bash
python3 scripts/compose-access.py grant did:gap:<64-hex-agent-identity>
python3 scripts/compose-access.py revoke did:gap:<64-hex-agent-identity>
python3 scripts/compose-access.py list
```

Agent approvals take effect immediately without restarting the node or worker.
The infrastructure is configured once; grant/revoke never changes environment
variables. Approval covers the agent's projects and does not create or publish
an application. Only the operator can run these host commands.

### Managed app quickstart

Compose is for long-running Docker applications, including multi-service stacks
and persistent volumes. Functions remain the lightweight, time-bounded JavaScript
runtime. Compose is experimental and requires operator approval, even on a
public node. On the public deployment, only explicitly approved agents can use it.

The complete flow is: create a project, create its microVM, deploy a Compose
release, then enable its application route. No additional DNS record or TLS
certificate is needed: visitors use `/apps/{project_id}/` on the existing node.

```bash
# NODE, TOKEN and PROJECT come from the identity/project quickstart above.
# Save each request body and reuse its request_id when retrying that operation.
curl -sX POST "$NODE/v1/cloud/projects/$PROJECT/stack/vm" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"request_id":"11111111111111111111111111111111","vcpus":1,"memory_mib":1024,"disk_gib":8,"ports":[8000]}'
# -> 202 with job_id; poll /stack/jobs/{job_id} until it finishes.
# Save result.vm.vm_id as VM. QEMU running does not yet mean Docker is ready.
export VM=vm_returned_by_the_job
```

Deploy your bundle with `/stack/releases` as shown in the next section and poll
that job. The app must listen on a published guest port, for example
`ports: ["8000:8000"]` in Compose. Once it is running, publish its route:

```bash
curl -sX PUT "$NODE/v1/cloud/projects/$PROJECT/stack/ingress" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d "{\"request_id\":\"22222222222222222222222222222222\",\"vm_id\":\"$VM\",\"enabled\":true,\"guest_port\":8000}"
# Poll the returned job, then inspect the resulting URL:
curl -s "$NODE/v1/cloud/projects/$PROJECT/stack/ingress" \
  -H "Authorization: Bearer $TOKEN"
# -> url: https://gap.geta.team/apps/prj_<project-id>/
#    base_path: /apps/prj_<project-id>/
```

`/apps/{project_id}/api/items?x=1` reaches the guest as `/api/items?x=1`.
HTTP methods, bodies, queries, streaming and WebSocket upgrades are forwarded.
The gateway supplies `X-Forwarded-Prefix`; configure the app's public base path
and cookie path, or use relative links. Root-relative assets and redirects are
not automatically rewritten. Apps share the node's browser origin; keep owner
bearers server-side. Publication is public: visitor login belongs to the app.
The returned `routed` flag describes routing configuration, not app health.

### Manage the microVM and publication

Every mutation below requires a fresh `request_id`; all except creation also
require the exact `vm_id`. Mutations return jobs to poll with the same API as
releases. A stale VM ID cannot mutate a replacement VM.

| Method and project suffix | Additional body fields | Result |
|---|---|---|
| GET `/stack/vm` | none | Inspect VM state and allocated resources |
| POST `/stack/vm` | optional `vcpus`, `memory_mib`, `disk_gib`, `ports`, `start`, `ssh_keys` | Create; starts by default |
| POST `/stack/vm/stop` | optional `force: true` | Shut down VM; force explicitly quits it |
| PATCH `/stack/vm` | selected `vcpus`, `memory_mib`, `disk_gib`, `ports` | Reconfigure while stopped; disk growth only |
| POST `/stack/vm/start` | none | Restart the same VM with its data |
| DELETE `/stack/vm` | optional `delete_data: true`, `confirm_data_loss: true` | Destroy stopped VM; retain data by default |
| GET `/stack/ingress` | none | Inspect publication and application URL |
| PUT `/stack/ingress` | `enabled: true`, `guest_port` | Publish one configured guest port |
| PUT `/stack/ingress` | `enabled: false` | Withdraw publication; omit guest_port |

`POST /stack/stop` stops application containers; `POST /stack/vm/stop` stops the
whole VM. VM stop/delete withdraws its route; restarting the same VM restores
an enabled route. Named volumes survive stops, updates and VM resizing.
Explicit data deletion is irreversible. Retained disks do not have an automated
restore API. Approval revocation blocks management but does not stop running
apps or remove visitor routes; operator containment is still required.

### Direct SSH and five public TCP/UDP ports

A managed microVM is a full Linux environment: Compose is optional. Approved
agents can use root SSH, SFTP/SCP, install tools and run services directly.
When public networking is configured, creation reserves **five public port
numbers per VM**. Each slot supports TCP, UDP or both using the same number.
No listener is enabled until you configure a mapping. HTTPS/API/WebSocket
publication under `/apps/{project_id}/` is separate and consumes no slots.

Use **`sites.gap.geta.team`** for direct SSH/TCP/UDP. `gap.geta.team` is behind
Cloudflare and remains the HTTPS management/application origin. GAP chooses
public ports; agents choose only a slot (1–5), guest port and protocol.
Two slots cannot both forward UDP to the same guest port; use distinct guest
ports so replies return through the correct public endpoint. A TCP
mapping to guest port 22 consumes one slot and provides direct SSH with no
bastion. The other four remain available. Do not use example port numbers as
allocations: read the API response.

| Method and project suffix | Body besides request_id and vm_id | Behavior |
|---|---|---|
| GET `/stack/ports` | none; no body needed | Five allocated numbers, mappings and routing state |
| PUT `/stack/ports` | `mappings: [{"slot":1,"guest_port":22,"protocol":"tcp"}]` | Replace all mappings at once, live |
| GET `/stack/ssh` | none; no body needed | Managed public keys, host key/fingerprint and SSH commands |
| PUT `/stack/ssh` | `authorized_keys: ["ssh-ed25519 AAAA..."]` | Replace managed owner SSH keys, live |

Writes return asynchronous jobs, use the exact VM generation, and require the
same agent approval as Compose. Retry a lost response with the same request ID
and body; after a failed job inspect its result before using a new ID. A
`pending` network state means application failed or was interrupted: inspect
and resubmit the intended complete mappings. `routed` is configuration status,
not a guest-service health check.

Use the agent CLI from the repository (Python standard library only):

```bash
export GAP_TOKEN="$TOKEN"
python3 scripts/microvm.py --project "$PROJECT" ports
python3 scripts/microvm.py --project "$PROJECT" --vm "$VM" set-ssh-keys --key ~/.ssh/id_ed25519.pub
# Poll each returned job before submitting another mutation.
python3 scripts/microvm.py --project "$PROJECT" job job_returned_above
python3 scripts/microvm.py --project "$PROJECT" --vm "$VM" set-ports --map 1:22:tcp --map 2:7000:both
python3 scripts/microvm.py --project "$PROJECT" ssh
# Run the returned ssh command; verify the returned host fingerprint.
```

The CLI prints the request ID before submitting; use `--request-id` to retry
that exact operation. `set-ports` without `--map` disables all mappings.
`set-ssh-keys` without `--key` removes all managed owner keys. Only unadorned
Ed25519 public keys are accepted; never upload private keys. Optional
`ssh_keys: [...]` on VM creation installs initial owner keys before first boot.
SSH passwords are disabled. GAP's restricted internal control key remains
separate and is never returned to the agent. Wait for SSH to boot before a live
key update. The guest supports interactive SSH, SFTP/SCP and TCP tunnels.

Numbers and mappings survive VM stop/start; stop closes listeners and active
VM connections, while start restores configured mappings. Destruction frees the
numbers even when the disk is retained; a replacement VM receives a new host
identity and does not inherit the old mappings or keys. Disabling a mapping or
removing a key blocks new access but may leave established sessions alive.
Guest root can independently modify sshd/keys; this API manages the supplied
keys and is not a boundary against that VM's root. Revoking agent approval
blocks management, not already published services or SSH sessions.

The direct endpoints do not add TLS or visitor authentication to TCP/UDP
services: configure those in your application. HTTP-only guest `ports` from
`POST/PATCH /stack/vm` remain separate internal forwards; public mappings can
target any guest port without changing that list or rebooting the VM.

### Runtime environment inside the microVM

GAP injects non-secret networking metadata before SSH and Docker start. Root
SSH sessions and login shells receive these variables automatically. Use
`gap-env COMMAND [ARGS...]` to load the latest values for a service or command,
or source `/etc/gap/runtime.sh` in its startup script. `/etc/gap/runtime.json`
is the machine-readable snapshot; `/etc/gap/runtime.env` is a raw env file.

| Variable | Meaning |
|---|---|
| `GAP_PROJECT_ID`, `GAP_VM_ID` | This guest's project and VM generation |
| `GAP_PUBLIC_HOST` | Direct TCP/UDP hostname, e.g. `sites.gap.geta.team` |
| `GAP_PUBLIC_PORTS` | Comma-separated list of the five allocated public numbers |
| `GAP_PORTS_JSON` | JSON array of slots with `public_port`, `guest_port`, `protocol`; unmapped targets are null |
| `GAP_PORT_1_PUBLIC`, `GAP_PORT_1_GUEST`, `GAP_PORT_1_PROTOCOL` | Per-slot values, also available for slots 2–5; unmapped fields are empty |
| `GAP_HTTP_PORT` | Main **guest listening port** routed for HTTPS/API/WS; empty when ingress is disabled or unconfigured |
| `GAP_INGRESS_ENABLED` | `1` when the HTTP route is configured, otherwise `0`; not a health check |
| `GAP_PUBLIC_URL`, `GAP_WS_URL` | Full public app URL and corresponding WS/WSS base URL; empty when disabled |
| `GAP_BASE_PATH` | App prefix such as `/apps/prj_.../` on the shared origin |
| `GAP_ENV_REVISION` | Digest identifying the current metadata snapshot |

Applications bind to the **guest** port, usually on `0.0.0.0`, not to the
allocated public port. No generic `PORT` variable is overwritten globally;
map it explicitly for the service that needs it.

For correct first startup, configure `/stack/ingress` **before deploying the
application**. You can create the VM with `start: false`, configure its HTTP
route and public mappings while stopped, then start it. The environment is
installed from the guest-only seed before Docker starts. Configuring a route
does not imply that an application already listens there.

GAP's Compose helper automatically loads these variables for `${GAP_HTTP_PORT}`
and other Compose interpolation. To pass the values into a container:

```yaml
services:
  app:
    image: your-app
    env_file:
      - path: /etc/gap/runtime.env
        format: raw
    environment:
      PORT: "${GAP_HTTP_PORT}"
    ports:
      - "${GAP_HTTP_PORT}:${GAP_HTTP_PORT}"
```

`format: raw` preserves the JSON values literally and is supported by the
managed guest's Compose version. For Compose run manually over SSH, use
`gap-env docker compose up -d`. For another process, use `gap-env ./your-app`.

Mapping and ingress changes refresh the files and future SSH sessions at
runtime, without restarting the VM. **Existing processes keep their old
environment.** An application can reread `runtime.json`; otherwise relaunch it
with `gap-env`. Existing Compose containers must be **recreated**, not merely
restarted, to receive new environment values (redeploy the bundle or use
`gap-env docker compose up -d --force-recreate`). To watch updates from a
container, mount `/etc/gap` read-only as a directory; individual file mounts
can retain the old inode when GAP atomically replaces the file.

A failed guest update fails the management job and exposes
`environment_sync_pending: true` in `/stack/vm`; routing may already have
changed. Inspect and retry the operation with a new request ID. The latest
catalog metadata is regenerated on the next VM start and synchronized before
managed Compose commands. The environment contains no owner bearer, controller
credentials, host paths or worker-internal ports.

### Submit a release or update

`POST /v1/cloud/projects/{project}/stack/releases` accepts a JSON bundle:

```json
{
  "request_id": "0123456789abcdef0123456789abcdef",
  "compose_file": "compose.yaml",
  "files": {
    "compose.yaml": "<base64-file-content>",
    "Dockerfile": "<base64-file-content>",
    ".env": "<base64-file-content>"
  }
}
```

Only include files you need. Keys are relative file paths; no absolute paths,
`..`, symlinks or archive extraction. Include referenced local build/config
files in the bundle. Compose interprets the files only inside the guest.
Example using an existing `compose.yaml` (requires jq and OpenSSL):

```bash
export REQUEST_ID=$(openssl rand -hex 16)
COMPOSE_BASE64=$(base64 < compose.yaml | tr -d '\n')

# Save this request before sending so a network retry uses identical input.
jq -n --arg id "$REQUEST_ID" --arg source "$COMPOSE_BASE64" \
  '{request_id:$id,compose_file:"compose.yaml",files:{"compose.yaml":$source}}' \
  > compose-request.json

curl -sX POST "$NODE/v1/cloud/projects/$PROJECT/stack/releases" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  --data-binary @compose-request.json
# 202 -> {"job_id":"job_...","request_id":"...","status":"queued"}
```

For updates, submit a complete new bundle with a new `request_id` to the same
endpoint. Files are stored as an immutable guest release. Compose validates it,
then runs `up --detach --build --remove-orphans --wait --wait-timeout 120` using
a stable project name. Updates may cause downtime or partially change services;
they are not atomic and do not automatically roll back database migrations.

### Poll a job and inspect the latest operation

```bash
export JOB=job_returned_by_submission
curl -s "$NODE/v1/cloud/projects/$PROJECT/stack/jobs/$JOB" \
  -H "Authorization: Bearer $TOKEN"
# -> {job_id,request_id,action,status,created_at,result}

curl -s "$NODE/v1/cloud/projects/$PROJECT/stack" \
  -H "Authorization: Bearer $TOKEN"
# -> {project_id,latest_job,note}; this is NOT live application health.
```

Job states: `queued`, `running`, `succeeded`, `failed`, `interrupted`.
The guest result includes `ok`, command output and exit/timeout information
when available. `succeeded` for start/stop means the command completed, not
continuous application health. Command output can contain application secrets;
never publish it or put your owner bearer in browser code.

### Start, stop, live status and recent logs

All four are asynchronous POST operations and return a job to poll:

```bash
# Start existing containers (not a redeploy).
curl -sX POST "$NODE/v1/cloud/projects/$PROJECT/stack/start" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d "{\"request_id\":\"$(openssl rand -hex 16)\"}"

# Stop app containers; preserve guest, containers and volumes.
curl -sX POST "$NODE/v1/cloud/projects/$PROJECT/stack/stop" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d "{\"request_id\":\"$(openssl rand -hex 16)\"}"

# Run Docker Compose ps --all --format json inside the guest.
curl -sX POST "$NODE/v1/cloud/projects/$PROJECT/stack/status" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d "{\"request_id\":\"$(openssl rand -hex 16)\"}"

# Fetch bounded recent output: Docker Compose logs --tail 200.
curl -sX POST "$NODE/v1/cloud/projects/$PROJECT/stack/logs" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d "{\"request_id\":\"$(openssl rand -hex 16)\"}"
```

Use a fresh id for a new operation; **save and reuse the original id/body when
retrying that operation**. The worker returns the same job for an identical
request and `409 request_id_conflict` if you change its input. Another operation
while one is running returns `409 stack_operation_in_progress`; wait and retry.
An unconfigured public node returns `404 compose_disabled`; missing/wrong agent
credentials return 401, and worker approval failures return 403. A runner
transport failure returns 502; retry with the same id rather than guessing
whether the job was accepted.

After a worker restart, queued/running jobs become `interrupted` without blind
replay. After SSH loss or timeout the remote state may be unknown; inspect status
before submitting a new mutation. Previously attempted guest release ids are
not automatically rerun. This is not exactly-once execution of arbitrary code.

The request transport is limited to 5 MiB including base64/JSON, and output is
bounded. Docker commands have operational timeouts (540s per command, 600s SSH
session); started applications are not given a 600-second lifetime. Fetch larger
build contexts inside the guest. Named volumes persist between updates, but
relative bind mounts point into the new release directory on each update.

Managed VM creation/start/stop, offline CPU/RAM/disk-growth updates and explicit
deletion are available through `/stack/vm`; see the [VM API](./runtime/compose/README.md#vm-api).
Apps publish at `https://gap.geta.team/apps/{project_id}/` through
`GET/PUT /stack/ingress`, using the existing DNS and TLS certificate.
See [setup, API and base-path contract](./runtime/compose/README.md#application-paths-on-the-existing-gap-origin).
Custom customer domains, arbitrary public TCP/UDP forwarding, rollback, backup
and HA are **not implemented**. Your `ports:` publishes
on the guest, not automatically on the GAT host. Approval revocation blocks new
management/admission, but does not stop running apps or revoke visitor/scoped
tokens: the operator must stop/fence the VM for incident containment.
Real KVM and API acceptance tests are provided; production activation requires
operator configuration of the execution host.
