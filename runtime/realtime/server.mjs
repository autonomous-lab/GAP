import http from "node:http";
import crypto from "node:crypto";
import { DatabaseSync } from "node:sqlite";
import { WebSocketServer } from "ws";

const port = Number(process.env.REALTIME_PORT || 8091);
const secret = process.env.REALTIME_SECRET || "";
const dbPath = process.env.REALTIME_DB || "/data/realtime.sqlite";
const gapNodeInternalUrl = process.env.GAP_NODE_INTERNAL_URL || "http://gap-node:8080";
const FREE_CONNECTIONS = 25;
const HARD_CONNECTIONS = 100;
const FREE_CHANNELS = 25;
const HARD_CHANNELS = 100;
const FREE_MESSAGE_BYTES = 64 * 1024;
const HARD_MESSAGE_BYTES = 256 * 1024;
const FREE_CONNECTION_RATE = 30;
const HARD_CONNECTION_RATE = 300;
const FREE_PROJECT_RATE = 300;
const HARD_PROJECT_RATE = 3000;
const RETENTION_SECONDS = 24 * 60 * 60;
const FREE_PERSISTED_BYTES = 25 * 1024 * 1024;
const HARD_PERSISTED_BYTES = 100 * 1024 * 1024;
const MIB = 1024 * 1024;

if (!secret) throw new Error("REALTIME_SECRET is required");
const db = new DatabaseSync(dbPath);
db.exec(`PRAGMA journal_mode=WAL;
CREATE TABLE IF NOT EXISTS messages(
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id TEXT NOT NULL,
  channel TEXT NOT NULL,
  body TEXT NOT NULL,
  size_bytes INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS messages_channel_seq ON messages(project_id,channel,seq);`);

const clients = new Map();
const projectRates = new Map();

function parseToken(token) {
  const [encoded, signature, extra] = String(token || "").split(".");
  if (!encoded || !signature || extra) throw new Error("invalid token");
  const expected = crypto.createHmac("sha256", secret).update(encoded).digest("hex");
  const a = Buffer.from(signature, "hex");
  const b = Buffer.from(expected, "hex");
  if (a.length !== b.length || !crypto.timingSafeEqual(a, b)) throw new Error("invalid token");
  const claims = JSON.parse(Buffer.from(encoded, "base64url").toString("utf8"));
  if (!claims.project_id || !Number.isSafeInteger(claims.exp) || claims.exp <= now()) {
    throw new Error("expired token");
  }
  claims.channels = Array.isArray(claims.channels) ? claims.channels : [];
  claims.permissions = Array.isArray(claims.permissions)
    ? claims.permissions
    : ["subscribe", "publish"];
  if (claims.permissions.length === 0 ||
      claims.permissions.some(permission => !["subscribe", "publish"].includes(permission))) {
    throw new Error("invalid permissions");
  }
  return claims;
}

const policyGenerations = new Map();
async function workloadPolicies(projects) {
  try {
    const response = await fetch(new URL("/internal/workload-policy", gapNodeInternalUrl), {
      method: "POST", headers: { Authorization: `Bearer ${secret}`, "Content-Type": "application/json" },
      body: JSON.stringify({ project_ids: projects }), signal: AbortSignal.timeout(3000)
    });
    if (!response.ok) return {};
    const data = await response.json();
    const policies = data.policies || {};
    for (const project of projects) {
      const p = policies[project], known = policyGenerations.get(project) || 0;
      if (!p || !Number.isSafeInteger(p.generation) || p.generation < known) {
        policies[project] = { allowed: false }; continue;
      }
      policyGenerations.set(project, p.generation);
    }
    return policies;
  } catch { return {}; }
}

setInterval(() => {
  for (const [socket,state] of clients) {
    if (performance.now() >= state.policyExpires || state.expiresAt <= now()) socket.terminate();
  }
}, 250).unref();

let refreshingPolicy = false;
setInterval(async () => {
  if (refreshingPolicy || !clients.size) return;
  refreshingPolicy = true;
  try {
    const projects = [...new Set([...clients.values()].map(s => s.projectId))];
    for (let offset = 0; offset < projects.length; offset += 1000) {
      const batch = projects.slice(offset, offset + 1000), policies = await workloadPolicies(batch);
      for (const [socket, state] of clients) {
        if (!batch.includes(state.projectId)) continue;
        const policy = policies[state.projectId];
        if (policy?.allowed !== true || policy.generation < (policyGenerations.get(state.projectId) || 0)) socket.terminate();
        else state.policyExpires = performance.now() + 5000;
      }
    }
  } finally { refreshingPolicy = false; }
}, 1000).unref();

async function customDomainAllows(hostname, projectId) {
  const url = new URL("/internal/realtime/custom-domain", gapNodeInternalUrl);
  url.searchParams.set("hostname", hostname);
  url.searchParams.set("project_id", projectId);
  try {
    const response = await fetch(url, {
      headers: { Authorization: `Bearer ${secret}` },
      signal: AbortSignal.timeout(3000)
    });
    return response.ok;
  } catch {
    return false;
  }
}

