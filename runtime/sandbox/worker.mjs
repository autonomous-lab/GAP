import vm from "node:vm";
import { randomFillSync, randomUUID } from "node:crypto";

let input = "";
for await (const chunk of process.stdin) input += chunk;
const payload = JSON.parse(input);
const vmTimeoutMs = Math.min(Math.max(Number(process.argv[2]) || 30000, 1), 30000);
const results = Array.isArray(payload.capability_results) ? payload.capability_results : [];
let capabilityIndex = 0;
let signalCapability;
const capabilitySignal = new Promise(resolve => { signalCapability = resolve; });
const timers = new Map();
const context = vm.createContext(Object.create(null), {
  codeGeneration: { strings: false, wasm: false },
});

// Only primitive JSON strings cross the realm boundary. Never expose a host
// object, Promise or ordinary host function to customer code.
const bridge = Object.setPrototypeOf((operation, inputJson) => {
  try {
    if (typeof operation !== "string" || typeof inputJson !== "string") {
      return '{"ok":false,"error":"invalid bridge input"}';
    }
    const args = JSON.parse(inputJson);
    if (operation === "capability") {
      const [kind, request] = args;
      const index = capabilityIndex++;
      if (index < results.length) {
        const previous = results[index];
        return JSON.stringify(previous?.ok
          ? { ok: true, value: previous.value }
          : { ok: false, error: previous?.error || "capability failed" });
      }
      signalCapability({ __gap_capability: true, index, kind, args: request });
      return '{"pending":true}';
    }
    if (operation === "atob") return JSON.stringify({ ok: true, value: atob(String(args)) });
    if (operation === "btoa") return JSON.stringify({ ok: true, value: btoa(String(args)) });
    if (operation === "encode") return JSON.stringify({ ok: true, value: Array.from(new TextEncoder().encode(String(args))) });
    if (operation === "decode") return JSON.stringify({ ok: true, value: new TextDecoder(args[1] || "utf-8", args[2] || {}).decode(Uint8Array.from(args[0])) });
    if (operation === "url") {
      const parsed = args[1] === null ? new URL(args[0]) : new URL(args[0], args[1]);
      return JSON.stringify({ ok: true, value: {
        href: parsed.href, origin: parsed.origin, protocol: parsed.protocol,
        username: parsed.username, password: parsed.password, host: parsed.host,
        hostname: parsed.hostname, port: parsed.port, pathname: parsed.pathname,
        search: parsed.search, hash: parsed.hash,
      } });
    }
    if (operation === "params") {
      const [action, raw, key, value] = args;
      const params = new URLSearchParams(raw);
      if (action === "get") return JSON.stringify({ ok: true, value: params.get(key) });
      if (action === "getAll") return JSON.stringify({ ok: true, value: params.getAll(key) });
      if (action === "has") return JSON.stringify({ ok: true, value: params.has(key) });
      if (action === "entries") return JSON.stringify({ ok: true, value: Array.from(params.entries()) });
      if (action === "append") params.append(key, value);
      if (action === "set") params.set(key, value);
      if (action === "delete") params.delete(key);
      return JSON.stringify({ ok: true, value: params.toString() });
    }
    if (operation === "uuid") return JSON.stringify({ ok: true, value: randomUUID() });
    if (operation === "random") {
      if (!Number.isSafeInteger(args) || args < 0 || args > 65536) throw new Error("invalid random byte length");
      return JSON.stringify({ ok: true, value: Array.from(randomFillSync(Buffer.alloc(args))) });
    }
    if (operation === "timer") {
      const [id, delay] = args;
      if (!Number.isSafeInteger(id) || id < 1 || timers.size >= 128) throw new Error("timer limit exceeded");
      const handle = setTimeout(() => {
        timers.delete(id);
        try { vm.runInContext(`globalThis.__gapFireTimer(${id})`, context, { timeout: vmTimeoutMs }); }
        catch { process.exitCode = 1; }
      }, Math.min(Math.max(Number(delay) || 0, 0), 30000));
      timers.set(id, handle);
      return '{"ok":true}';
    }
    if (operation === "clearTimer") {
      const handle = timers.get(args);
      if (handle) clearTimeout(handle);
      timers.delete(args);
      return '{"ok":true}';
    }
    return '{"ok":false,"error":"unsupported operation"}';
  } catch (error) {
    return JSON.stringify({ ok: false, error: String(error.message || error) });
  }
}, null);
context.__gapBridge = bridge;
context.__gapRequestJson = JSON.stringify(payload.request || {});

