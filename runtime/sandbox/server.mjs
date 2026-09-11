import http from "node:http";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const addr = process.env.SANDBOX_ADDR || "0.0.0.0";
const port = Number(process.env.SANDBOX_PORT || "8090");
const token = process.env.SANDBOX_TOKEN || "";
const maxBody = Number(process.env.SANDBOX_MAX_BODY_BYTES || "600000");
const timeoutMs = Number(process.env.SANDBOX_TIMEOUT_MS || "30000");
const vmTimeoutMs = Number(process.env.SANDBOX_VM_TIMEOUT_MS || "30000");
const capabilityTimeoutMs = Number(process.env.SANDBOX_CAPABILITY_TIMEOUT_MS || "35000");
const capabilityUrl = process.env.CAPABILITY_URL || "http://gap-node:8080/internal/functions/capability";
const maxCapabilities = Number(process.env.SANDBOX_MAX_CAPABILITIES || "128");
const maxHttpCapabilities = Number(process.env.SANDBOX_MAX_HTTP_CAPABILITIES || "32");
const maxGlobalConcurrency = Number(process.env.SANDBOX_MAX_GLOBAL_CONCURRENCY || "16");
const maxProjectConcurrency = Number(process.env.SANDBOX_MAX_PROJECT_CONCURRENCY || "4");
const maxQueue = Number(process.env.SANDBOX_MAX_QUEUE || "32");
const queueTimeoutMs = Number(process.env.SANDBOX_QUEUE_TIMEOUT_MS || "30000");
const workerPath = fileURLToPath(new URL("./worker.mjs", import.meta.url));

if (!token) throw new Error("SANDBOX_TOKEN is required");

class SandboxBusyError extends Error {
  constructor() {
    super("sandbox is busy");
    this.code = "sandbox_busy";
  }
}

let activeGlobal = 0;
const activeByProject = new Map();
const queue = [];

function hasCapacity(projectId) {
  return activeGlobal < maxGlobalConcurrency
    && (activeByProject.get(projectId) || 0) < maxProjectConcurrency;
}

function takeSlot(projectId) {
  activeGlobal += 1;
  activeByProject.set(projectId, (activeByProject.get(projectId) || 0) + 1);
}

function drainQueue() {
  for (let index = 0; index < queue.length && activeGlobal < maxGlobalConcurrency;) {
    const pending = queue[index];
    if (!hasCapacity(pending.projectId)) {
      index += 1;
      continue;
    }
    queue.splice(index, 1);
    clearTimeout(pending.timer);
    takeSlot(pending.projectId);
    pending.resolve();
  }
}

function acquireSlot(projectId) {
  if (hasCapacity(projectId)) {
    takeSlot(projectId);
    return Promise.resolve();
  }
  if (queue.length >= maxQueue) return Promise.reject(new SandboxBusyError());
  return new Promise((resolve, reject) => {
    const pending = { projectId, resolve, reject, timer: undefined };
    pending.timer = setTimeout(() => {
      const index = queue.indexOf(pending);
      if (index !== -1) queue.splice(index, 1);
      reject(new SandboxBusyError());
    }, queueTimeoutMs);
    queue.push(pending);
  });
}

function releaseSlot(projectId) {
  activeGlobal -= 1;
  const remaining = (activeByProject.get(projectId) || 1) - 1;
  if (remaining === 0) activeByProject.delete(projectId);
  else activeByProject.set(projectId, remaining);
  drainQueue();
}

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

function remainingTime(deadline) {
  const remaining = Math.floor(deadline - performance.now());
  if (remaining <= 0) throw new Error("function timed out");
  return remaining;
}

const invocations = new Set(), policyGenerations = new Map();
async function readPolicies(projects) {
  try {
    const response = await fetch(new URL("/internal/workload-policy", capabilityUrl), {
      method: "POST", headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: JSON.stringify({ project_ids: projects }), signal: AbortSignal.timeout(3000)
    });
    if (!response.ok) return {};
    const values = (await response.json()).policies || {};
    for (const id of projects) {
      const p=values[id], known=policyGenerations.get(id)||0;
      if (!p || !Number.isSafeInteger(p.generation) || p.generation<known) values[id]={allowed:false};
      else policyGenerations.set(id,p.generation);
    }
    return values;
  } catch { return {}; }
}
function cancelInvocation(context) {
  context.cancelled=true;
  for (const child of context.children) child.kill("SIGKILL");
  for (const controller of context.controllers) controller.abort();
}
function applyPolicy(context,policy) {
  if (context.cancelled || policy?.allowed!==true || policy.generation<(policyGenerations.get(context.project)||0)) {
    cancelInvocation(context);return false;
  }
  context.expires=performance.now()+5000;return true;
}
async function ensurePolicy(context) {
  if (context.cancelled) throw new Error("project suspended or policy unavailable");
  if (performance.now()>=context.expires && !applyPolicy(context,(await readPolicies([context.project]))[context.project])) {
    throw new Error("project suspended or policy unavailable");
  }
}
let policyRefresh=false;
setInterval(async()=>{
  if (policyRefresh || !invocations.size) return;
  policyRefresh=true;
  try {
    const contexts=[...invocations], projects=[...new Set(contexts.map(c=>c.project))];
    const policies=await readPolicies(projects);
    for (const context of contexts) applyPolicy(context,policies[context.project]);
  } finally {policyRefresh=false;}
},1000).unref();
setInterval(()=>{
  for (const context of invocations) if (performance.now()>=context.expires) cancelInvocation(context);
},250).unref();