async function spendCredits(projectId, charges) {
  const filtered = Object.fromEntries(Object.entries(charges).filter(([, amount]) => amount > 0));
  if (Object.keys(filtered).length === 0) return;
  const url = new URL("/internal/realtime/credits/spend", gapNodeInternalUrl);
  let response;
  try {
    response = await fetch(url, {
      method: "POST",
      headers: { Authorization: `Bearer ${secret}`, "Content-Type": "application/json" },
      body: JSON.stringify({ project_id: projectId, charges: filtered }),
      signal: AbortSignal.timeout(3000)
    });
  } catch {
    throw new Error("credit service unavailable");
  }
  if (!response.ok) {
    const result = await response.json().catch(() => ({}));
    throw new Error(result?.error?.message || "insufficient realtime credits");
  }
}

function now() { return Math.floor(Date.now() / 1000); }
function bucketCount(map, key) {
  const minute = Math.floor(Date.now() / 60000);
  const current = map.get(key);
  if (!current || current.minute !== minute) {
    map.set(key, { minute, count: 1 });
    return 1;
  }
  current.count++;
  return current.count;
}
function projectConnections(projectId) {
  let count = 0;
  for (const client of clients.values()) if (client.projectId === projectId) count++;
  return count;
}
function projectChannels(projectId) {
  const channels = new Set();
  for (const client of clients.values()) {
    if (client.projectId === projectId) for (const channel of client.channels) channels.add(channel);
  }
  return channels;
}
function allowed(client, channel) {
  return typeof channel === "string" && channel.length <= 128 &&
    (client.allowedChannels.length === 0 || client.allowedChannels.includes(channel));
}
function send(socket, value) {
  const state=clients.get(socket);
  if (state && (performance.now() >= state.policyExpires || state.expiresAt <= now())) return socket.terminate();
  if (socket.readyState === socket.OPEN) socket.send(JSON.stringify(value));
}
function prune() {
  const timestamp = now();
  db.prepare("DELETE FROM messages WHERE expires_at<=?").run(timestamp);
}
setInterval(prune, 60_000).unref();

const server = http.createServer((req, res) => {
  if (req.url === "/health") {
    res.writeHead(200, { "content-type": "application/json" });
    return res.end('{"status":"ok"}');
  }
  res.writeHead(404).end();
});
const websocket = new WebSocketServer({ noServer: true, maxPayload: HARD_MESSAGE_BYTES + 16 * 1024 });
server.on("upgrade", (req, socket, head) => {
  if (req.url !== "/v1/realtime") return socket.destroy();
  const customHost = String(req.headers["x-gap-custom-host"] || "")
    .trim().toLowerCase().replace(/\.$/, "");
  websocket.handleUpgrade(req, socket, head, ws => {
    ws.gapCustomHost = customHost;
    websocket.emit("connection", ws);
  });
});

