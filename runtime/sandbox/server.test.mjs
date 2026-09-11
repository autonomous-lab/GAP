import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { spawn } from 'node:child_process';
import { once } from 'node:events';

async function fixture(overrides = {}) {
  let calls = 0;
  const backend = http.createServer(async (req, res) => {
    const chunks=[];for await (const chunk of req) chunks.push(chunk);
    if (req.url==='/internal/workload-policy') {
      const body=JSON.parse(Buffer.concat(chunks).toString());
      if(overrides.policyFail){res.writeHead(503);res.end('{}');return}
      const allowed=!overrides.suspended;
      res.writeHead(200,{'content-type':'application/json'});
      res.end(JSON.stringify({policies:Object.fromEntries(body.project_ids.map(id=>[id,{allowed,generation:overrides.generation??(allowed?0:1)}]))}));return;
    }
    calls++;
    if (overrides.delay) await new Promise(r => setTimeout(r, overrides.delay));
    res.writeHead(overrides.fail ? 400 : 200, { 'content-type': 'application/json' });
    res.end(JSON.stringify(overrides.fail ? { error: 'test failure' } : { ok: true }));
  });
  backend.listen(0, '127.0.0.1');
  await once(backend, 'listening');
  const reservation = http.createServer();
  reservation.listen(0, '127.0.0.1');
  await once(reservation, 'listening');
  const port = reservation.address().port;
  await new Promise(r => reservation.close(r));
  const child = spawn(process.execPath, [new URL('./server.mjs', import.meta.url).pathname], {
    env: { ...process.env, SANDBOX_TOKEN: 'test-only', SANDBOX_ADDR: '127.0.0.1',
      SANDBOX_PORT: String(port), CAPABILITY_URL: `http://127.0.0.1:${backend.address().port}`,
      SANDBOX_TIMEOUT_MS: String(overrides.timeout || 30000),
      SANDBOX_VM_TIMEOUT_MS: '30000', SANDBOX_CAPABILITY_TIMEOUT_MS: '35000',
      SANDBOX_MAX_CAPABILITIES: '128', SANDBOX_MAX_HTTP_CAPABILITIES: '32',
      SANDBOX_MAX_GLOBAL_CONCURRENCY:String(overrides.concurrency||16) },
    stdio: 'ignore',
  });
  const close = async () => {
    const exited = once(child, 'exit'); child.kill(); await exited;
    backend.closeAllConnections(); await new Promise(r => backend.close(r));
  };
  try {
    let ready = false;
    for (let i = 0; i < 100; i++) {
      try { ready = (await fetch(`http://127.0.0.1:${port}/health`)).ok; } catch {}
      if (ready) break;
      await new Promise(r => setTimeout(r, 20));
    }
    assert.ok(ready, 'sandbox ready');
    return { close, calls: () => calls, async invoke(source) {
      const response = await fetch(`http://127.0.0.1:${port}/invoke`, {
        method: 'POST', headers: { authorization: 'Bearer test-only', 'content-type': 'application/json' },
        body: JSON.stringify({ project_id: 'test', source }), signal: AbortSignal.timeout(40000),
      });
      return { status: response.status, body: await response.json() };
    } };
  } catch (error) { await close(); throw error; }
}

test('128 storage calls succeed, 129th never dispatches, counters reset', async () => {
  const f = await fixture();
  try {
    const source = n => `async (_, gap) => { for(let i=0;i<${n};i++) await gap.kv.get('x'); return 42; }`;
    assert.deepEqual(await f.invoke(source(128)), { status: 200, body: { result: 42 } });
    assert.equal(f.calls(), 128);
    const overflow = await f.invoke(source(129));
    assert.equal(overflow.status, 422);
    assert.match(overflow.body.error, /maximum 128/);
    assert.equal(f.calls(), 256);
  } finally { await f.close(); }
});

test('HTTP has an independent 32-call cap, failed HTTP calls count', async () => {
  for (const fail of [false, true]) {
    const f = await fixture({ fail });
    try {
      const result = await f.invoke(`async (_, gap) => {
        for(let i=0;i<40;i++) { try { await gap.kv.get('x'); } catch {} }
        for(let i=0;i<33;i++) { try { await gap.http.get('https://example.test'); } catch {} }
        return 42;
      }`);
      assert.equal(result.status, 422);
      assert.match(result.body.error, /HTTP capability calls: maximum 32/);
      assert.equal(f.calls(), 72);
    } finally { await f.close(); }
  }
});

test('deadline spans all replays and capability waits', async () => {
  const f = await fixture({ timeout: 500, delay: 150 });
  try {
    const started = performance.now();
    const result = await f.invoke(`async (_, gap) => { for(let i=0;i<10;i++) await gap.kv.get('x'); return 42; }`);
    assert.equal(result.status, 422);
    assert.match(result.body.error, /timed out/);
    assert.ok(performance.now() - started < 2000);
    assert.ok(f.calls() < 10);
    assert.equal((await f.invoke('() => 42')).status, 200);
  } finally { await f.close(); }
});


test('suspension kills an active worker, denies queued execution and fails closed on policy outage', {timeout:15000}, async()=>{
  const settings={concurrency:1};const f=await fixture(settings);
  try {
    const active=f.invoke('() => { while (true) {} }');
    await new Promise(r=>setTimeout(r,150));
    const queued=f.invoke("async (_,gap) => { await gap.kv.get('must-not-run');return 42; }");
    await new Promise(r=>setTimeout(r,100));settings.suspended=true;settings.generation=1;
    const [a,q]=await Promise.all([active,queued]);
    assert.equal(a.status,422);assert.match(a.body.error,/suspended|policy/);
    assert.equal(q.status,422);assert.equal(f.calls(),0);
    settings.suspended=false;settings.generation=2;
    assert.deepEqual(await f.invoke('() => 42'),{status:200,body:{result:42}});
    const duringOutage=f.invoke('() => { while (true) {} }');
    await new Promise(r=>setTimeout(r,150));settings.policyFail=true;
    const denied=await duringOutage;assert.equal(denied.status,422);assert.match(denied.body.error,/suspended|policy/);
  } finally {await f.close()}
});