function runWorker(payload, deadline, context) {
  const remaining = remainingTime(deadline);
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [workerPath, String(Math.min(vmTimeoutMs, remaining))], {
      env: {},
      stdio: ["pipe", "pipe", "pipe"],
    });
    context.children.add(child);
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill("SIGKILL");
    }, remaining);
    let stdout = "";
    child.stdout.on("data", chunk => {
      stdout += chunk;
      if (stdout.length > maxBody) child.kill("SIGKILL");
    });
    child.stderr.resume();
    child.on("error", reject);
    child.on("close", code => {
      clearTimeout(timer);context.children.delete(child);
      if (context.cancelled) return reject(new Error("project suspended or policy unavailable"));
      if (timedOut) return reject(new Error("function timed out"));
      if (code !== 0) return reject(new Error("function failed"));
      try { resolve(JSON.parse(stdout)); }
      catch { reject(new Error("function returned an invalid result")); }
    });
    child.stdin.end(JSON.stringify(payload));
  });
}

async function callCapability(projectId, request, deadline, context) {
  const controller=new AbortController();context.controllers.add(controller);
  try {
  const response = await fetch(capabilityUrl, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify({ project_id: projectId, request }),
    signal: AbortSignal.any([controller.signal,AbortSignal.timeout(Math.min(capabilityTimeoutMs, remainingTime(deadline)))]),
  });
  const body = await response.json();
  if (!response.ok) throw new Error(body?.error?.message || body?.error || "capability failed");
  return body;
  } finally {context.controllers.delete(controller);}
}

async function invoke(payload) {
  const context={project:payload.project_id,children:new Set(),controllers:new Set(),cancelled:false,expires:0};
  await ensurePolicy(context);invocations.add(context);
  try { return await invokeAdmitted(payload,context); } finally {invocations.delete(context);cancelInvocation(context);}
}

async function invokeAdmitted(payload,context) {
  const deadline = performance.now() + timeoutMs;
  const capabilityResults = [];
  let httpCalls = 0;
  for (;;) {
    await ensurePolicy(context);
    const output = await runWorker({ ...payload, capability_results: capabilityResults }, deadline,context);
    remainingTime(deadline);
    if (!output.capability_request) return output;
    if (!payload.project_id) throw new Error("missing capability project");
    if (output.capability_request.index !== capabilityResults.length) {
      throw new Error("invalid capability sequence");
    }
    // Check before dispatch: the final replay is permitted, but the next
    // operation must never execute a side effect beyond either budget.
    if (capabilityResults.length >= maxCapabilities) {
      throw new Error(`too many capability calls: maximum ${maxCapabilities} per invocation`);
    }
    if (output.capability_request.kind === "http.request") {
      if (httpCalls >= maxHttpCapabilities) {
        throw new Error(`too many HTTP capability calls: maximum ${maxHttpCapabilities} per invocation`);
      }
      httpCalls++;
    }
    try {
      await ensurePolicy(context);
      capabilityResults.push({ ok: true, value: await callCapability(payload.project_id, output.capability_request, deadline,context) });
    } catch (error) {
      capabilityResults.push({ ok: false, error: String(error.message || error) });
    }
  }
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
      if (typeof payload.project_id !== "string" || !payload.project_id) {
        return reply(res, 400, { error: "missing project id" });
      }
      await acquireSlot(payload.project_id);
      try {
        const result = await invoke(payload);
        reply(res, 200, result);
      } finally {
        releaseSlot(payload.project_id);
      }
    } catch (error) {
      if (error?.code === "sandbox_busy") {
        reply(res, 429, { error: { code: "sandbox_busy", message: "sandbox is busy" } });
      } else {
        reply(res, 422, { error: String(error.message || error) });
      }
    }
  });
});

server.listen(port, addr);
