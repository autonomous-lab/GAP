import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";

async function waitForHealth(port) {
  for (let attempt = 0; attempt < 100; attempt++) {
    try { if ((await fetch(`http://127.0.0.1:${port}/health`)).ok) return; } catch {}
    await new Promise(resolve => setTimeout(resolve, 20));
  }
  throw new Error("realtime did not become healthy");
}

test("plaintext messages migrate and a wrong storage secret is rejected", async () => {
  const root = await mkdtemp(join(tmpdir(), "gap-realtime-encryption-"));
  const path = join(root, "realtime.sqlite");
  const seed = new DatabaseSync(path);
  seed.exec(`CREATE TABLE messages(seq INTEGER PRIMARY KEY AUTOINCREMENT,project_id TEXT NOT NULL,
    channel TEXT NOT NULL,body TEXT NOT NULL,size_bytes INTEGER NOT NULL,created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL);`);
  seed.prepare("INSERT INTO messages(project_id,channel,body,size_bytes,created_at,expires_at) VALUES(?,?,?,?,?,?)")
    .run("prj_test", "channel", '{"secret":"preserved"}', 22, 100, 9999999999);
  seed.close();
  const reservation = await import("node:net").then(({ createServer }) => createServer());
  reservation.listen(0, "127.0.0.1"); await once(reservation, "listening");
  const port = reservation.address().port; await new Promise(resolve => reservation.close(resolve));
  const script = new URL("./server.mjs", import.meta.url).pathname;
  const env = { ...process.env, REALTIME_PORT: String(port), REALTIME_SECRET: "stable-test-secret", REALTIME_DB: path };
  const child = spawn(process.execPath, [script], { env, stdio: "ignore" });
  await waitForHealth(port); child.kill(); await once(child, "exit");
  const encrypted = new DatabaseSync(path, { readOnly: true });
  const body = encrypted.prepare("SELECT body FROM messages").get().body;
  encrypted.close();
  assert.match(body, /^enc:v1:/);
  assert(!body.includes("preserved"));

  const wrong = spawn(process.execPath, [script], { env: { ...env, REALTIME_SECRET: "wrong-test-secret" }, stdio: "ignore" });
  const [code] = await once(wrong, "exit");
  assert.notEqual(code, 0);
  await rm(root, { recursive: true, force: true });
});
