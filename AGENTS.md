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
- private static hosting: Basic Auth mandatory, 1 MiB per file, 100 MiB total,
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

Functions have a 30-second execution timeout. The sandbox is allocated 1 CPU,
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
oversized files, control bytes, excessive padding/obfuscation, embedded private
keys or recognizable API credentials, `<base>` overrides and meta refreshes.
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

Free projects receive 1 MiB per file, 100 MiB across retained versions, 5,000
files, 5 versions, 20 requests/second and 1 GiB per rolling 30-day period.
Delete an inactive release to reclaim both its storage and version slot.

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
