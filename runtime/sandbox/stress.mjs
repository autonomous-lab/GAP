// Run in an isolated container with the production CPU, memory and PID limits.
import http from "node:http";
import { spawn } from "node:child_process";
import { once } from "node:events";

const count = Number(process.argv[2] || 128);
const holdMs = Number(process.argv[3] || 5000);
const backend = http.createServer(async (req, res) => {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  if (req.url === "/internal/workload-policy") {
    const ids = JSON.parse(Buffer.concat(chunks).toString()).project_ids;
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ policies: Object.fromEntries(ids.map(id => [id, { allowed: true, generation: 1 }])) }));
    return;
  }
  res.writeHead(200, { "content-type": "application/json" });
  res.end('{"ok":true}');
});
backend.listen(0, "127.0.0.1");
await once(backend, "listening");
const reservation = http.createServer();
reservation.listen(0, "127.0.0.1");
await once(reservation, "listening");
const port = reservation.address().port;
await new Promise(resolve => reservation.close(resolve));
const sandbox = spawn(process.execPath, [new URL("./server.mjs", import.meta.url).pathname], {
  env: {
    SANDBOX_TOKEN: "stress-only",
    SANDBOX_ADDR: "127.0.0.1",
    SANDBOX_PORT: String(port),
    CAPABILITY_URL: `http://127.0.0.1:${backend.address().port}`,
    SANDBOX_MAX_GLOBAL_CONCURRENCY: String(count),
    SANDBOX_MAX_PROJECT_CONCURRENCY: String(count),
    SANDBOX_MAX_QUEUE: String(count),
  },
  stdio: ["ignore", "ignore", "pipe"],
});
sandbox.stderr.on("data", chunk => process.stderr.write(chunk));
try {
  for (let attempt = 0; attempt < 100; attempt++) {
    try {
      if ((await fetch(`http://127.0.0.1:${port}/health`)).ok) break;
    } catch {}
    await new Promise(resolve => setTimeout(resolve, 20));
  }
  const source = `async () => { await new Promise(resolve => setTimeout(resolve, ${holdMs})); return 1; }`;
  const start = performance.now();
  const outcomes = await Promise.all(Array.from({ length: count }, async () => {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/invoke`, {
        method: "POST",
        headers: { authorization: "Bearer stress-only", "content-type": "application/json" },
        body: JSON.stringify({ project_id: "stress", source }),
        signal: AbortSignal.timeout(45000),
      });
      return response.status;
    } catch { return 0; }
  }));
  const statuses = Object.fromEntries([...new Set(outcomes)].map(status => [status, outcomes.filter(value => value === status).length]));
  process.stdout.write(JSON.stringify({ count, holdMs, elapsedMs: Math.round(performance.now() - start), statuses }) + "\n");
  if (statuses[200] !== count) process.exitCode = 1;
} finally {
  sandbox.kill();
  backend.closeAllConnections();
  backend.close();
}
