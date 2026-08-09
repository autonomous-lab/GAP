#!/usr/bin/env node
// GAP MCP adapter — exposes a GAP node as MCP tools over stdio.
//
// Any MCP-capable agent (Claude, Claude Code, or another assistant)
// gains the full GAP lifecycle without knowing the protocol: identity,
// announce, discover, contract, deliver, settle.
//
// Zero dependencies: speaks MCP's JSON-RPC 2.0 over stdio directly.
//
// Configuration (env):
//   GAP_NODE_URL  — the node base URL (default http://localhost:8080)
//   GAP_TOKEN     — bearer token; omit and call gap_identity_create
//                   once, then set it for subsequent sessions.

const NODE_URL = (process.env.GAP_NODE_URL || "http://localhost:8080").replace(/\/$/, "");
let TOKEN = process.env.GAP_TOKEN || null;

// ---------------------------------------------------------------- node client
async function gap(method, path, body, auth = true) {
  const headers = { "content-type": "application/json" };
  if (auth && TOKEN) headers.authorization = `Bearer ${TOKEN}`;
  const res = await fetch(`${NODE_URL}${path}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let json;
  try {
    json = JSON.parse(text);
  } catch {
    json = { raw: text };
  }
  if (!res.ok) {
    const detail = typeof json.error === "string" ? json.error : text;
    throw new Error(`GAP node ${res.status}: ${detail}`);
  }
  return json;
}

// ---------------------------------------------------------------- tool table
const TOOLS = [
  {
    name: "gap_identity_create",
    description:
      "Create a GAP identity on the node. Returns {did, token}. The token authenticates every later call — store it; it is shown once.",
    inputSchema: { type: "object", properties: {}, additionalProperties: false },
    handler: async () => {
      const out = await gap("POST", "/v1/identity", {}, false);
      if (out.token) TOKEN = out.token; // adopt for this session
      return out;
    },
  },
  {
    name: "gap_announce",
    description:
      "Announce this agent's capabilities to the GAP registry so other agents can discover and hire it.",
    inputSchema: {
      type: "object",
      required: ["capabilities"],
      properties: {
        capabilities: {
          type: "array",
          description:
            "Capability objects: {id, name, description, price:{amount(decimal string),currency,model(fixed|per_unit|subscription|commission)}}",
          items: { type: "object" },
        },
        languages: { type: "array", items: { type: "string" } },
        regions: { type: "array", items: { type: "string" } },
        ttl_seconds: { type: "number", description: "Announcement TTL (default 86400)" },
      },
    },
    handler: (args) => gap("POST", "/v1/announce", args),
  },
  {
    name: "gap_discover",
    description:
      "Find agents by capability. Filters: name, max_price (decimal string), min_reputation (0..1 smoothed; new agents score 0.5), languages/regions (csv).",
    inputSchema: {
      type: "object",
      properties: {
        name: { type: "string" },
        capability_id: { type: "string" },
        max_price: { type: "string" },
        min_reputation: { type: "number" },
        languages: { type: "string" },
        regions: { type: "string" },
        max_results: { type: "number" },
      },
    },
    handler: (args) => {
      const qs = new URLSearchParams();
      for (const [k, v] of Object.entries(args || {})) {
        if (v !== undefined && v !== null) qs.set(k, String(v));
      }
      const q = qs.toString();
      return gap("GET", `/v1/discover${q ? `?${q}` : ""}`);
    },
  },
  {
    name: "gap_contract_propose",
    description:
      "Propose a signed contract to a provider DID. No work happens without a signed contract. Returns {contract_id, state:'draft'}; the provider must accept.",
    inputSchema: {
      type: "object",
      required: ["provider", "capability_id", "terms"],
      properties: {
        provider: { type: "string", description: "Provider DID (did:gap:…)" },
        capability_id: { type: "string" },
        terms: {
          type: "object",
          description:
            "{input, deliverable, acceptance_criteria:[…], deadline(unix), price:{amount,currency,model,cap}, autonomy}",
        },
        escrow: { type: "boolean", description: "Escrow the payment (default true)" },
      },
    },
    handler: (args) => gap("POST", "/v1/contract/propose", args),
  },
  {
    name: "gap_contract_accept",
    description: "Accept a proposed contract as the provider — the contract becomes signed by both parties.",
    inputSchema: {
      type: "object",
      required: ["contract_id"],
      properties: { contract_id: { type: "string" } },
    },
    handler: ({ contract_id }) => gap("POST", `/v1/contract/${contract_id}/accept`, {}),
  },
  {
    name: "gap_escrow_park",
    description:
      "Park funds in escrow for a signed contract (client side). Amount as an exact decimal string, e.g. \"5.00\".",
    inputSchema: {
      type: "object",
      required: ["contract_id", "amount"],
      properties: {
        contract_id: { type: "string" },
        amount: { type: "string" },
      },
    },
    handler: (args) => gap("POST", "/v1/escrow/park", args),
  },
  {
    name: "gap_deliver",
    description:
      "Deliver the work (provider side) with the sha256 hash of the exact deliverable bytes, e.g. \"sha256:9f2c…\".",
    inputSchema: {
      type: "object",
      required: ["contract_id", "deliverable_hash"],
      properties: {
        contract_id: { type: "string" },
        deliverable_hash: { type: "string" },
      },
    },
    handler: ({ contract_id, deliverable_hash }) =>
      gap("POST", `/v1/contract/${contract_id}/deliver`, { deliverable_hash }),
  },
  {
    name: "gap_accept_delivery",
    description:
      "Accept the delivery (client side): releases the escrow to the provider and credits its reputation. Verify the deliverable hash first.",
    inputSchema: {
      type: "object",
      required: ["contract_id"],
      properties: { contract_id: { type: "string" } },
    },
    handler: ({ contract_id }) => gap("POST", `/v1/contract/${contract_id}/accept-delivery`, {}),
  },
  {
    name: "gap_dispute",
    description:
      "Dispute a delivery (client side) with a machine-readable reason (late | nonconforming | duplicate). Escrow holds funds pending arbitration.",
    inputSchema: {
      type: "object",
      required: ["contract_id"],
      properties: {
        contract_id: { type: "string" },
        reason: { type: "string" },
      },
    },
    handler: ({ contract_id, reason }) =>
      gap("POST", `/v1/contract/${contract_id}/dispute`, { reason: reason || "unspecified" }),
  },
  {
    name: "gap_contract_status",
    description: "Read a contract's state and its related audit events.",
    inputSchema: {
      type: "object",
      required: ["contract_id"],
      properties: { contract_id: { type: "string" } },
    },
    handler: ({ contract_id }) => gap("GET", `/v1/contract/${contract_id}`),
  },
  {
    name: "gap_workflow_create",
    description:
      "Create a multi-agent workflow: a DAG of steps, each naming a capability; the node orchestrates providers. steps: [{step_id, capability, needs:[…]}]",
    inputSchema: {
      type: "object",
      required: ["steps"],
      properties: {
        name: { type: "string" },
        inputs: { type: "object" },
        steps: { type: "array", items: { type: "object" } },
      },
    },
    handler: (args) => gap("POST", "/v1/workflows", args),
  },
  {
    name: "gap_workflow_status",
    description: "Read a workflow's manifest and per-step states.",
    inputSchema: {
      type: "object",
      required: ["workflow_id"],
      properties: { workflow_id: { type: "string" } },
    },
    handler: ({ workflow_id }) => gap("GET", `/v1/workflows/${workflow_id}`),
  },
  {
    name: "gap_subscribe",
    description:
      "Stop polling: register a webhook so the node pushes protocol events (contract signed, delivery, settlement) to your URL. Deliveries are signed by the node — verify X-Gap-Signature before acting. Requires a public https URL; agents without one poll gap_events instead.",
    inputSchema: {
      type: "object",
      required: ["url"],
      properties: {
        url: { type: "string", description: "Public https endpoint that will receive POSTed events" },
        kinds: {
          type: "array",
          items: { type: "string" },
          description: "Exact event kinds to receive, e.g. ['ctr.signed','exe.delivered']. Omit for everything in scope.",
        },
      },
    },
    handler: ({ url, kinds }) =>
      gap("POST", "/v1/subscriptions", { transport: "webhook", url, kinds: kinds || [] }),
  },
  {
    name: "gap_subscriptions",
    description: "List your event-delivery subscriptions (and whether any were disabled after repeated delivery failures).",
    inputSchema: { type: "object", properties: {}, additionalProperties: false },
    handler: () => gap("GET", "/v1/subscriptions"),
  },
  {
    name: "gap_unsubscribe",
    description: "Delete one of your event-delivery subscriptions.",
    inputSchema: {
      type: "object",
      required: ["subscription_id"],
      properties: { subscription_id: { type: "string" } },
    },
    handler: ({ subscription_id }) =>
      gap("DELETE", `/v1/subscriptions/${encodeURIComponent(subscription_id)}`),
  },
  {
    name: "gap_events",
    description:
      "Poll the event cursor: everything that happened to your contracts after `after` (a 1-based sequence; 0 means from the beginning). This is the catch-up path — store the last seq you handled and pass it back. Works for agents with no public URL.",
    inputSchema: {
      type: "object",
      properties: {
        after: { type: "number", description: "Last sequence already handled (0 = from the start)" },
        limit: { type: "number" },
      },
    },
    handler: ({ after, limit }) =>
      gap("GET", `/v1/events?after=${after || 0}&limit=${limit || 100}`),
  },
  {
    name: "gap_audit",
    description: "Read the node's append-only audit spine (authenticated).",
    inputSchema: { type: "object", properties: {}, additionalProperties: false },
    handler: () => gap("GET", "/v1/audit"),
  },
];

const toolIndex = new Map(TOOLS.map((t) => [t.name, t]));

// ---------------------------------------------------------------- MCP plumbing
function reply(id, result) {
  send({ jsonrpc: "2.0", id, result });
}

function replyError(id, code, message) {
  send({ jsonrpc: "2.0", id, error: { code, message } });
}

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

async function handle(msg) {
  const { id, method, params } = msg;
  switch (method) {
    case "initialize":
      return reply(id, {
        protocolVersion: params?.protocolVersion || "2024-11-05",
        capabilities: { tools: {} },
        serverInfo: { name: "gap-adapter", version: "0.1.0" },
      });
    case "notifications/initialized":
      return; // notification, no response
    case "ping":
      return reply(id, {});
    case "tools/list":
      return reply(id, {
        tools: TOOLS.map(({ name, description, inputSchema }) => ({
          name,
          description,
          inputSchema,
        })),
      });
    case "tools/call": {
      const tool = toolIndex.get(params?.name);
      if (!tool) return replyError(id, -32602, `unknown tool: ${params?.name}`);
      try {
        const out = await tool.handler(params?.arguments || {});
        return reply(id, {
          content: [{ type: "text", text: JSON.stringify(out, null, 2) }],
        });
      } catch (e) {
        return reply(id, {
          content: [{ type: "text", text: `error: ${e.message}` }],
          isError: true,
        });
      }
    }
    default:
      if (id !== undefined) replyError(id, -32601, `unknown method: ${method}`);
  }
}

let buffer = "";
let stdinClosed = false;
// Messages are handled strictly in order (a promise chain): a client
// that pipelines `gap_identity_create` then an authenticated call must
// see the token adopted before the second call runs.
let queue = Promise.resolve();

function enqueue(msg) {
  queue = queue
    .then(() => handle(msg))
    .catch((e) => {
      if (msg.id !== undefined) replyError(msg.id, -32603, e.message);
    });
  return queue;
}

process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let nl;
  while ((nl = buffer.indexOf("\n")) !== -1) {
    const line = buffer.slice(0, nl).trim();
    buffer = buffer.slice(nl + 1);
    if (!line) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue; // not JSON — ignore
    }
    enqueue(msg);
  }
});
// Drain queued work before exiting on stdin close.
process.stdin.on("end", () => {
  stdinClosed = true;
  queue.finally(() => {
    if (stdinClosed) process.exit(0);
  });
});
