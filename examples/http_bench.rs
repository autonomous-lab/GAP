//! HTTP throughput benchmark for the GAP node (raw capacity).
//!
//! Starts the node in-process on an ephemeral port with the security
//! rate caps lifted (the bench measures the node's raw capacity; the
//! operator-facing caps are configurable via GAP_RATE_*_CAP).
//!
//! Endpoints measured: GET /health (no auth), GET /v1/audit (auth +
//! storage read), POST /v1/identity (auth + Ed25519 keygen).
//!
//! Run:  cargo run --release --example http_bench [seconds_per_run]

use gap::server::{route_with_ip, NodeState};
use gap::storage::sqlite::SqliteStorage;
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_http::Response;

const DEFAULT_SECONDS: u64 = 5;

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(DEFAULT_SECONDS);

    // Node with lifted rate caps — we measure capacity, not policy.
    let state = Arc::new(Mutex::new(NodeState::with_rate_limits(
        Box::new(SqliteStorage::open(":memory:").unwrap()),
        None,
        10_000_000,
        10_000_000,
    )));

    let base = std::env::var("GAP_BENCH_TARGET").unwrap_or_else(|_| {
        // Bind on an ephemeral port.
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
        let port = server.server_addr().to_ip().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        // Serve in the background (worker pool, like main.rs).
        let workers: usize = std::env::var("GAP_BENCH_WORKERS")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(8);
        let server = std::sync::Arc::new(server);
        for _ in 0..workers {
            let state2 = state.clone();
            let server = server.clone();
            std::thread::spawn(move || loop {
                let mut request: tiny_http::Request = match server.recv() {
                    Ok(r) => r,
                    Err(_) => break,
                };
                let method = request.method().as_str().to_string();
                let url = request.url().to_string();
                let auth = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Authorization"))
                    .map(|h| h.value.as_str().to_string());
                let mut body = Vec::new();
                let _ = request.as_reader().read_to_end(&mut body);
                let client_ip = request.remote_addr().map(|a| a.ip().to_string());
                let (status, json_body) =
                    route_with_ip(&state2, &method, &url, &body, auth.as_deref(), client_ip.as_deref());
                let payload = json_body.to_string();
                let _ = request.respond(
                    Response::from_string(payload)
                        .with_status_code(status)
                        .with_header(tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap()),
                );
            });
        }
        base
    });


    // Warm up: create identities (client + provider), announce the
    // provider's capability, then benchmark propose flows.
    let agent = ureq::Agent::new_with_defaults();
    let client = post_identity(&agent, &base);
    let provider = post_identity(&agent, &base);
    let token = client.2.clone();
    // Announce a capability so propose can reference it.
    let announce = json!({ "capabilities": [{
        "id": "cap:p:bench",
        "name": "bench",
        "description": "benchmark capability",
        "input": {},
        "output": {},
        "price": { "amount": 0.05, "currency": "EUR", "model": "fixed" },
        "autonomy": ["propose"]
    }] });
    let resp = agent
        .post(&format!("{base}/v1/announce"))
        .header("Authorization", &format!("Bearer {}", provider.2))
        .send(announce.to_string())
        .expect("announce");
    assert!(resp.status() == 200, "announce failed: {}", resp.status());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let propose_body = json!({
        "provider": provider.1,
        "capability_id": "cap:p:bench",
        "terms": {
            "input": {},
            "deliverable": {},
            "acceptance_criteria": ["ok"],
            "deadline": now + 3600,
            "price": { "amount": 0.05, "currency": "EUR", "model": "fixed", "cap": 100.0 },
            "autonomy": "propose",
            "confidentiality": null
        },
        "escrow": false
    }).to_string();

    println!("GAP node HTTP benchmark — {seconds}s per run, node on {base}");
    println!("CPU: {}/{} cores, platform: {}", std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0), num_cpus(), os_name());
    println!();

    let mut table = String::from("| concurrency | endpoint | req/s | p50 (ms) | p99 (ms) | errors |\n|---|---|---|---|---|---|\n");

    for concurrency in [1usize, 4, 8, 16] {
        for (label, endpoint, body) in [
            ("GET /health", format!("{base}/health"), None),
            ("GET /v1/audit", format!("{base}/v1/audit"), None),
            ("POST /v1/identity", format!("{base}/v1/identity"), None),
            ("POST /v1/contract/propose", format!("{base}/v1/contract/propose"), Some(propose_body.clone())),
        ] {
            let authed = label.starts_with("GET /v1") || label.starts_with("POST");
            let (rps, p50, p99, errors) =
                run_phase(&endpoint, concurrency, seconds, authed, &token, body.as_deref());
            println!("c={concurrency:2}  {label:<24} {rps:>9.0} req/s  p50={p50:>6.2}ms  p99={p99:>7.2}ms  errors={errors}");
            table.push_str(&format!(
                "| {concurrency} | {label} | {rps:.0} | {p50:.2} | {p99:.2} | {errors} |\n"
            ));
        }
    }

    std::fs::write("/tmp/gap-http-bench.md", &table).ok();
    println!("\nMarkdown table saved to /tmp/gap-http-bench.md");
}

