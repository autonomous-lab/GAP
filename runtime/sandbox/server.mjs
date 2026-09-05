import http from "node:http";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const addr = process.env.SANDBOX_ADDR || "0.0.0.0";
const port = Number(process.env.SANDBOX_PORT || "8090");
const token = process.env.SANDBOX_TOKEN || "";
const maxBody = Number(process.env.SANDBOX_MAX_BODY_BYTES || "600000");
const timeoutMs = Number(process.env.SANDBOX_TIMEOUT_MS || "1000");
const capabilityTimeoutMs = Number(process.env.SANDBOX_CAPABILITY_TIMEOUT_MS || "35000");
const capabilityUrl = process.env.CAPABILITY_URL || "http://gap-node:8080/internal/functions/capability";
const maxCapabilities = Number(process.env.SANDBOX_MAX_CAPABILITIES || "32");
const workerPath = fileURLToPath(new URL("./worker.mjs", import.meta.url));

if (!token) throw new Error("SANDBOX_TOKEN is required");

function reply(res, status, body) {
  const data = JSON.stringify(body);
  res.writeHead(status, {
    "content-type": "application/json",
    "content-length": Buffer.byteLength(data),
    "cache-control": "no-store",
    "x-content-type-options": "nosniff",
  });
  res.end(data);
}

function runWorker(payload) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [workerPath], {
      env: {},
      stdio: ["pipe", "pipe", "pipe"],
    });
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      reject(new Error("function timed out"));
    }, timeoutMs);
    let stdout = "";
    child.stdout.on("data", chunk => {
      stdout += chunk;
      if (stdout.length > maxBody) child.kill("SIGKILL");
    });
    child.stderr.resume();
    child.on("error", reject);
    child.on("close", code => {
      clearTimeout(timer);
      if (code !== 0) return reject(new Error("function failed"));
      try { resolve(JSON.parse(stdout)); }
      catch { reject(new Error("function returned an invalid result")); }
    });
    child.stdin.end(JSON.stringify(payload));
  });
}

async function callCapability(projectId, request) {
  const response = await fetch(capabilityUrl, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify({ project_id: projectId, request }),
    signal: AbortSignal.timeout(capabilityTimeoutMs),
  });
  const body = await response.json();
  if (!response.ok) throw new Error(body?.error?.message || body?.error || "capability failed");
  return body;
}

async function invoke(payload) {
  const capabilityResults = [];
  for (let count = 0; count <= maxCapabilities; count++) {
    const output = await runWorker({ ...payload, capability_results: capabilityResults });
    if (!output.capability_request) return output;
    if (!payload.project_id) throw new Error("missing capability project");
    if (output.capability_request.index !== capabilityResults.length) {
      throw new Error("invalid capability sequence");
    }
    try {
      capabilityResults.push({ ok: true, value: await callCapability(payload.project_id, output.capability_request) });
    } catch (error) {
      capabilityResults.push({ ok: false, error: String(error.message || error) });
    }
  }
  throw new Error("too many capability calls");
}

const server = http.createServer((req, res) => {
  if (req.method === "GET" && req.url === "/health") {
    return reply(res, 200, { status: "ok" });
  }
  if (req.method !== "POST" || req.url !== "/invoke") {
    return reply(res, 404, { error: "not found" });
  }
  if (!token || req.headers.authorization !== `Bearer ${token}`) {
    return reply(res, 401, { error: "unauthorized" });
  }
  if (!String(req.headers["content-type"] || "").startsWith("application/json")) {
    return reply(res, 415, { error: "application/json required" });
  }
  let size = 0;
  const chunks = [];
  req.on("data", chunk => {
    size += chunk.length;
    if (size > maxBody) req.destroy();
    else chunks.push(chunk);
  });
  req.on("end", async () => {
    try {
      const payload = JSON.parse(Buffer.concat(chunks).toString("utf8"));
      if (typeof payload.source !== "string" || Buffer.byteLength(payload.source) > 524288) {
        return reply(res, 400, { error: "invalid or oversized function source" });
      }
      const result = await invoke(payload);
      reply(res, 200, result);
    } catch (error) {
      reply(res, 422, { error: String(error.message || error) });
    }
  });
});

server.listen(port, addr);
