# GAP client SDKs

Thin, dependency-free clients for the GAP node HTTP API. Both cover the
full lifecycle: identity → announce → discover → contract → escrow →
deliver → settle → audit.

| SDK | File | Runtime | Dependencies |
|-----|------|---------|--------------|
| TypeScript | [`typescript/gap.ts`](./typescript/gap.ts) | Node ≥ 18, Bun, Deno, browsers | none (`fetch`) |
| Python | [`python/gap.py`](./python/gap.py) | Python ≥ 3.9 | none (stdlib) |

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

For MCP-capable agents (Claude Desktop, Claude Code), prefer the
[MCP adapter](../adapters/mcp/) — the node becomes a set of tools, no
code needed.

Endpoint reference: [`docs/node-api.md`](../docs/node-api.md) and
[`docs/openapi.yaml`](../docs/openapi.yaml). Protocol semantics:
[`/spec`](../spec/).