fn post_identity(agent: &ureq::Agent, base: &str) -> (serde_json::Value, String, String) {
    let resp = agent
        .post(&format!("{base}/v1/identity"))
        .send_empty()
        .expect("identity");
    let body: serde_json::Value =
        serde_json::from_reader(resp.into_body().as_reader()).expect("json");
    (
        body.clone(),
        body["did"].as_str().unwrap().to_string(),
        body["token"].as_str().unwrap().to_string(),
    )
}

fn num_cpus() -> usize {
    std::fs::read_to_string("/proc/cpuinfo")
        .map(|s| s.matches("processor").count())
        .unwrap_or(1)
}

fn os_name() -> &'static str {
    std::env::consts::OS
}

/// Hammer one endpoint with `concurrency` workers for `seconds`.
/// Returns (req/s, p50 ms, p99 ms, error count).
fn run_phase(
    url: &str,
    concurrency: usize,
    seconds: u64,
    authed: bool,
    token: &str,
    body: Option<&str>,
) -> (f64, f64, f64, u64) {
    let done = Arc::new(AtomicU64::new(0));
    let errs = Arc::new(AtomicU64::new(0));
    let latencies: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    let start = Instant::now();
    let deadline = Duration::from_secs(seconds);

    let mut workers = Vec::new();
    for _ in 0..concurrency {
        let done = done.clone();
        let errs = errs.clone();
        let latencies = latencies.clone();
        let url = url.to_string();
        let token = token.to_string();
        let body = body.map(|b| b.to_string());
        workers.push(std::thread::spawn(move || {
            let agent = ureq::Agent::new_with_defaults();
            let is_post = url.contains("/v1/identity") || url.contains("/v1/contract/propose");
            while start.elapsed() < deadline {
                let t0 = Instant::now();
                let result = if let Some(b) = &body {
                    let mut req = agent.post(&url);
                    if authed {
                        req = req.header("Authorization", &format!("Bearer {token}"));
                    }
                    req.send(b.clone())
                } else if is_post {
                    if authed {
                        agent
                            .post(&url)
                            .header("Authorization", &format!("Bearer {token}"))
                            .send_empty()
                    } else {
                        agent.post(&url).send_empty()
                    }
                } else if authed {
                    agent
                        .get(&url)
                        .header("Authorization", &format!("Bearer {token}"))
                        .call()
                } else {
                    agent.get(&url).call()
                };
                let elapsed = t0.elapsed().as_secs_f64() * 1000.0; // ms
                match result {
                    Ok(_) => {
                        done.fetch_add(1, Ordering::Relaxed);
                        latencies.lock().unwrap().push(elapsed);
                    }
                    Err(_) => {
                        errs.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    for w in workers {
        w.join().unwrap();
    }
    let elapsed = start.elapsed().as_secs_f64();
    let total = done.load(Ordering::Relaxed);
    let mut lat = latencies.lock().unwrap().clone();
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = percentile(&lat, 0.50);
    let p99 = percentile(&lat, 0.99);
    (total as f64 / elapsed, p50, p99, errs.load(Ordering::Relaxed))
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx]
}
