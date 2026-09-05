import vm from "node:vm";

let input = "";
for await (const chunk of process.stdin) input += chunk;
const payload = JSON.parse(input);

const request = Object.freeze(payload.request || {});
const gap = Object.freeze({
  // Capabilities will be added explicitly. The sandbox never receives a
  // database path, filesystem handle, bearer token or process environment.
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
const result = await script.runInContext(context, { timeout: 250 });
process.stdout.write(JSON.stringify({ result }));
