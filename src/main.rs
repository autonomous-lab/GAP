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
use std::io::Read;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Response, Server};

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
            let transport = UreqTransport::new(&url);
            let storage = ClickHouseStorage::new(transport);
            if env::var("GAP_DB_INIT").as_deref() == Ok("1") {
                storage.migrate()?;
                println!("[gap-node] clickhouse schema migrated");
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

    // Audit M-02: warn loudly when the node is exposed without TLS.
    let exposed = addr.starts_with("0.0.0.0") || addr.starts_with("::");
    if exposed {
        println!(
            "[gap-node] ⚠ SECURITY: node bound to {addr} without TLS. \
             In production, terminate TLS at the load balancer / reverse \
             proxy — bearer tokens and contract data travel in cleartext."
        );
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
            // Read the body (limit to a sane size).
            let mut body = Vec::new();
            request
                .as_reader()
                .take(1024 * 1024)
                .read_to_end(&mut body)
                .ok();

            let method = request.method().as_str().to_string();
            let path = request.url().to_string();
            let auth = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .map(|h| h.value.as_str().to_string());

            let client_ip = request.remote_addr().map(|addr| addr.ip().to_string());
            let (status, json_body) = route_with_ip(
                &state,
                &method,
                &path,
                &body,
                auth.as_deref(),
                client_ip.as_deref(),
            );
            let json_str = json_body.to_string();

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
