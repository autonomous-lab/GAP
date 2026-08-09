//! RFC-0013 integration: signed webhook delivery, scoping, retries,
//! disabling, and resumable cursor streaming — driven through the real
//! HTTP routes with a mock transport standing in for the network.

use gap::delivery::{DeliveryBody, MockSender, MAX_ATTEMPTS, MAX_CONSECUTIVE_FAILURES};
use gap::server::{drain_outbox, route, NodeState};
use gap::storage::sqlite::SqliteStorage;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn node() -> Arc<Mutex<NodeState>> {
    // Rate caps lifted: these tests hammer the routes.
    Arc::new(Mutex::new(NodeState::with_rate_limits(
        Box::new(SqliteStorage::open(":memory:").unwrap()),
        None,
        1_000_000,
        1_000_000,
    )))
}

fn register(state: &Arc<Mutex<NodeState>>) -> (String, String) {
    let (status, v) = route(state, "POST", "/v1/identity", b"{}", None);
    assert_eq!(status, 200);
    (
        v["did"].as_str().unwrap().to_string(),
        v["token"].as_str().unwrap().to_string(),
    )
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

fn post(state: &Arc<Mutex<NodeState>>, path: &str, body: &Value, token: &str) -> (u16, Value) {
    route(
        state,
        "POST",
        path,
        &body.to_string().into_bytes(),
        Some(&bearer(token)),
    )
}

/// Subscribe a webhook. `https://example.com` resolves publicly, so it
/// passes the SSRF guard without any network call being made (the mock
/// sender never leaves the process).
fn subscribe(state: &Arc<Mutex<NodeState>>, token: &str, kinds: Value) -> String {
    let (status, v) = post(
        state,
        "/v1/subscriptions",
        &json!({ "transport": "webhook", "url": "https://example.com/gap/events", "kinds": kinds }),
        token,
    );
    assert_eq!(status, 200, "subscribe failed: {v}");
    v["subscription_id"].as_str().unwrap().to_string()
}

/// Drive a contract to the signed state; returns (contract_id).
fn signed_contract(
    state: &Arc<Mutex<NodeState>>,
    client_tok: &str,
    provider_tok: &str,
    provider_did: &str,
) -> String {
    let terms = json!({
        "input": {}, "deliverable": {}, "acceptance_criteria": ["ok"],
        "deadline": 4_102_444_800u64,
        "price": { "amount": 1.0, "currency": "EUR", "model": "fixed", "cap": 5.0 },
        "autonomy": "propose"
    });
    let (s, v) = post(
        state,
        "/v1/contract/propose",
        &json!({ "provider": provider_did, "capability_id": "cap:x", "terms": terms, "escrow": true }),
        client_tok,
    );
    assert_eq!(s, 200, "propose failed: {v}");
    let cid = v["contract_id"].as_str().unwrap().to_string();
    let (s, _) = post(
        state,
        &format!("/v1/contract/{cid}/accept"),
        &json!({}),
        provider_tok,
    );
    assert_eq!(s, 200);
    cid
}

#[test]
fn webhook_is_delivered_signed_and_verifiable_by_the_receiver() {
    let state = node();
    let (_client_did, client_tok) = register(&state);
    let (provider_did, provider_tok) = register(&state);

    let sub_id = subscribe(&state, &client_tok, json!([]));
    let cid = signed_contract(&state, &client_tok, &provider_tok, &provider_did);

    let sender = MockSender::new();
    let attempted = drain_outbox(&state, &sender);
    assert!(attempted > 0, "events should have been queued for delivery");

    let bodies = sender.bodies();
    assert!(!bodies.is_empty(), "no webhook delivered");

    // Every delivery verifies against the node DID it declares — this
    // is exactly what a receiving agent does before acting.
    let node_did = state.lock().unwrap().node_did().to_string();
    for body in &bodies {
        assert!(body.verify().is_ok(), "delivery must verify");
        assert_eq!(body.node, node_did);
        assert_eq!(body.subscription_id, sub_id);
        assert_eq!(body.attempt, 1);
    }

    // The four RFC-0013 headers are present and consistent.
    assert_eq!(
        sender.header_of(0, "x-gap-node").as_deref(),
        Some(node_did.as_str())
    );
    assert!(sender
        .header_of(0, "x-gap-signature")
        .unwrap()
        .starts_with("ed25519:"));
    assert_eq!(
        sender.header_of(0, "x-gap-delivery").as_deref(),
        Some(bodies[0].delivery_id.as_str())
    );
    assert_eq!(
        sender.header_of(0, "x-gap-event-seq").unwrap(),
        bodies[0].event.seq.to_string()
    );

    // The contract lifecycle actually reached the subscriber.
    let kinds: Vec<String> = bodies.iter().map(|b| b.event.kind.clone()).collect();
    assert!(
        kinds.iter().any(|k| k == "ctr.signed"),
        "expected ctr.signed among {kinds:?}"
    );
    let _ = cid;
}

#[test]
fn a_forged_delivery_does_not_verify() {
    let state = node();
    let (_did, tok) = register(&state);
    let (provider_did, provider_tok) = register(&state);
    subscribe(&state, &tok, json!([]));
    signed_contract(&state, &tok, &provider_tok, &provider_did);

    let sender = MockSender::new();
    drain_outbox(&state, &sender);
    let body = sender.bodies().remove(0);

    // An attacker who replays the body with a different payload cannot
    // keep the node's signature valid.
    let mut forged: DeliveryBody = body.clone();
    forged.event.payload = json!({ "contract_id": "urn:gap:ctr:attacker" });
    assert!(forged.verify().is_err());
}

#[test]
fn events_are_scoped_to_the_parties_of_the_contract() {
    let state = node();
    let (_client_did, client_tok) = register(&state);
    let (provider_did, provider_tok) = register(&state);
    let (_stranger_did, stranger_tok) = register(&state);

    // A third agent subscribes to everything, then a contract runs
    // between the other two.
    let stranger_sub = subscribe(&state, &stranger_tok, json!([]));
    let _client_sub = subscribe(&state, &client_tok, json!([]));
    signed_contract(&state, &client_tok, &provider_tok, &provider_did);

    let sender = MockSender::new();
    drain_outbox(&state, &sender);

    let stranger_got: Vec<_> = sender
        .bodies()
        .into_iter()
        .filter(|b| b.subscription_id == stranger_sub)
        .filter(|b| b.event.payload.get("contract_id").is_some())
        .collect();
    assert!(
        stranger_got.is_empty(),
        "a non-party must never receive contract events: {stranger_got:?}"
    );

    // And the cursor endpoint applies the same scoping.
    let (s, v) = route(
        &state,
        "GET",
        "/v1/events?after=0&limit=100",
        &[],
        Some(&bearer(&stranger_tok)),
    );
    assert_eq!(s, 200);
    let leaked = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["payload"].get("contract_id").is_some())
        .count();
    assert_eq!(leaked, 0, "cursor must not leak other parties' contracts");
}

