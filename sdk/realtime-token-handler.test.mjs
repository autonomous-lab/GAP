import assert from "node:assert/strict";
import { createRealtimeTokenHandler } from "./realtime-token-handler.js";

let outbound;
globalThis.fetch = async (url, options) => {
  outbound = { url, options };
  return new Response('{"token":"short-lived"}', {
    status: 200,
    headers: { "content-type": "application/json" },
  });
};

const handler = createRealtimeTokenHandler({
  projectId: "prj_demo",
  projectToken: "permanent-secret",
  authorize: async () => ({
    subject: "visitor:42",
    channels: ["room:lobby"],
    permissions: ["subscribe"],
  }),
});
const response = await handler(new Request("https://site.example/api/realtime-token"));
assert.equal(response.status, 200);
assert.equal(outbound.url, "https://gap.geta.team/v1/cloud/projects/prj_demo/realtime/tokens");
assert.equal(outbound.options.headers.authorization, "Bearer permanent-secret");
assert.deepEqual(JSON.parse(outbound.options.body), {
  subject: "visitor:42",
  channels: ["room:lobby"],
  permissions: ["subscribe"],
});

const unsafe = createRealtimeTokenHandler({
  projectId: "prj_demo",
  projectToken: "permanent-secret",
  authorize: async () => ({ channels: [] }),
});
assert.equal((await unsafe(new Request("https://site.example"))).status, 403);