new vm.Script(`
  const callBridge = (operation, value) => {
    const answer = JSON.parse(__gapBridge(operation, JSON.stringify(value)));
    if (operation !== "capability" && !answer.ok) throw new TypeError(answer.error);
    return answer;
  };
  const capability = (kind, args) => {
    const answer = callBridge("capability", [kind, args]);
    if (answer.pending) return new Promise(() => {});
    return answer.ok ? Promise.resolve(answer.value) : Promise.reject(new Error(answer.error));
  };
  globalThis.request = JSON.parse(__gapRequestJson);
  globalThis.gap = Object.freeze({
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
    realtime: Object.freeze({ issueToken: (options = {}) => capability("realtime.token", options) }),
  });
  globalThis.atob = value => callBridge("atob", value).value;
  globalThis.btoa = value => callBridge("btoa", value).value;
  globalThis.TextEncoder = class TextEncoder {
    get encoding() { return "utf-8"; }
    encode(value = "") { return Uint8Array.from(callBridge("encode", value).value); }
  };
  globalThis.TextDecoder = class TextDecoder {
    constructor(label = "utf-8", options = {}) { this.label = label; this.options = options; }
    decode(value = new Uint8Array()) { return callBridge("decode", [Array.from(value), this.label, this.options]).value; }
    get encoding() { return this.label; }
  };
  globalThis.URL = class URL {
    constructor(value, base) { Object.assign(this, callBridge("url", [String(value), base === undefined ? null : String(base)]).value); }
    toString() { return this.href; }
    toJSON() { return this.href; }
  };
  globalThis.URLSearchParams = class URLSearchParams {
    constructor(value = "") { this.value = String(value).replace(/^\\?/, ""); }
    append(key, value) { this.value = callBridge("params", ["append", this.value, String(key), String(value)]).value; }
    set(key, value) { this.value = callBridge("params", ["set", this.value, String(key), String(value)]).value; }
    delete(key) { this.value = callBridge("params", ["delete", this.value, String(key)]).value; }
    get(key) { return callBridge("params", ["get", this.value, String(key)]).value; }
    getAll(key) { return callBridge("params", ["getAll", this.value, String(key)]).value; }
    has(key) { return callBridge("params", ["has", this.value, String(key)]).value; }
    entries() { return callBridge("params", ["entries", this.value]).value[Symbol.iterator](); }
    toString() { return this.value; }
    [Symbol.iterator]() { return this.entries(); }
  };
  globalThis.crypto = Object.freeze({
    randomUUID: () => callBridge("uuid", null).value,
    getRandomValues: array => {
      if (!ArrayBuffer.isView(array) || !(array instanceof Uint8Array)) throw new TypeError("Uint8Array required");
      array.set(callBridge("random", array.length).value);
      return array;
    },
  });
  const callbacks = new Map();
  let nextTimerId = 1;
  globalThis.setTimeout = (callback, delay = 0, ...args) => {
    if (typeof callback !== "function") throw new TypeError("callback must be a function");
    const id = nextTimerId++;
    callbacks.set(id, () => callback(...args));
    try { callBridge("timer", [id, delay]); } catch (error) { callbacks.delete(id); throw error; }
    return id;
  };
  globalThis.clearTimeout = id => { callbacks.delete(id); callBridge("clearTimer", id); };
  globalThis.__gapFireTimer = id => {
    const callback = callbacks.get(id);
    callbacks.delete(id);
    if (callback) callback();
  };
  globalThis.queueMicrotask = callback => Promise.resolve().then(callback);
`, { filename: "gap-compat.js" }).runInContext(context, { timeout: vmTimeoutMs });

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
for (const handle of timers.values()) clearTimeout(handle);
process.stdout.write(JSON.stringify(outcome));