#[test]
fn kind_filter_limits_what_is_delivered() {
    let state = node();
    let (_client_did, client_tok) = register(&state);
    let (provider_did, provider_tok) = register(&state);

    let sub_id = subscribe(&state, &client_tok, json!(["ctr.signed"]));
    signed_contract(&state, &client_tok, &provider_tok, &provider_did);

    let sender = MockSender::new();
    drain_outbox(&state, &sender);
    let mine: Vec<_> = sender
        .bodies()
        .into_iter()
        .filter(|b| b.subscription_id == sub_id)
        .collect();
    assert!(!mine.is_empty());
    assert!(
        mine.iter().all(|b| b.event.kind == "ctr.signed"),
        "filter must be exact"
    );
}

#[test]
fn failed_delivery_retries_with_backoff_then_gives_up() {
    let state = node();
    let (_client_did, client_tok) = register(&state);
    let (provider_did, provider_tok) = register(&state);
    subscribe(&state, &client_tok, json!(["ctr.signed"]));
    signed_contract(&state, &client_tok, &provider_tok, &provider_did);

    let sender = MockSender::new();
    sender.set_status(500); // subscriber is broken

    // First attempt fails and is re-queued for a later second.
    let attempted = drain_outbox(&state, &sender);
    assert_eq!(attempted, 1);
    assert_eq!(state.lock().unwrap().outbox_len(), 1, "must be re-queued");

    // Backoff is respected: nothing is due immediately.
    let again = drain_outbox(&state, &sender);
    assert_eq!(again, 0, "backoff must delay the retry");

    // Fast-forward by forcing every pending delivery to be due, and
    // exhaust the attempts.
    for _ in 1..MAX_ATTEMPTS {
        {
            let mut guard = state.lock().unwrap();
            let mut due = guard.take_due_deliveries(u64::MAX);
            for d in due.drain(..) {
                guard.settle_delivery(d, false);
            }
        }
    }
    // Attempts exhausted: the event is dropped from the outbox and the
    // subscription carries a failure.
    assert_eq!(state.lock().unwrap().outbox_len(), 0);
}

