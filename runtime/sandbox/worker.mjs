import vm from "node:vm";

let input = "";
for await (const chunk of process.stdin) input += chunk;
const payload = JSON.parse(input);
const vmTimeoutMs = Math.min(Math.max(Number(process.argv[2]) || 30000, 1), 30000);

const request = Object.freeze(payload.request || {});
const results = Array.isArray(payload.capability_results) ? payload.capability_results : [];
let capabilityIndex = 0;
let signalCapability;
const capabilitySignal = new Promise(resolve => { signalCapability = resolve; });
function capability(kind, args) {
  const index = capabilityIndex++;
  if (index < results.length) {
    const previous = results[index];
    if (previous && previous.ok) return Promise.resolve(previous.value);
    return Promise.reject(new Error(previous?.error || "capability failed"));
  }
  // Suspend without throwing. A user function is allowed to catch ordinary
  // capability failures, but it must not be able to catch GAP's internal
  // replay signal and accidentally return it as its own result.
  signalCapability({ __gap_capability: true, index, kind, args });
  return new Promise(() => {});
}
const gap = Object.freeze({
  kv: Object.freeze({
    get: key => capability("kv.get", { key }),
    put: (key, value, options = {}) => capability("kv.put", { key, value, ...options }),
  }),
  objects: Object.freeze({
    get: key => capability("objects.get", { key }),
    put: (key, content, mediaType = "application/octet-stream") =>
      capability("objects.put", { key, content, media_type: mediaType }),
  }),
  db: Object.freeze({
    query: (sql, params = []) => capability("db.query", { sql, params }),
    execute: (sql, params = []) => capability("db.execute", { sql, params }),
  }),
  http: Object.freeze({
    get: (url, options = {}) => capability("http.request", { method: "GET", url, ...options }),
    post: (url, options = {}) => capability("http.request", { method: "POST", url, ...options }),
  }),
  realtime: Object.freeze({
    issueToken: (options = {}) => capability("realtime.token", options),
  }),
});
const context = vm.createContext(Object.create(null), {
  codeGeneration: { strings: false, wasm: false },
});
context.request = request;
context.gap = gap;

const script = new vm.Script(
  `(async () => { "use strict"; const handler = (${payload.source}); return await handler(request, gap); })()`,
  { filename: "gap-function.js" },
);
const execution = Promise.resolve(script.runInContext(context, { timeout: vmTimeoutMs }))
  .then(result => ({ result }));
const outcome = await Promise.race([
  execution,
  capabilitySignal.then(capability_request => ({ capability_request })),
]);
process.stdout.write(JSON.stringify(outcome));
