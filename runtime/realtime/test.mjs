import assert from "node:assert/strict";
import crypto from "node:crypto";
import WebSocket from "ws";

const secret = process.env.REALTIME_SECRET || "test-secret";
const claims = Buffer.from(JSON.stringify({
  project_id: "prj_test",
  channels: ["contract:demo"],
  exp: Math.floor(Date.now() / 1000) + 60,
  jti: "test"
})).toString("base64url");
const token = `${claims}.${crypto.createHmac("sha256", secret).update(claims).digest("hex")}`;
const socket = new WebSocket(process.env.REALTIME_URL || "ws://127.0.0.1:8091/v1/realtime");
const received = [];
socket.on("message", data => received.push(JSON.parse(data.toString())));
await new Promise((resolve, reject) => {
  socket.once("open", resolve);
  socket.once("error", reject);
});
socket.send(JSON.stringify({ action: "authenticate", token }));
socket.send(JSON.stringify({ action: "subscribe", channel: "contract:demo" }));
await new Promise(resolve => setTimeout(resolve, 50));
socket.send(JSON.stringify({
  action: "publish",
  channel: "contract:demo",
  payload: { answer: 42 },
  persist: true
}));
await new Promise(resolve => setTimeout(resolve, 100));
assert(received.some(message => message.type === "authenticated"));
assert(received.some(message => message.type === "subscribed"));
assert(received.some(message => message.type === "message" && message.payload.answer === 42));
socket.close();