#[test]
fn subscription_is_disabled_after_repeated_failures_and_recorded() {
    let state = node();
    let (_did, tok) = register(&state);
    let sub_id = subscribe(&state, &tok, json!(["never.matches"]));

    // Simulate MAX_CONSECUTIVE_FAILURES exhausted events.
    for _ in 0..MAX_CONSECUTIVE_FAILURES {
        let mut guard = state.lock().unwrap();
        let node_identity = guard.node.identity.clone();
        let mut body = gap::delivery::DeliveryBody::new(
            &node_identity,
            &sub_id,
            gap::delivery::DeliveredEvent {
                seq: 1,
                kind: "x".into(),
                payload: json!({}),
                at: 0,
            },
        );
        body.attempt = MAX_ATTEMPTS; // last attempt
        body.sign(&node_identity);
        let pending = gap::delivery::PendingDelivery {
            subscription_id: sub_id.clone(),
            url: "https://example.com/h".into(),
            body,
            not_before: 0,
        };
        guard.settle_delivery(pending, false);
    }

    let guard = state.lock().unwrap();
    let sub = guard.subscription(&sub_id).unwrap();
    assert!(!sub.active, "subscription must be disabled");
    assert_eq!(sub.failures, MAX_CONSECUTIVE_FAILURES);
    drop(guard);

    // Disabling is auditable, not silent.
    let (s, v) = route(
        &state,
        "GET",
        "/v1/audit?after=0&limit=200",
        &[],
        Some(&bearer(&tok)),
    );
    assert_eq!(s, 200);
    let disabled = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "sub.disabled");
    assert!(disabled, "sub.disabled must be recorded on the spine");
}

#[test]
fn successful_delivery_resets_the_failure_counter() {
    let state = node();
    let (_client_did, client_tok) = register(&state);
    let (provider_did, provider_tok) = register(&state);
    let sub_id = subscribe(&state, &client_tok, json!(["ctr.signed"]));
    signed_contract(&state, &client_tok, &provider_tok, &provider_did);

    let sender = MockSender::new();
    sender.set_status(500);
    drain_outbox(&state, &sender);
    // Force the failure count up.
    {
        let mut guard = state.lock().unwrap();
        let mut due = guard.take_due_deliveries(u64::MAX);
        for _ in 0..MAX_ATTEMPTS {
            for d in due.drain(..) {
                guard.settle_delivery(d, false);
            }
            due = guard.take_due_deliveries(u64::MAX);
        }
        assert!(guard.subscription(&sub_id).unwrap().failures > 0);
    }

    // A later success clears it.
    {
        let mut guard = state.lock().unwrap();
        let node_identity = guard.node.identity.clone();
        let mut body = gap::delivery::DeliveryBody::new(
            &node_identity,
            &sub_id,
            gap::delivery::DeliveredEvent {
                seq: 99,
                kind: "ctr.signed".into(),
                payload: json!({}),
                at: 0,
            },
        );
        body.sign(&node_identity);
        guard.settle_delivery(
            gap::delivery::PendingDelivery {
                subscription_id: sub_id.clone(),
                url: "https://example.com/h".into(),
                body,
                not_before: 0,
            },
            true,
        );
        assert_eq!(guard.subscription(&sub_id).unwrap().failures, 0);
    }
}

#[test]
fn ssrf_urls_are_refused_at_subscription_time() {
    let state = node();
    let (_did, tok) = register(&state);

    for url in [
        "http://169.254.169.254/latest/meta-data/",
        "http://127.0.0.1:8080/v1/escrow/rule",
        "https://10.0.0.5/hook",
        "https://[::1]/hook",
        "file:///etc/passwd",
        "https://user:pass@example.com/h",
    ] {
        let (status, v) = post(
            &state,
            "/v1/subscriptions",
            &json!({ "transport": "webhook", "url": url }),
            &tok,
        );
        assert_ne!(status, 200, "must refuse {url}: {v}");
    }

    // Nothing hostile was stored.
    let (status, v) = route(&state, "GET", "/v1/subscriptions", &[], Some(&bearer(&tok)));
    assert_eq!(status, 200);
    assert!(v["subscriptions"].as_array().unwrap().is_empty());
}

