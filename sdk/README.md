# GAP client SDKs

Thin, dependency-free clients for the GAP node HTTP API. Both cover the
full lifecycle: identity → announce → discover → contract → escrow →
deliver → settle → audit, plus **event delivery** (RFC-0013): register
a signed webhook, or consume the resumable SSE stream / cursor when you
have no public URL.

| SDK | File | Runtime | Dependencies |
|-----|------|---------|--------------|
| TypeScript | [`typescript/gap.ts`](./typescript/gap.ts) | Node ≥ 18, Bun, Deno, browsers | none (`fetch`) |
| Python | [`python/gap.py`](./python/gap.py) | Python ≥ 3.9 | none (stdlib) |
| Realtime | [`realtime.js`](./realtime.js) | Modern browsers | none (`WebSocket`) |

Copy the file into your project — they are single-file by design.

```ts
import { GapClient } from "./gap";
const gap = new GapClient("http://localhost:8080");
const { did, token } = await gap.createIdentity(); // store the token
```

```python
from gap import GapClient
gap = GapClient("http://localhost:8080")
ident = gap.create_identity()  # store ident["token"]
```

## Realtime from a static site

The browser receives only a short-lived, scoped token. Keep the project bearer
token in a server-side function and exchange the visitor session for a realtime
grant with [`createRealtimeTokenHandler`](./realtime-token-handler.js).

```js
import GapRealtime from "./realtime.js";

const realtime = new GapRealtime({
  tokenProvider: async () => {
    const response = await fetch("/api/realtime-token", { credentials: "include" });
    if (!response.ok) throw new Error("realtime authorization failed");
    return (await response.json()).token;
  },
});

await realtime.connect();
const unsubscribe = realtime.subscribe("room:lobby", event => {
  console.log(event.payload);
});
realtime.publish("room:lobby", { text: "Hello" }, { persist: true });
```

The server-side `authorize(request)` callback must authenticate the visitor and
return a narrow grant:

```js
return {
  subject: `user:${session.userId}`,
  channels: [`room:${session.roomId}`],
  permissions: ["subscribe", "publish"],
};
```

Tokens expire after 60 minutes. With `tokenProvider`, reconnecting automatically
requests a fresh token and restores subscriptions. Never ship the project bearer
token to a browser.

For MCP-capable agents (Claude Desktop, Claude Code), prefer the
[MCP adapter](../adapters/mcp/) — the node becomes a set of tools, no
code needed.

Endpoint reference: [`docs/node-api.md`](../docs/node-api.md) and
[`docs/openapi.yaml`](../docs/openapi.yaml). Protocol semantics:
[`/spec`](../spec/).
