import http from "node:http";
import crypto from "node:crypto";
import { DatabaseSync } from "node:sqlite";
import { WebSocketServer } from "ws";

const port = Number(process.env.REALTIME_PORT || 8091);
const secret = process.env.REALTIME_SECRET || "";
const dbPath = process.env.REALTIME_DB || "/data/realtime.sqlite";
const MAX_CONNECTIONS = 25;
const MAX_CHANNELS = 25;
const MAX_MESSAGE_BYTES = 64 * 1024;
const CONNECTION_RATE = 30;
const PROJECT_RATE = 300;
const RETENTION_SECONDS = 24 * 60 * 60;
const MAX_PERSISTED_BYTES = 25 * 1024 * 1024;

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
  return claims;
}

function now() { return Math.floor(Date.now() / 1000); }
function bucket(map, key, limit) {
  const minute = Math.floor(Date.now() / 60000);
  const current = map.get(key);
  if (!current || current.minute !== minute) {
    map.set(key, { minute, count: 1 });
    return true;
  }
  if (current.count >= limit) return false;
  current.count++;
  return true;
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
  if (socket.readyState === socket.OPEN) socket.send(JSON.stringify(value));
}
function prune() {
  const timestamp = now();
  db.prepare("DELETE FROM messages WHERE expires_at<=?").run(timestamp);
  const usage = db.prepare("SELECT COALESCE(SUM(size_bytes),0) AS bytes FROM messages").get().bytes;
  if (usage > MAX_PERSISTED_BYTES) {
    db.prepare(`DELETE FROM messages WHERE seq IN
      (SELECT seq FROM messages ORDER BY seq LIMIT
       (SELECT COUNT(*) FROM messages)/10 + 1)`).run();
  }
}
setInterval(prune, 60_000).unref();

const server = http.createServer((req, res) => {
  if (req.url === "/health") {
    res.writeHead(200, { "content-type": "application/json" });
    return res.end('{"status":"ok"}');
  }
  res.writeHead(404).end();
});
const websocket = new WebSocketServer({ noServer: true, maxPayload: MAX_MESSAGE_BYTES });
server.on("upgrade", (req, socket, head) => {
  if (req.url !== "/v1/realtime") return socket.destroy();
  websocket.handleUpgrade(req, socket, head, ws => websocket.emit("connection", ws));
});

websocket.on("connection", socket => {
  const state = { authenticated: false, connectionRate: new Map(), channels: new Set() };
  const timer = setTimeout(() => socket.close(4401, "authentication required"), 5_000);
  socket.on("message", raw => {
    try {
      const message = JSON.parse(raw.toString());
      if (!state.authenticated) {
        if (message.action !== "authenticate") throw new Error("authentication required");
        const claims = parseToken(message.token);
        if (projectConnections(claims.project_id) >= MAX_CONNECTIONS) throw new Error("connection quota exceeded");
        Object.assign(state, {
          authenticated: true,
          projectId: claims.project_id,
          allowedChannels: claims.channels,
          expiresAt: claims.exp,
          connectionId: claims.jti
        });
        clients.set(socket, state);
        clearTimeout(timer);
        return send(socket, { type: "authenticated", project_id: state.projectId, expires_at: state.expiresAt });
      }
      if (state.expiresAt <= now()) return socket.close(4401, "token expired");
      if (!bucket(state.connectionRate, "messages", CONNECTION_RATE) ||
          !bucket(projectRates, state.projectId, PROJECT_RATE)) throw new Error("message rate exceeded");
      if (!allowed(state, message.channel)) throw new Error("channel not allowed");
      if (message.action === "subscribe") {
        const active = projectChannels(state.projectId);
        if (!active.has(message.channel) && active.size >= MAX_CHANNELS) throw new Error("channel quota exceeded");
        state.channels.add(message.channel);
        const after = Number.isSafeInteger(message.after) ? message.after : 0;
        const history = db.prepare(`SELECT seq,body,created_at FROM messages
          WHERE project_id=? AND channel=? AND seq>? AND expires_at>? ORDER BY seq LIMIT 100`)
          .all(state.projectId, message.channel, after, now());
        send(socket, { type: "subscribed", channel: message.channel });
        for (const item of history) send(socket, { type: "message", channel: message.channel,
          seq: item.seq, payload: JSON.parse(item.body), created_at: item.created_at, replay: true });
      } else if (message.action === "unsubscribe") {
        state.channels.delete(message.channel);
        send(socket, { type: "unsubscribed", channel: message.channel });
      } else if (message.action === "publish") {
        if (!state.channels.has(message.channel)) throw new Error("subscribe before publishing");
        const body = JSON.stringify(message.payload ?? null);
        const bytes = Buffer.byteLength(body);
        if (bytes > MAX_MESSAGE_BYTES) throw new Error("message too large");
        let seq = null;
        const createdAt = now();
        if (message.persist === true) {
          prune();
          const used = db.prepare("SELECT COALESCE(SUM(size_bytes),0) AS bytes FROM messages").get().bytes;
          if (used + bytes > MAX_PERSISTED_BYTES) throw new Error("persistence quota exceeded");
          seq = Number(db.prepare(`INSERT INTO messages(project_id,channel,body,size_bytes,created_at,expires_at)
            VALUES(?,?,?,?,?,?)`).run(state.projectId, message.channel, body, bytes,
              createdAt, createdAt + RETENTION_SECONDS).lastInsertRowid);
        }
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
  });
  socket.on("close", () => { clearTimeout(timer); clients.delete(socket); });
  socket.on("error", () => clients.delete(socket));
});

server.listen(port, "0.0.0.0");
