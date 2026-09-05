import vm from "node:vm";

let input = "";
for await (const chunk of process.stdin) input += chunk;
const payload = JSON.parse(input);

const request = Object.freeze(payload.request || {});
const results = Array.isArray(payload.capability_results) ? payload.capability_results : [];
let capabilityIndex = 0;
function capability(kind, args) {
  const index = capabilityIndex++;
  if (index < results.length) {
    const previous = results[index];
    if (previous && previous.ok) return Promise.resolve(previous.value);
    return Promise.reject(new Error(previous?.error || "capability failed"));
  }
  throw { __gap_capability: true, index, kind, args };
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
try {
  const result = await script.runInContext(context, { timeout: 250 });
  process.stdout.write(JSON.stringify({ result }));
} catch (error) {
  if (error && error.__gap_capability === true) {
    process.stdout.write(JSON.stringify({ capability_request: error }));
  } else {
    throw error;
  }
}
