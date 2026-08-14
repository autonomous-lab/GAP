//! GAP node — the HTTP server binary.
//!
//! Run:
//!   GAP_STORAGE=sqlite    gap-node           (default, SQLite file)
//!   GAP_STORAGE=clickhouse gap-node          (needs GAP_CLICKHOUSE_URL)
//!
//! Environment:
//!   GAP_ADDR             listen address, default 0.0.0.0:8080
//!   GAP_STORAGE          "sqlite" | "clickhouse"
//!   GAP_SQLITE_PATH      SQLite file, default ./gap-node.db
//!   GAP_CLICKHOUSE_URL   ClickHouse HTTP URL, e.g. http://clickhouse:8123
//!   GAP_DB_INIT          "1" to run ClickHouse migrations at startup
//!   GAP_ADMIN_TOKEN      bearer token required for node arbitration

use gap::error::Result;
use gap::server::{route_with_ip, NodeState};
use gap::storage::clickhouse::{ClickHouseStorage, UreqTransport};
use gap::storage::sqlite::SqliteStorage;
use gap::storage::Storage;
use std::env;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Response, Server};

/// Stream events as Server-Sent Events (RFC-0013 §3.3).
///
/// Resumable: the client passes `?after=<seq>` (or `Last-Event-ID`) and
/// receives every later in-scope event with `id:` set to the sequence,
/// so a reconnect leaves no gap. Keepalive comments stop proxies from
/// reaping the connection.
///
/// This holds a worker thread for the life of the connection —
/// deliberate, and bounded by `GAP_SSE_MAX_SECS` (default 300 s), after
/// which the client reconnects with its cursor. Webhooks are the
/// zero-connection path for agents that can receive inbound HTTP.
fn stream_events(
    state: &Arc<Mutex<NodeState>>,
    request: tiny_http::Request,
    path: &str,
    auth: Option<&str>,
) {
    let token = auth.and_then(|a| a.strip_prefix("Bearer ").map(|t| t.to_string()));
    let token = match token {
        Some(t) => t,
        None => {
            let _ = request.respond(
                Response::from_string(r#"{"error":"unauthorized"}"#).with_status_code(401),
            );
            return;
        }
    };
    // Cursor: ?after=<seq>, or the Last-Event-ID header on reconnect.
    let mut cursor: u64 = path
        .split_once('?')
        .map(|(_, q)| q)
        .and_then(|q| {
            q.split('&')
                .find_map(|kv| kv.strip_prefix("after=").and_then(|v| v.parse().ok()))
        })
        .or_else(|| {
            request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Last-Event-ID"))
                .and_then(|h| h.value.as_str().parse().ok())
        })
        .unwrap_or(0);

    // Authenticate before opening the stream.
    if state
        .lock()
        .ok()
        .map(|g| g.events_for(&token, cursor, 1).is_err())
        .unwrap_or(true)
    {
        let _ = request
            .respond(Response::from_string(r#"{"error":"unauthorized"}"#).with_status_code(401));
        return;
    }

    let max_secs: u64 = env::var("GAP_SSE_MAX_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let started = std::time::Instant::now();

    let mut writer = request.into_writer();
    let preamble = "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache\r\n\
         Connection: keep-alive\r\n\
         X-Accel-Buffering: no\r\n\r\n";
    if writer.write_all(preamble.as_bytes()).is_err() {
        return;
    }
    let _ = writer.flush();

    loop {
        let batch = match state.lock() {
            Ok(guard) => guard.events_for(&token, cursor, 200).unwrap_or_default(),
            Err(_) => break,
        };
        let mut wrote = false;
        for event in &batch {
            let seq = event.get("seq").and_then(|v| v.as_u64()).unwrap_or(cursor);
            let kind = event
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("event");
            let frame = format!("id: {seq}\nevent: {kind}\ndata: {event}\n\n");
            if writer.write_all(frame.as_bytes()).is_err() {
                return; // client hung up
            }
            cursor = seq;
            wrote = true;
        }
        if wrote && writer.flush().is_err() {
            return;
        }
        if started.elapsed().as_secs() >= max_secs {
            // Ask the client to come back with its cursor.
            let _ = writer.write_all(b": reconnect\n\n");
            let _ = writer.flush();
            return;
        }
        if batch.is_empty() {
            // Keepalive comment: proxies reap silent connections.
            if writer.write_all(b": keepalive\n\n").is_err() || writer.flush().is_err() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(700));
        }
    }
}

/// Stream public activity as SSE (`/v1/activity/stream?after=<seq>`).
///
/// Two named event types share one connection:
///   * `lifecycle` — every published step of a deal, from proposal to
///     payout, keyed on the spine sequence;
///   * `settlement` — the closing row, with the verdict and the money.
///
/// A named SSE event does NOT fire `onmessage`; a browser must call
/// `addEventListener` for each name. That is a trap worth stating here
/// because it is invisible from the server side: the node happily
/// writes frames nobody is listening for.
///
/// Unauthenticated on purpose: every field is pseudonymous. Resumable
/// on the same cursor as the rest of the protocol, so a reconnecting
/// browser never misses a settlement.
fn stream_activity(state: &Arc<Mutex<NodeState>>, request: tiny_http::Request, path: &str) {
    let mut cursor: u64 = path
        .split_once('?')
        .map(|(_, q)| q)
        .and_then(|q| {
            q.split('&')
                .find_map(|kv| kv.strip_prefix("after=").and_then(|v| v.parse().ok()))
        })
        .or_else(|| {
            request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Last-Event-ID"))
                .and_then(|h| h.value.as_str().parse().ok())
        })
        .unwrap_or(0);

    let max_secs: u64 = env::var("GAP_SSE_MAX_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let started = std::time::Instant::now();
    let mut writer = request.into_writer();
    let preamble = "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache\r\n\
         Connection: keep-alive\r\n\
         Access-Control-Allow-Origin: *\r\n\
         X-Accel-Buffering: no\r\n\r\n";
    if writer.write_all(preamble.as_bytes()).is_err() {
        return;
    }
    let _ = writer.flush();

    // Two cursors over one connection. A settlement's sequence is the
    // spine height at the moment the job was recorded, so the two live
    // in the same numeric space but do not advance in lockstep: sharing
    // one cursor would let a lifecycle event drag it past a settlement
    // that had not been emitted yet. The client resumes from the LOWER
    // of the two and drops what it has already drawn.
    let mut spine_cursor = cursor;

    loop {
        let (life, batch) = match state.lock() {
            Ok(guard) => (
                guard.public_lifecycle_after(spine_cursor, 200),
                guard.public_activity_after(cursor, 100),
            ),
            Err(_) => return,
        };

        // Lifecycle first: these are the steps that lead up to a
        // settlement, so emitting them after it would play the story
        // backwards on a catch-up replay.
        let events = life["events"].as_array().cloned().unwrap_or_default();
        // The spine head travels with each frame so a page can keep its
        // "audit spine events" counter true. Derived here rather than
        // counted by the browser: most spine kinds are internal and are
        // never published, so a client tallying the frames it can see
        // would drift low for ever.
        let head = life["scanned_to"].as_u64().unwrap_or(spine_cursor);
        for ev in &events {
            let seq = ev["seq"].as_u64().unwrap_or(spine_cursor);
            let mut ev = ev.clone();
            if let Some(obj) = ev.as_object_mut() {
                obj.insert("spine".into(), serde_json::json!(head));
            }
            let frame = format!("id: {seq}\nevent: lifecycle\ndata: {ev}\n\n");
            if writer.write_all(frame.as_bytes()).is_err() {
                return;
            }
        }
        // Advance past everything LOOKED AT, not everything published:
        // most spine kinds are internal, and a cursor that only moved
        // past published rows would rescan the same tail every pass.
        spine_cursor = life["scanned_to"].as_u64().unwrap_or(spine_cursor).max(spine_cursor);

        let jobs = batch["jobs"].as_array().cloned().unwrap_or_default();
        for job in &jobs {
            let seq = job["seq"].as_u64().unwrap_or(cursor);
            let frame = format!("id: {seq}\nevent: settlement\ndata: {job}\n\n");
            if writer.write_all(frame.as_bytes()).is_err() {
                return;
            }
            cursor = seq.max(cursor);
        }
        let idle = jobs.is_empty() && events.is_empty();
        if !idle && writer.flush().is_err() {
            return;
        }
        if started.elapsed().as_secs() >= max_secs {
            let _ = writer.write_all(b": reconnect\n\n");
            let _ = writer.flush();
            return;
        }
        if idle {
            if writer.write_all(b": keepalive\n\n").is_err() || writer.flush().is_err() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(900));
        }
    }
}

fn build_storage() -> Result<Box<dyn Storage>> {
    let kind = env::var("GAP_STORAGE").unwrap_or_else(|_| "sqlite".into());
    match kind.as_str() {
        "sqlite" => {
            let path = env::var("GAP_SQLITE_PATH").unwrap_or_else(|_| "./gap-node.db".into());
            println!("[gap-node] storage: sqlite ({path})");
            Ok(Box::new(SqliteStorage::open(&path)?))
        }
        "clickhouse" => {
            let url =
                env::var("GAP_CLICKHOUSE_URL").unwrap_or_else(|_| "http://clickhouse:8123".into());
            println!("[gap-node] storage: clickhouse ({url})");
            let transport = UreqTransport::from_env(&url);
            let storage = ClickHouseStorage::new(transport);
            if env::var("GAP_DB_INIT").as_deref() == Ok("1") {
                storage.migrate()?;
                println!("[gap-node] clickhouse schema migrated");
            }
            // Read the cluster back into the in-memory mirrors that
            // every read goes through. Skipping this made ClickHouse a
            // write-only sink: durable, and never consulted again.
            match storage.hydrate() {
                Ok(h) => println!(
                    "[gap-node] clickhouse hydrated: {} event(s), {} identity(ies), \
{} announcement(s), {} contract(s), {} escrow(s), {} artifact(s), {} state entr(ies)",
                    h.events,
                    h.identities,
                    h.announcements,
                    h.contracts,
                    h.escrows,
                    h.deliverables,
                    h.state
                ),
                // A node that cannot read its own history should say so
                // loudly and still start: refusing to boot would turn a
                // degraded cluster into an outage.
                Err(e) => eprintln!("[gap-node] WARNING: clickhouse hydrate failed: {e}"),
            }
            Ok(Box::new(storage))
        }
        other => Err(gap::Error::Other(format!(
            "unknown GAP_STORAGE: {other} (use sqlite or clickhouse)"
        ))),
    }
}

fn main() -> Result<()> {
    let addr = env::var("GAP_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let storage = build_storage()?;

    // Node identity persistence (audit fix H-01): load the seed from
    // GAP_NODE_SEED (hex) or GAP_NODE_SEED_FILE. Without it, the node
    // DID changes on every restart.
    let seed: Option<[u8; 32]> = {
        let hex_seed = env::var("GAP_NODE_SEED").ok().or_else(|| {
            env::var("GAP_NODE_SEED_FILE")
                .ok()
                .and_then(|f| std::fs::read_to_string(f).ok())
                .map(|s| s.trim().to_string())
        });
        match hex_seed {
            Some(hex_str) => {
                let bytes = hex::decode(hex_str.trim())
                    .map_err(|_| gap::Error::Other("GAP_NODE_SEED must be 64 hex chars".into()))?;
                let arr: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| gap::Error::Other("GAP_NODE_SEED must be 32 bytes".into()))?;
                Some(arr)
            }
            None => None,
        }
    };
    let token_cap: u32 = env::var("GAP_RATE_TOKEN_CAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let ip_cap: u32 = env::var("GAP_RATE_IP_CAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600);
    let mut state = NodeState::with_rate_limits(storage, seed, token_cap, ip_cap);
    if let Ok(admin_token) = env::var("GAP_ADMIN_TOKEN") {
        state.set_admin_token(admin_token);
        println!("[gap-node] arbitration admin token configured");
    } else {
        println!("[gap-node] arbitration disabled: set GAP_ADMIN_TOKEN to enable /v1/escrow/rule");
    }

    // Read access to a chain, so deposits can be verified. Independent
    // of the relayer: checking a receipt needs no keys and no escrow
    // contract, and without it the on-chain deposit rail is dead code
    // that answers "no chain connection configured" to every caller.
    if let Ok(rpc_url) = env::var("GAP_RPC_URL") {
        let reader = gap::relayer::JsonRpcChain::new(&rpc_url, 1);
        state.set_deposit_chain(Box::new(reader));
        println!("[gap-node] deposit verification: reading {rpc_url}");
    } else {
        println!(
            "[gap-node] deposit verification: no GAP_RPC_URL, on-chain deposits cannot be checked"
        );
    }

    // Optional on-chain escrow: when GAP_ESCROW_ADDRESS is set, escrow
    // operations go to the GapEscrow contract via the relayer.
    if let Ok(escrow_address) = env::var("GAP_ESCROW_ADDRESS") {
        let rpc_url = env::var("GAP_RPC_URL").unwrap_or_else(|_| "http://localhost:8545".into());
        let chain = gap::relayer::JsonRpcChain::new(&rpc_url, 1);
        state.set_relayer(Box::new(chain), &escrow_address);
        println!("[gap-node] escrow: on-chain via {escrow_address} ({rpc_url})");
    } else {
        println!("[gap-node] escrow: off-chain (reference implementation)");
    }
    let state = Arc::new(Mutex::new(state));

    let server = Arc::new(
        Server::http(&addr)
            .map_err(|e| gap::Error::Other(format!("failed to bind {addr}: {e}")))?,
    );
    println!("[gap-node] listening on http://{addr}");
    println!("[gap-node] node DID: {}", state.lock().unwrap().node_did());
    println!("[gap-node] agent card: http://{addr}/.well-known/gap-agent.json");

    // Worker pool: request parsing and response serialization run in
    // parallel; the state lock serializes only the protocol core
    // (event-sourcing requires one order, so writes stay ordered).
    let workers: usize = env::var("GAP_WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(8)
        });
    println!("[gap-node] worker pool: {workers} threads");

    // Maximum request body: 5 MB, enough to carry a modest artifact
    // inline (a base64 image, say). Above it the answer is not a bigger
    // buffer - it is to host the file and deliver its URL, so the node
    // never becomes a file server it was not designed to be.
    let max_body: u64 = env::var("GAP_MAX_BODY_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5 * 1024 * 1024);
    println!("[gap-node] max request body: {max_body} bytes");

    // Audit M-02: warn loudly when the node is exposed without TLS.
    let exposed = addr.starts_with("0.0.0.0") || addr.starts_with("::");
    if exposed {
        println!(
            "[gap-node] ⚠ SECURITY: node bound to {addr} without TLS. \
             In production, terminate TLS at the load balancer / reverse \
             proxy — bearer tokens and contract data travel in cleartext."
        );
    }

    // Event delivery (RFC-0013): one background thread drains the
    // outbox. Network I/O happens with the state lock released, so a
    // slow subscriber cannot stall contract processing.
    {
        let state = state.clone();
        std::thread::spawn(move || {
            let sender = gap::delivery::UreqSender::default();
            loop {
                let sent = gap::server::drain_outbox(&state, &sender);
                // Idle: back off; busy: keep draining.
                std::thread::sleep(std::time::Duration::from_millis(if sent == 0 {
                    500
                } else {
                    50
                }));
            }
        });
        println!("[gap-node] event delivery: webhooks enabled (RFC-0013)");
    }

    // Resolve contracts that stopped moving.
    //
    // Without this a deal nobody funded, or nobody answered, stays open
    // for ever: escrow parked, the provider unpaid, the public feed
    // advertising as live something abandoned days ago. A day of silence
    // is the node's answer, and it resolves in the direction the work
    // points - cancel and refund what was never delivered, accept and
    // pay what was. It runs every fifteen minutes rather than on a
    // timer per contract, so a restart cannot lose a pending expiry:
    // the rule is re-evaluated from state each pass, not scheduled.
    //
    // GAP_EXPIRE_AFTER_SECS tunes the window; 0 turns the sweep off.
    {
        let window: u64 = env::var("GAP_EXPIRE_AFTER_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(gap::EXPIRE_AFTER_SECS);
        if window == 0 {
            println!("[gap-node] contract expiry: DISABLED (GAP_EXPIRE_AFTER_SECS=0)");
        } else {
            let state = state.clone();
            std::thread::spawn(move || loop {
                let outcome = match state.lock() {
                    Ok(mut guard) => {
                        let now = gap::message::now_unix();
                        Some(guard.sweep_expired(now, window, window, true, false))
                    }
                    Err(_) => None,
                };
                if let Some(o) = outcome {
                    let cancelled = o["cancelled"].as_u64().unwrap_or(0);
                    let accepted = o["auto_accepted"].as_u64().unwrap_or(0);
                    if cancelled > 0 || accepted > 0 {
                        println!(
                            "[gap-node] expiry sweep: {cancelled} cancelled, \
                             {accepted} auto-accepted and paid"
                        );
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(900));
            });
            println!("[gap-node] contract expiry: stalled deals resolved after {window}s");
        }
    }

    let mut handles = Vec::new();
    for _ in 0..workers {
        let server = server.clone();
        let state = state.clone();
        handles.push(std::thread::spawn(move || loop {
            let mut request = match server.recv() {
                Ok(r) => r,
                Err(_) => break,
            };
            // Read the body, bounded.
            //
            // This used to `.take(LIMIT)` and carry on, which does not
            // reject an oversized body - it TRUNCATES it. The caller
            // then got a JSON parse error for a payload that was
            // perfectly well formed when it left, and no hint that size
            // was the problem. An agent hit exactly that delivering an
            // image and spent its time shrinking the PNG by trial and
            // error, because the node's answer pointed at the wrong
            // thing entirely.
            //
            // Read one byte past the limit: if it arrives, the body was
            // too big, and we say so with the status that means it.
            let mut body = Vec::new();
            request
                .as_reader()
                .take(max_body + 1)
                .read_to_end(&mut body)
                .ok();
            if body.len() as u64 > max_body {
                let msg = serde_json::json!({
                    "error": "payload too large",
                    "limit_bytes": max_body,
                    "hint": "host the artifact yourself and deliver a URL instead of the bytes: \
POST /v1/contract/{id}/deliver with {\"deliverable_hash\":\"sha256:...\",\
\"deliverable_uri\":\"https://...\"}. The digest still governs - whatever the client \
retrieves from that URL must hash to it.",
                })
                .to_string();
                let response = Response::from_string(msg)
                    .with_status_code(413)
                    .with_header(
                        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
                    );
                let _ = request.respond(response);
                continue;
            }

            // HEAD is GET without the body (RFC 9110 section 9.3.2).
            //
            // The node answered 400 to every HEAD, on every path,
            // including /health. Uptime monitors send HEAD by default,
            // so did link checkers and preview bots - which meant every
            // automated check reported the site DOWN while a browser
            // loaded it perfectly. Reproduced straight against the node,
            // with Cloudflare out of the picture.
            let head_only = request.method().as_str() == "HEAD";
            let method = if head_only {
                "GET".to_string()
            } else {
                request.method().as_str().to_string()
            };
            let path = request.url().to_string();
            let auth = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .map(|h| h.value.as_str().to_string());

            // SSE stream (RFC-0013 §3.3): this endpoint owns the
            // connection, so it cannot go through `route()` (which
            // returns a single JSON body). Accept-header negotiated:
            // clients that just want the cursor get the JSON form.
            // Public, unauthenticated activity stream for the web UI:
            // the projection is already pseudonymous, so it needs no
            // credentials — and a live feed nobody can watch without a
            // token would not be a public directory.
            // A stream is the one thing HEAD must not open: it would
            // hold a worker on a connection whose caller wants headers
            // and nothing else. It still gets a truthful answer - the
            // status and content type a GET would produce - rather than
            // the 400 that made every checker call this endpoint dead.
            if path.starts_with("/v1/activity/stream") {
                if head_only {
                    let mut response = Response::from_string(String::new()).with_status_code(200);
                    response.add_header(
                        Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..])
                            .unwrap(),
                    );
                    let _ = request.respond(response);
                } else {
                    stream_activity(&state, request, &path);
                }
                continue;
            }

            let wants_sse = path.starts_with("/v1/events")
                && !head_only
                && request.headers().iter().any(|h| {
                    h.field.equiv("Accept") && h.value.as_str().contains("text/event-stream")
                });
            if wants_sse {
                stream_events(&state, request, &path, auth.as_deref());
                continue;
            }

            // Static binary assets first: the Open Graph card is fetched
            // by crawlers that never send an Accept header we could
            // route on.
            if method == "GET" {
                let clean = path.split('?').next().unwrap_or(&path);
                if let Some((ctype, bytes)) = gap::server::static_asset(clean) {
                    let body: &[u8] = if head_only { &[] } else { bytes };
                    let mut response = Response::from_data(body).with_status_code(200);
                    response.add_header(
                        Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()).unwrap(),
                    );
                    // Immutable for a build: the bytes only change when
                    // the binary does.
                    response.add_header(
                        Header::from_bytes(&b"Cache-Control"[..], &b"public, max-age=86400"[..])
                            .unwrap(),
                    );
                    let _ = request.respond(response);
                    continue;
                }
            }

            // Web UI (public directory + operator console) before the
            // JSON API: browsers ask for HTML, agents ask for JSON.
            if let Some((status, ctype, body)) =
                gap::server::route_html(&state, &method, &path, auth.as_deref())
            {
                let body = if head_only { String::new() } else { body };
                let mut response = Response::from_string(body).with_status_code(status);
                response.add_header(
                    Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()).unwrap(),
                );
                let _ = request.respond(response);
                continue;
            }

            let client_ip = request.remote_addr().map(|addr| addr.ip().to_string());
            let (status, json_body) = route_with_ip(
                &state,
                &method,
                &path,
                &body,
                auth.as_deref(),
                client_ip.as_deref(),
            );
            let json_str = if head_only {
                String::new()
            } else {
                json_body.to_string()
            };

            let mut response = Response::from_string(json_str).with_status_code(status);
            response.add_header(
                Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
            );

            let _ = request.respond(response);
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}
