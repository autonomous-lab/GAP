//! Exhaustive HTTP route tests: every route documented in
//! `docs/node-api.md` is exercised over a real HTTP server (worker
//! pool, like main.rs), with positive and negative cases.

use gap::server::{route_with_ip, NodeState};
use gap::storage::sqlite::SqliteStorage;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Request, Response, Server};

/// Start a real node server (worker pool of 4) on an ephemeral port.
/// Returns the base URL. `token_cap`/`ip_cap` configure rate limits.
fn spawn_node(token_cap: u32, ip_cap: u32) -> String {
    let state = Arc::new(Mutex::new(NodeState::with_rate_limits(
        Box::new(SqliteStorage::open(":memory:").unwrap()),
        None,
        token_cap,
        ip_cap,
    )));
    let server = Arc::new(Server::http("127.0.0.1:0").expect("bind"));
    let base = format!(
        "http://127.0.0.1:{}",
        server.server_addr().to_ip().unwrap().port()
    );
    for _ in 0..4 {
        let state = state.clone();
        let server = server.clone();
        std::thread::spawn(move || loop {
            let mut request: Request = match server.recv() {
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
                route_with_ip(&state, &method, &url, &body, auth.as_deref(), client_ip.as_deref());
            let _ = request.respond(
                Response::from_string(json_body.to_string())
                    .with_status_code(status)
                    .with_header(Header::from_bytes("Content-Type", "application/json").unwrap()),
            );
        });
    }
    base
}

struct Client {
    agent: ureq::Agent,
    base: String,
}

impl Client {
    fn new(base: &str) -> Self {
        let config = ureq::config::Config::builder().http_status_as_error(false).build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            base: base.to_string(),
        }
    }

    fn post(&self, path: &str, token: Option<&str>, body: Value) -> (u16, Value) {
        let mut req = self.agent.post(&format!("{}{}", self.base, path));
        if let Some(t) = token {
            req = req.header("Authorization", &format!("Bearer {t}"));
        }
        let resp = req.send(body.to_string());
        self.read(resp)
    }

    fn post_empty(&self, path: &str, token: Option<&str>) -> (u16, Value) {
        let mut req = self.agent.post(&format!("{}{}", self.base, path));
        if let Some(t) = token {
            req = req.header("Authorization", &format!("Bearer {t}"));
        }
        let resp = req.send_empty();
        self.read(resp)
    }

    fn get(&self, path: &str, token: Option<&str>) -> (u16, Value) {
        let mut req = self.agent.get(&format!("{}{}", self.base, path));
        if let Some(t) = token {
            req = req.header("Authorization", &format!("Bearer {t}"));
        }
        let resp = req.call();
        self.read(resp)
    }

    fn read(&self, resp: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> (u16, Value) {
        match resp {
            Ok(r) => {
                let status = r.status().as_u16();
                let body: Value = serde_json::from_reader(r.into_body().as_reader()).unwrap_or(Value::Null);
                (status, body)
            }
            Err(e) => panic!("transport error: {e:?}"),
        }
    }

    fn identity(&self) -> (String, String) {
        let (s, v) = self.post_empty("/v1/identity", None);
        assert_eq!(s, 200, "identity failed: {v}");
        (
            v["did"].as_str().unwrap().to_string(),
            v["token"].as_str().unwrap().to_string(),
        )
    }
}

fn terms_json() -> Value {
    json!({
        "input": {}, "deliverable": {}, "acceptance_criteria": ["ok"],
        "deadline": 4_000_000_000u64,
        "price": { "amount": 0.05, "currency": "EUR", "model": "fixed", "cap": 100.0 },
        "autonomy": "propose", "confidentiality": null
    })
}

/// A full contract lifecycle via HTTP, asserting every route along the
/// way (positive cases).
#[test]
fn all_routes_full_lifecycle() {
    let base = spawn_node(100_000, 100_000);
    let c = Client::new(&base);

    // 1. GET /health
    let (s, v) = c.get("/health", None);
    assert_eq!(s, 200, "health: {v}");
    assert_eq!(v["status"], "ok");

    // 2. GET /.well-known/gap-agent.json
    let (s, v) = c.get("/.well-known/gap-agent.json", None);
    assert_eq!(s, 200, "agent card: {v}");
    assert!(v["agent"]["did"].as_str().is_some(), "card missing did: {v}");

    // 3. POST /v1/identity (client + provider)
    let (_client_did, client_tok) = c.identity();
    let (provider_did, provider_tok) = c.identity();

    // 4. POST /v1/announce (provider capability)
    let announce = json!({ "capabilities": [{
        "id": "cap:p:bench", "name": "bench", "description": "d",
        "input": {}, "output": {},
        "price": { "amount": 0.05, "currency": "EUR", "model": "fixed" },
        "autonomy": ["propose"]
    }] });
    let (s, v) = c.post("/v1/announce", Some(&provider_tok), announce.clone());
    assert_eq!(s, 200, "announce: {v}");
    assert!(v["announcement_id"].as_str().is_some());

    // 5. GET /v1/discover
    let (s, v) = c.get("/v1/discover", None);
    assert_eq!(s, 200, "discover: {v}");
    assert!(
        v["results"].as_array().is_some() && !v["results"].as_array().unwrap().is_empty(),
        "discover missing results: {v}"
    );

    // 6. POST /v1/contract/propose
    let propose = json!({
        "provider": provider_did, "capability_id": "cap:p:bench",
        "terms": terms_json(), "escrow": true
    });
    let (s, v) = c.post("/v1/contract/propose", Some(&client_tok), propose.clone());
    assert_eq!(s, 200, "propose: {v}");
    let contract_id = v["contract_id"].as_str().unwrap().to_string();
    assert_eq!(v["state"], "draft");

    // 7. GET /v1/contract/{id}
    let (s, v) = c.get(&format!("/v1/contract/{contract_id}"), Some(&client_tok));
    assert_eq!(s, 200, "contract get: {v}");
    assert_eq!(v["state"], "draft");

    // 8. POST /v1/contract/{id}/accept (provider)
    let (s, v) = c.post_empty(&format!("/v1/contract/{contract_id}/accept"), Some(&provider_tok));
    assert_eq!(s, 200, "accept: {v}");
    assert_eq!(v["state"], "signed");

    // 9. POST /v1/escrow/park (client, exact decimal string)
    let (s, v) = c.post(
        "/v1/escrow/park",
        Some(&client_tok),
        json!({ "contract_id": contract_id, "amount": "0.05" }),
    );
    assert_eq!(s, 200, "park: {v}");

    // 10. POST /v1/contract/{id}/deliver (provider)
    let (s, v) = c.post(
        &format!("/v1/contract/{contract_id}/deliver"),
        Some(&provider_tok),
        json!({ "deliverable_hash": "sha256:abc123" }),
    );
    assert_eq!(s, 200, "deliver: {v}");
    assert_eq!(v["state"], "delivered");

    // 11. POST /v1/contract/{id}/accept-delivery (client) -> release
    let (s, v) = c.post_empty(&format!("/v1/contract/{contract_id}/accept-delivery"), Some(&client_tok));
    assert_eq!(s, 200, "accept-delivery: {v}");
    assert_eq!(v["state"], "accepted");
    assert_eq!(v["settlement"]["amount"], 0.05);

    // 12. POST /v1/escrow/release (explicit confirmation)
    let (s, v) = c.post(
        "/v1/escrow/release",
        Some(&client_tok),
        json!({ "contract_id": contract_id }),
    );
    assert_eq!(s, 200, "release: {v}");
    assert_eq!(v["state"], "released");

    // 13. GET /v1/audit — the spine saw everything.
    let (s, v) = c.get("/v1/audit", Some(&client_tok));
    assert_eq!(s, 200, "audit: {v}");
    let events = v["events"].as_array().expect("audit events");
    let kinds: Vec<&str> = events.iter().filter_map(|e| e["kind"].as_str()).collect();
    for expected in ["ctr.proposed", "ctr.signed", "pay.parked", "exe.delivered", "exe.accepted"] {
        assert!(kinds.contains(&expected), "audit missing {expected}: {kinds:?}");
    }

    // 14. Second contract: refund path.
    let (s, v) = c.post("/v1/contract/propose", Some(&client_tok), propose.clone());
    assert_eq!(s, 200);
    let c2 = v["contract_id"].as_str().unwrap().to_string();
    let (s, _) = c.post_empty(&format!("/v1/contract/{c2}/accept"), Some(&provider_tok));
    assert_eq!(s, 200);
    let (s, v) = c.post("/v1/escrow/park", Some(&client_tok), json!({ "contract_id": c2, "amount": "1.00" }));
    assert_eq!(s, 200, "park2: {v}");
    let (s, v) = c.post("/v1/escrow/refund", Some(&client_tok), json!({ "contract_id": c2 }));
    assert_eq!(s, 200, "refund: {v}");
    assert_eq!(v["receipt"]["event"], "pay.refunded");

    // 15. Third contract: dispute + arbitration ruling.
    let (s, v) = c.post("/v1/contract/propose", Some(&client_tok), propose);
    assert_eq!(s, 200);
    let c3 = v["contract_id"].as_str().unwrap().to_string();
    let (s, _) = c.post_empty(&format!("/v1/contract/{c3}/accept"), Some(&provider_tok));
    assert_eq!(s, 200);
    let (s, _) = c.post("/v1/escrow/park", Some(&client_tok), json!({ "contract_id": c3, "amount": "2.00" }));
    assert_eq!(s, 200);
    // Client disputes with a reason code.
    let (s, v) = c.post(
        &format!("/v1/contract/{c3}/dispute"),
        Some(&client_tok),
        json!({ "reason": "late" }),
    );
    assert_eq!(s, 200, "dispute: {v}");
    assert_eq!(v["state"], "disputed");
    // Arbitrator (node) rules 40/60.
    let (s, v) = c.post(
        "/v1/escrow/rule",
        None,
        json!({ "contract_id": c3, "split": { "client": 0.4, "provider": 0.6 } }),
    );
    assert_eq!(s, 200, "rule: {v}");
    assert_eq!(v["state"], "ruled");
}

/// Negative cases for every protection layer.
#[test]
fn all_routes_negative_cases() {
    let base = spawn_node(100_000, 100_000);
    let c = Client::new(&base);
    let (_client_did, client_tok) = c.identity();
    let (provider_did, provider_tok) = c.identity();

    // Unknown route -> generic error (L-03/L-04), no path echo.
    let (s, v) = c.get("/v1/totally-unknown", None);
    assert_eq!(s, 400);
    assert!(!v.to_string().contains("/v1/totally-unknown"), "path echoed: {v}");

    // Invalid JSON body.
    let resp = c
        .agent
        .post(&format!("{base}/v1/announce"))
        .header("Authorization", &format!("Bearer {provider_tok}"))
        .send("{not json".to_string());
    let (s, v) = c.read(resp);
    assert_eq!(s, 400, "invalid json: {v}");
    assert_eq!(v["error"]["code"], "invalid_request");

    // Protected route without token.
    let (s, v) = c.post(
        "/v1/contract/propose",
        None,
        json!({ "provider": provider_did, "capability_id": "cap:x", "terms": terms_json(), "escrow": false }),
    );
    assert_eq!(s, 400, "unauth propose: {v}");
    assert_eq!(v["error"]["code"], "unauthorized");

    // Invalid bearer token.
    let (s, v) = c.get("/v1/audit", Some("Bearer gat_doesnotexist"));
    assert_eq!(s, 400, "bad token: {v}");
    assert_eq!(v["error"]["code"], "unauthorized");

    // Unknown provider in propose.
    let (s, v) = c.post(
        "/v1/contract/propose",
        Some(&client_tok),
        json!({ "provider": "did:gap:1111111111111111111111111111111111111111111111111111111111111111", "capability_id": "cap:x", "terms": terms_json(), "escrow": false }),
    );
    assert_eq!(s, 400, "unknown provider: {v}");
    assert_eq!(v["error"]["code"], "contract_not_found");

    // Invalid escrow amount (7 decimals) — exact-money validation.
    let announce = json!({ "capabilities": [{
        "id": "cap:p:bench", "name": "b", "description": "d",
        "input": {}, "output": {},
        "price": { "amount": 0.05, "currency": "EUR", "model": "fixed" },
        "autonomy": ["propose"]
    }] });
    let (s, _) = c.post("/v1/announce", Some(&provider_tok), announce);
    assert_eq!(s, 200);
    let (s, v) = c.post(
        "/v1/contract/propose",
        Some(&client_tok),
        json!({ "provider": provider_did, "capability_id": "cap:p:bench", "terms": terms_json(), "escrow": true }),
    );
    assert_eq!(s, 200);
    let cid = v["contract_id"].as_str().unwrap().to_string();
    let (s, _) = c.post_empty(&format!("/v1/contract/{cid}/accept"), Some(&provider_tok));
    assert_eq!(s, 200);
    let (s, v) = c.post("/v1/escrow/park", Some(&client_tok), json!({ "contract_id": cid, "amount": "1.0000001" }));
    assert_eq!(s, 400, "7-decimal amount must be rejected: {v}");

    // Parking without prior accept -> escrow violation.
    let (s, v) = c.post(
        "/v1/contract/propose",
        Some(&client_tok),
        json!({ "provider": provider_did, "capability_id": "cap:p:bench", "terms": terms_json(), "escrow": true }),
    );
    assert_eq!(s, 200);
    let cid2 = v["contract_id"].as_str().unwrap().to_string();
    let (s, v) = c.post("/v1/escrow/park", Some(&client_tok), json!({ "contract_id": cid2, "amount": "1.00" }));
    assert_eq!(s, 400, "park before accept must fail: {v}");
    assert_eq!(v["error"]["code"], "escrow_violation");

    // Ruling split that does not sum to 1.0.
    let (s, _) = c.post_empty(&format!("/v1/contract/{cid2}/accept"), Some(&provider_tok));
    assert_eq!(s, 200);
    let (s, _) = c.post("/v1/escrow/park", Some(&client_tok), json!({ "contract_id": cid2, "amount": "1.00" }));
    assert_eq!(s, 200);
    let (s, _) = c.post(&format!("/v1/contract/{cid2}/dispute"), Some(&client_tok), json!({ "reason": "late" }));
    assert_eq!(s, 200);
    let (s, v) = c.post(
        "/v1/escrow/rule",
        None,
        json!({ "contract_id": cid2, "split": { "client": 0.4, "provider": 0.5 } }),
    );
    assert_eq!(s, 400, "split != 1.0 must be rejected: {v}");
    assert_eq!(v["error"]["code"], "escrow_violation");
}

/// Rate limiting: 429 after the per-token cap (audit H-03).
#[test]
fn rate_limit_returns_429_over_http() {
    let base = spawn_node(5, 100_000); // 5 req/min per token
    let c = Client::new(&base);
    let (_, tok) = c.identity();
    let mut saw_429 = false;
    for _ in 0..10 {
        let (s, v) = c.get("/v1/audit", Some(&tok));
        if s == 429 {
            assert_eq!(v["error"]["code"], "rate_limited");
            saw_429 = true;
            break;
        }
    }
    assert!(saw_429, "expected a 429 after the per-token cap");
}

/// Worker pool concurrency: parallel requests all succeed.
#[test]
fn worker_pool_handles_concurrent_requests() {
    let base = spawn_node(100_000, 100_000);
    let c = Client::new(&base);
    let (_, tok) = c.identity();
    let mut handles = Vec::new();
    for _ in 0..32 {
        let base = base.clone();
        let tok = tok.clone();
        handles.push(std::thread::spawn(move || {
            let c = Client::new(&base);
            for _ in 0..10 {
                let (s, _) = c.get("/health", Some(&tok));
                assert_eq!(s, 200);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
}