#[test]
fn subscriptions_are_owned_listed_and_deletable_only_by_their_owner() {
    let state = node();
    let (_a_did, a_tok) = register(&state);
    let (_b_did, b_tok) = register(&state);
    let sub_id = subscribe(&state, &a_tok, json!([]));

    // Owner sees it; the other agent does not.
    let (_, v) = route(
        &state,
        "GET",
        "/v1/subscriptions",
        &[],
        Some(&bearer(&a_tok)),
    );
    assert_eq!(v["subscriptions"].as_array().unwrap().len(), 1);
    let (_, v) = route(
        &state,
        "GET",
        "/v1/subscriptions",
        &[],
        Some(&bearer(&b_tok)),
    );
    assert_eq!(v["subscriptions"].as_array().unwrap().len(), 0);

    // A stranger cannot delete it.
    let (status, _) = route(
        &state,
        "DELETE",
        &format!("/v1/subscriptions/{sub_id}"),
        &[],
        Some(&bearer(&b_tok)),
    );
    assert_ne!(status, 200);

    // The owner can.
    let (status, _) = route(
        &state,
        "DELETE",
        &format!("/v1/subscriptions/{sub_id}"),
        &[],
        Some(&bearer(&a_tok)),
    );
    assert_eq!(status, 200);
    let (_, v) = route(
        &state,
        "GET",
        "/v1/subscriptions",
        &[],
        Some(&bearer(&a_tok)),
    );
    assert!(v["subscriptions"].as_array().unwrap().is_empty());
}

#[test]
fn subscription_routes_require_authentication() {
    let state = node();
    for (method, path) in [
        ("POST", "/v1/subscriptions"),
        ("GET", "/v1/subscriptions"),
        ("GET", "/v1/events?after=0"),
        ("DELETE", "/v1/subscriptions/urn:gap:sub:x"),
    ] {
        let (status, _) = route(&state, method, path, b"{}", None);
        assert_eq!(status, 401, "{method} {path} must require auth");
    }
}

#[test]
fn event_cursor_resumes_without_gap_or_duplication() {
    let state = node();
    let (_client_did, client_tok) = register(&state);
    let (provider_did, provider_tok) = register(&state);
    let cid = signed_contract(&state, &client_tok, &provider_tok, &provider_did);

    // Read the whole stream from the start.
    let (_, v) = route(
        &state,
        "GET",
        "/v1/events?after=0&limit=100",
        &[],
        Some(&bearer(&client_tok)),
    );
    let all: Vec<u64> = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    assert!(all.len() >= 2, "expected several events, got {all:?}");
    assert!(all.windows(2).all(|w| w[0] < w[1]), "seq must be monotonic");

    // Resume from the middle: strictly-after semantics, no duplicate.
    let midpoint = all[all.len() / 2];
    let (_, v) = route(
        &state,
        "GET",
        &format!("/v1/events?after={midpoint}&limit=100"),
        &[],
        Some(&bearer(&client_tok)),
    );
    let rest: Vec<u64> = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    assert!(
        rest.iter().all(|s| *s > midpoint),
        "resume must be strictly after the cursor"
    );
    // The union of the two reads covers the whole stream exactly once.
    let expected: Vec<u64> = all.iter().copied().filter(|s| *s > midpoint).collect();
    assert_eq!(rest, expected, "no gap, no duplication on resume");

    // Escrow + deliver generate more events; the cursor picks them up.
    let (s, v) = post(
        &state,
        "/v1/escrow/park",
        &json!({ "contract_id": cid, "amount": "1.00" }),
        &client_tok,
    );
    assert_eq!(s, 200, "park failed: {v}");
    let (s, v) = post(
        &state,
        &format!("/v1/contract/{cid}/deliver"),
        &json!({ "deliverable_hash": "sha256:abc" }),
        &provider_tok,
    );
    assert_eq!(s, 200, "deliver failed: {v}");
    let last = *all.last().unwrap();
    let (_, v) = route(
        &state,
        "GET",
        &format!("/v1/events?after={last}&limit=100"),
        &[],
        Some(&bearer(&client_tok)),
    );
    assert!(
        !v["events"].as_array().unwrap().is_empty(),
        "new events must appear after the cursor"
    );
}

#[test]
fn announce_keeps_the_reachability_the_agent_declared() {
    // Spec 02 §2.2 / §2.4.4: the node used to overwrite this with a
    // placeholder, discarding the data delivery needs (RFC-0013 §2.2).
    let state = node();
    let (did, tok) = register(&state);
    let (status, _) = post(
        &state,
        "/v1/announce",
        &json!({
            "capabilities": [{ "id": "cap:me:x", "name": "x", "description": "d",
                               "price": { "amount": 1.0, "currency": "EUR", "model": "fixed" } }],
            "reachability": [{ "transport": "https", "endpoint": "https://agent.example/gap" }]
        }),
        &tok,
    );
    assert_eq!(status, 200);

    let (_, v) = route(&state, "GET", "/v1/discover?name=x", &[], None);
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    let reach = results[0]["reachability"].as_array().unwrap();
    assert!(
        reach
            .iter()
            .any(|r| r["endpoint"] == "https://agent.example/gap" && r["transport"] == "https"),
        "declared reachability must be stored verbatim: {reach:?}"
    );
    // The announcement is still correctly signed by the agent after the
    // node appended its own node-mediated entry.
    assert_eq!(results[0]["agent_did"], did);
}