websocket.on("connection", socket => {
  const state = { authenticated: false, authenticating: false, connectionRate: new Map(), channels: new Set(), renewalTimer: null, paidConnection: false };
  const timer = setTimeout(() => socket.close(4401, "authentication required"), 5_000);
  async function receive(raw) {
    try {
      const message = JSON.parse(raw.toString());
      if (!state.authenticated) {
        if (state.authenticating) throw new Error("authentication in progress");
        if (message.action !== "authenticate") throw new Error("authentication required");
        const claims = parseToken(message.token);
        if (socket.gapCustomHost) {
          state.authenticating = true;
          const allowed = await customDomainAllows(socket.gapCustomHost, claims.project_id);
          state.authenticating = false;
          if (!allowed) throw new Error("token project does not match custom domain");
        }
        const policy = (await workloadPolicies([claims.project_id]))[claims.project_id];
        if (policy?.allowed !== true || policy.generation < (policyGenerations.get(claims.project_id) || 0)) throw new Error("project suspended or policy unavailable");
        const policyExpires = performance.now() + 5000;
        const connectionCount = projectConnections(claims.project_id);
        if (connectionCount >= HARD_CONNECTIONS) throw new Error("hard connection limit exceeded");
        if (connectionCount >= FREE_CONNECTIONS) {
          await spendCredits(claims.project_id, { connection_hour: 1 });
          state.paidConnection = true;
        }
        if (socket.readyState !== 1 || performance.now() >= policyExpires || claims.exp <= now() || policy.generation < (policyGenerations.get(claims.project_id) || 0)) throw new Error("authentication lease expired");
        Object.assign(state, {
          authenticated: true,
          policyExpires,
          projectId: claims.project_id,
          allowedChannels: claims.channels,
          permissions: claims.permissions,
          subject: typeof claims.subject === "string" ? claims.subject : null,
          expiresAt: claims.exp,
          connectionId: claims.jti
        });
        clients.set(socket, state);
        if (state.paidConnection) {
          state.renewalTimer = setInterval(async () => {
            if (projectConnections(state.projectId) <= FREE_CONNECTIONS) return;
            try {
              await spendCredits(state.projectId, { connection_hour: 1 });
            } catch {
              socket.close(4402, "realtime credits exhausted");
            }
          }, 60 * 60 * 1000);
          state.renewalTimer.unref();
        }
        clearTimeout(timer);
        return send(socket, { type: "authenticated", project_id: state.projectId,
          subject: state.subject, permissions: state.permissions, expires_at: state.expiresAt });
      }
      if (performance.now() >= state.policyExpires) return socket.terminate();
      if (state.expiresAt <= now()) return socket.close(4401, "token expired");
      const connectionRate = bucketCount(state.connectionRate, "messages");
      const projectRate = bucketCount(projectRates, state.projectId);
      if (connectionRate > HARD_CONNECTION_RATE || projectRate > HARD_PROJECT_RATE) {
        throw new Error("hard message rate exceeded");
      }
      const charges = {};
      if (connectionRate > FREE_CONNECTION_RATE || projectRate > FREE_PROJECT_RATE) {
        charges.rate_overage = 1;
      }
      if (!allowed(state, message.channel)) throw new Error("channel not allowed");
      if (message.action === "subscribe") {
        if (!state.permissions.includes("subscribe")) throw new Error("subscribe not allowed");
        const active = projectChannels(state.projectId);
        if (!active.has(message.channel) && active.size >= HARD_CHANNELS) throw new Error("hard channel limit exceeded");
        if (!active.has(message.channel) && active.size >= FREE_CHANNELS) charges.channel_activation = 1;
        await spendCredits(state.projectId, charges);
        state.channels.add(message.channel);
        const after = Number.isSafeInteger(message.after) ? message.after : 0;
        const history = db.prepare(`SELECT seq,body,created_at FROM messages
          WHERE project_id=? AND channel=? AND seq>? AND expires_at>? ORDER BY seq LIMIT 100`)
          .all(state.projectId, message.channel, after, now());
        send(socket, { type: "subscribed", channel: message.channel });
        for (const item of history) send(socket, { type: "message", channel: message.channel,
          seq: item.seq, payload: JSON.parse(item.body), created_at: item.created_at, replay: true });
      } else if (message.action === "unsubscribe") {
        if (!state.permissions.includes("subscribe")) throw new Error("subscribe not allowed");
        await spendCredits(state.projectId, charges);
        state.channels.delete(message.channel);
        send(socket, { type: "unsubscribed", channel: message.channel });
      } else if (message.action === "publish") {
        if (!state.permissions.includes("publish")) throw new Error("publish not allowed");
        if (!state.channels.has(message.channel)) throw new Error("subscribe before publishing");
        const body = JSON.stringify(message.payload ?? null);
        const bytes = Buffer.byteLength(body);
        if (bytes > HARD_MESSAGE_BYTES) throw new Error("hard message size limit exceeded");
        if (bytes > FREE_MESSAGE_BYTES) {
          charges.payload_chunk = Math.ceil(bytes / FREE_MESSAGE_BYTES) - 1;
        }
        let seq = null;
        const createdAt = now();
        if (message.persist === true) {
          prune();
          const used = db.prepare("SELECT COALESCE(SUM(size_bytes),0) AS bytes FROM messages WHERE project_id=?")
            .get(state.projectId).bytes;
          if (used + bytes > HARD_PERSISTED_BYTES) throw new Error("hard persistence limit exceeded");
          const before = Math.ceil(Math.max(used - FREE_PERSISTED_BYTES, 0) / MIB);
          const after = Math.ceil(Math.max(used + bytes - FREE_PERSISTED_BYTES, 0) / MIB);
          if (after > before) charges.persisted_megabyte = after - before;
          await spendCredits(state.projectId, charges);
          seq = Number(db.prepare(`INSERT INTO messages(project_id,channel,body,size_bytes,created_at,expires_at)
            VALUES(?,?,?,?,?,?)`).run(state.projectId, message.channel, body, bytes,
              createdAt, createdAt + RETENTION_SECONDS).lastInsertRowid);
        } else await spendCredits(state.projectId, charges);
        for (const [peer, client] of clients) {
          if (client.projectId === state.projectId && client.channels.has(message.channel)) {
            send(peer, { type: "message", channel: message.channel, seq,
              payload: message.payload ?? null, created_at: createdAt, replay: false });
          }
        }
      } else throw new Error("unknown action");
    } catch (error) {
      send(socket, { type: "error", error: String(error.message || error) });
    }
  }
  // Preserve frame ordering across asynchronous admission checks, with a bounded
  // queue so unauthenticated clients cannot accumulate unlimited pending work.
  let processing = Promise.resolve(), pending = 0, pendingBytes = 0;
  socket.on("message", raw => {
    pending++; pendingBytes += raw.length;
    if (pending > 32 || pendingBytes > 1024 * 1024) return socket.terminate();
    processing = processing.then(() => socket.readyState === 1 ? receive(raw) : undefined)
      .catch(() => socket.terminate()).finally(() => { pending--; pendingBytes -= raw.length; });
  });
  socket.on("close", () => { clearTimeout(timer); clearInterval(state.renewalTimer); clients.delete(socket); });
  socket.on("error", () => { clearInterval(state.renewalTimer); clients.delete(socket); });
});

server.listen(port, "0.0.0.0");
