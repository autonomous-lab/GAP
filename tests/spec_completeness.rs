//! Spec requirements that were specified from v0.1 and unimplemented
//! until this pass: principal rights (01 §1.3, 06 §6.5), the rest of
//! the negotiation state machine (03 §3.3), execution signals
//! (04 §4.2), and deregistration (02 §2.5).

use gap::identity::AgentIdentity;
use gap::principal::{BudgetGrant, Principal, PrincipalBinding, Unbind, Veto};
use gap::server::{route, NodeState};
use gap::storage::sqlite::SqliteStorage;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn node() -> Arc<Mutex<NodeState>> {
    Arc::new(Mutex::new(NodeState::with_rate_limits(
        Box::new(SqliteStorage::open(":memory:").unwrap()),
        None,
        1_000_000,
        1_000_000,
    )))
}
fn bearer(t: &str) -> String {
    format!("Bearer {t}")
}
fn post(s: &Arc<Mutex<NodeState>>, p: &str, b: &Value, t: &str) -> (u16, Value) {
    route(s, "POST", p, &b.to_string().into_bytes(), Some(&bearer(t)))
}
fn anon(s: &Arc<Mutex<NodeState>>, p: &str, b: &Value) -> (u16, Value) {
    route(s, "POST", p, &b.to_string().into_bytes(), None)
}
fn register(s: &Arc<Mutex<NodeState>>) -> (String, String) {
    let (_, v) = route(s, "POST", "/v1/identity", b"{}", None);
    (
        v["did"].as_str().unwrap().into(),
        v["token"].as_str().unwrap().into(),
    )
}
fn terms(price: f64) -> Value {
    json!({
        "input": {}, "deliverable": {}, "acceptance_criteria": ["ok"],
        "deadline": 4_102_444_800u64,
        "price": { "amount": price, "currency": "EUR", "model": "fixed", "cap": price },
        "autonomy": "propose"
    })
}
fn propose(s: &Arc<Mutex<NodeState>>, ct: &str, pd: &str, price: f64) -> String {
    let (st, v) = post(
        s,
        "/v1/contract/propose",
        &json!({ "provider": pd, "capability_id": "cap:x", "terms": terms(price), "escrow": true }),
        ct,
    );
    assert_eq!(st, 200, "propose: {v}");
    v["contract_id"].as_str().unwrap().into()
}

/// Bind a principal to an agent DID; returns the principal identity.
fn bind(state: &Arc<Mutex<NodeState>>, agent_did: &str) -> AgentIdentity {
    let principal = AgentIdentity::generate();
    let agent_key = AgentIdentity::generate(); // stands in for the agent's own key
    let _ = agent_key;
    let mut b = PrincipalBinding::draft(
        gap::identity::Did::parse(agent_did).unwrap(),
        principal.did().clone(),
        Principal {
            kind: "organization".into(),
            name: "Geta.Team".into(),
            jurisdiction: "FR".into(),
            id: "FR-1".into(),
        },
        gap::governance::AutonomyLevel::ExecuteNotify,
        86_400,
    );
    b.sign_as_principal(&principal).unwrap();
    // The node holds the agent's key, so it co-signs on the agent's
    // behalf: this is the custody model, and the test mirrors it.
    let agent_identity = state.lock().unwrap().agent_identity_for(agent_did).unwrap();
    b.sign_as_agent(&agent_identity).unwrap();
    let (s, v) = anon(
        state,
        "/v1/principal/bind",
        &serde_json::to_value(&b).unwrap(),
    );
    assert_eq!(s, 200, "bind: {v}");
    principal
}

#[test]
fn a_principal_veto_stops_its_agent_dead() {
    // Spec 06 §6.5 calls these rights inalienable. Before this pass the
    // node implemented none of them.
    let state = node();
    let (client_did, ct) = register(&state);
    let (pd, _pt) = register(&state);
    let principal = bind(&state, &client_did);

    // Normal operation first.
    let _ = propose(&state, &ct, &pd, 1.0);

    let veto = Veto::signed(
        &principal,
        gap::identity::Did::parse(&client_did).unwrap(),
        Veto::SCOPE_ALL,
        "spend freeze",
    );
    let (s, v) = anon(
        &state,
        "/v1/principal/veto",
        &serde_json::to_value(&veto).unwrap(),
    );
    assert_eq!(s, 200, "veto: {v}");

    // The agent's own valid token is now powerless — which is the point:
    // a veto that the agent could ignore would not be a veto.
    let (s, v) = post(
        &state,
        "/v1/contract/propose",
        &json!({ "provider": pd, "capability_id": "cap:x", "terms": terms(1.0), "escrow": true }),
        &ct,
    );
    assert_ne!(s, 200, "vetoed agent must not contract: {v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("veto"));
}

#[test]
fn only_the_bound_principal_may_veto() {
    let state = node();
    let (client_did, ct) = register(&state);
    let (pd, _) = register(&state);
    let _principal = bind(&state, &client_did);

    let impostor = AgentIdentity::generate();
    let forged = Veto::signed(
        &impostor,
        gap::identity::Did::parse(&client_did).unwrap(),
        Veto::SCOPE_ALL,
        "malice",
    );
    let (s, _) = anon(
        &state,
        "/v1/principal/veto",
        &serde_json::to_value(&forged).unwrap(),
    );
    assert_ne!(s, 200, "a stranger cannot freeze somebody else's agent");
    // …and the agent still works.
    let _ = propose(&state, &ct, &pd, 1.0);
}

#[test]
fn a_veto_can_be_scoped_to_a_single_contract() {
    let state = node();
    let (client_did, ct) = register(&state);
    let (pd, pt) = register(&state);
    let principal = bind(&state, &client_did);
    let cid = propose(&state, &ct, &pd, 1.0);
    post(
        &state,
        &format!("/v1/contract/{cid}/accept"),
        &json!({}),
        &pt,
    );

    let veto = Veto::signed(
        &principal,
        gap::identity::Did::parse(&client_did).unwrap(),
        &cid,
        "this deal only",
    );
    anon(
        &state,
        "/v1/principal/veto",
        &serde_json::to_value(&veto).unwrap(),
    );

    // Blocked on that contract…
    let (s, _) = post(
        &state,
        "/v1/escrow/park",
        &json!({ "contract_id": cid, "amount": "1.00" }),
        &ct,
    );
    assert_ne!(s, 200);
    // …but free to do business elsewhere.
    let other = propose(&state, &ct, &pd, 1.0);
    post(
        &state,
        &format!("/v1/contract/{other}/accept"),
        &json!({}),
        &pt,
    );
    let (s, _) = post(
        &state,
        "/v1/escrow/park",
        &json!({ "contract_id": other, "amount": "1.00" }),
        &ct,
    );
    assert_eq!(s, 200);
}

#[test]
fn the_principal_budget_is_a_hard_cap_on_spending() {
    let state = node();
    let (client_did, ct) = register(&state);
    let (pd, pt) = register(&state);
    let principal = bind(&state, &client_did);

    let grant = BudgetGrant::signed(
        &principal,
        gap::identity::Did::parse(&client_did).unwrap(),
        "1.50",
        "EUR",
    );
    let (s, _) = anon(
        &state,
        "/v1/principal/budget",
        &serde_json::to_value(&grant).unwrap(),
    );
    assert_eq!(s, 200);

    // First park fits under the cap.
    let c1 = propose(&state, &ct, &pd, 1.0);
    post(
        &state,
        &format!("/v1/contract/{c1}/accept"),
        &json!({}),
        &pt,
    );
    let (s, _) = post(
        &state,
        "/v1/escrow/park",
        &json!({ "contract_id": c1, "amount": "1.00" }),
        &ct,
    );
    assert_eq!(s, 200);

    // The second would cross it, and the runtime refuses — the agent
    // cannot spend past what its principal allowed.
    let c2 = propose(&state, &ct, &pd, 1.0);
    post(
        &state,
        &format!("/v1/contract/{c2}/accept"),
        &json!({}),
        &pt,
    );
    let (s, v) = post(
        &state,
        "/v1/escrow/park",
        &json!({ "contract_id": c2, "amount": "1.00" }),
        &ct,
    );
    assert_ne!(s, 200);
    assert!(v["error"]["message"].as_str().unwrap().contains("budget"));
}

#[test]
fn unbinding_releases_the_veto_with_the_binding() {
    let state = node();
    let (client_did, ct) = register(&state);
    let (pd, _) = register(&state);
    let principal = bind(&state, &client_did);
    let did = gap::identity::Did::parse(&client_did).unwrap();
    let veto = Veto::signed(&principal, did.clone(), Veto::SCOPE_ALL, "pause");
    anon(
        &state,
        "/v1/principal/veto",
        &serde_json::to_value(&veto).unwrap(),
    );
    assert_ne!(
        post(
            &state,
            "/v1/contract/propose",
            &json!({ "provider": pd, "capability_id": "cap:x", "terms": terms(1.0), "escrow": true }),
            &ct
        )
        .0,
        200
    );

    // The principal releases the agent entirely.
    let binding = PrincipalBinding::draft(
        did.clone(),
        principal.did().clone(),
        Principal {
            kind: "organization".into(),
            name: "Geta.Team".into(),
            jurisdiction: "FR".into(),
            id: "FR-1".into(),
        },
        gap::governance::AutonomyLevel::ExecuteNotify,
        1,
    );
    let unbind = Unbind::signed(&binding, &principal).unwrap();
    let (s, v) = anon(
        &state,
        "/v1/principal/unbind",
        &serde_json::to_value(&unbind).unwrap(),
    );
    assert_eq!(s, 200, "unbind: {v}");
    let _ = propose(&state, &ct, &pd, 1.0);
}

#[test]
fn principal_status_is_publicly_readable() {
    let state = node();
    let (client_did, _ct) = register(&state);
    let principal = bind(&state, &client_did);
    let veto = Veto::signed(
        &principal,
        gap::identity::Did::parse(&client_did).unwrap(),
        Veto::SCOPE_ALL,
        "audit",
    );
    anon(
        &state,
        "/v1/principal/veto",
        &serde_json::to_value(&veto).unwrap(),
    );

    let (s, v) = route(
        &state,
        "GET",
        &format!("/v1/principal/{client_did}"),
        &[],
        None,
    );
    assert_eq!(s, 200);
    assert_eq!(v["bound"]["principal"], "Geta.Team");
    assert_eq!(v["vetoes"][0]["reason"], "audit");
}

// ------------------------------------------------ negotiation (03 §3.3)

#[test]
fn a_provider_can_counter_instead_of_walking_away() {
    let state = node();
    let (_cd, ct) = register(&state);
    let (pd, pt) = register(&state);
    let cid = propose(&state, &ct, &pd, 1.0);

    // The provider wants more money: it counters rather than rejecting.
    let (s, v) = post(
        &state,
        &format!("/v1/contract/{cid}/counter"),
        &json!({ "terms": terms(2.0) }),
        &pt,
    );
    assert_eq!(s, 200, "counter: {v}");
    assert_eq!(v["state"], "draft", "a counter keeps it in negotiation");

    let (_, c) = route(&state, "GET", &format!("/v1/contract/{cid}"), &[], None);
    assert_eq!(c["contract"]["terms"]["price"]["amount"], 2.0);
    // Fresh terms void both signatures: nobody edits a live document.
    assert!(c["contract"]["client_sig"].is_null());

    // The client accepts the revision, and the deal is signed.
    let (s, _) = post(
        &state,
        &format!("/v1/contract/{cid}/accept"),
        &json!({}),
        &ct,
    );
    assert_eq!(s, 200);
}

#[test]
fn either_party_can_reject_or_cancel() {
    let state = node();
    let (_cd, ct) = register(&state);
    let (pd, pt) = register(&state);

    let a = propose(&state, &ct, &pd, 1.0);
    let (s, v) = post(
        &state,
        &format!("/v1/contract/{a}/reject"),
        &json!({ "reason": "too cheap" }),
        &pt,
    );
    assert_eq!(s, 200, "reject: {v}");
    assert_eq!(v["state"], "rejected");
    // A rejected contract is terminal.
    let (s, _) = post(&state, &format!("/v1/contract/{a}/accept"), &json!({}), &pt);
    assert_ne!(s, 200);

    let b = propose(&state, &ct, &pd, 1.0);
    let (s, v) = post(
        &state,
        &format!("/v1/contract/{b}/cancel"),
        &json!({ "reason": "changed my mind" }),
        &ct,
    );
    assert_eq!(s, 200, "cancel: {v}");
    assert_eq!(v["state"], "cancelled");
}

#[test]
fn cancelling_after_escrow_returns_the_money() {
    let state = node();
    let (_cd, ct) = register(&state);
    let (pd, pt) = register(&state);
    let cid = propose(&state, &ct, &pd, 1.0);
    post(
        &state,
        &format!("/v1/contract/{cid}/accept"),
        &json!({}),
        &pt,
    );
    let (s, _) = post(
        &state,
        "/v1/escrow/park",
        &json!({ "contract_id": cid, "amount": "1.00" }),
        &ct,
    );
    assert_eq!(s, 200);
    let (s, v) = post(
        &state,
        &format!("/v1/contract/{cid}/cancel"),
        &json!({}),
        &ct,
    );
    assert_eq!(s, 200, "cancel: {v}");
    assert_eq!(
        v["escrow_refunded"], true,
        "a cancelled deal must not strand funds"
    );
}

#[test]
fn a_non_party_cannot_touch_the_negotiation() {
    let state = node();
    let (_cd, ct) = register(&state);
    let (pd, _pt) = register(&state);
    let (_sd, stranger) = register(&state);
    let cid = propose(&state, &ct, &pd, 1.0);
    for (path, body) in [
        (
            format!("/v1/contract/{cid}/counter"),
            json!({ "terms": terms(9.0) }),
        ),
        (format!("/v1/contract/{cid}/reject"), json!({})),
        (format!("/v1/contract/{cid}/cancel"), json!({})),
    ] {
        let (s, _) = post(&state, &path, &body, &stranger);
        assert_eq!(s, 401, "{path} must be party-only");
    }
}

// --------------------------------------------- execution signals (04 §4.2)

#[test]
fn a_provider_can_signal_start_and_progress() {
    let state = node();
    let (_cd, ct) = register(&state);
    let (pd, pt) = register(&state);
    let cid = propose(&state, &ct, &pd, 1.0);
    post(
        &state,
        &format!("/v1/contract/{cid}/accept"),
        &json!({}),
        &pt,
    );
    post(
        &state,
        "/v1/escrow/park",
        &json!({ "contract_id": cid, "amount": "1.00" }),
        &ct,
    );

    let (s, v) = post(
        &state,
        &format!("/v1/contract/{cid}/start"),
        &json!({ "plan": { "steps": ["scrape", "score"], "eta_secs": 120 } }),
        &pt,
    );
    assert_eq!(s, 200, "start: {v}");
    assert_eq!(v["state"], "executing");

    let (s, _) = post(
        &state,
        &format!("/v1/contract/{cid}/progress"),
        &json!({ "step": 1, "note": "scraped 40 pages" }),
        &pt,
    );
    assert_eq!(s, 200);

    // Both land on the spine under the normative kinds.
    let (_, a) = route(
        &state,
        "GET",
        "/v1/audit?after=0&limit=200",
        &[],
        Some(&bearer(&ct)),
    );
    let kinds: Vec<&str> = a["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"exe.start"), "{kinds:?}");
    assert!(kinds.contains(&"exe.progress"), "{kinds:?}");

    // Delivery still works after an explicit start.
    let (s, _) = post(
        &state,
        &format!("/v1/contract/{cid}/deliver"),
        &json!({ "deliverable_hash": format!("sha256:{}", "ab".repeat(32)) }),
        &pt,
    );
    assert_eq!(s, 200);
}

#[test]
fn progress_requires_execution_to_have_started() {
    let state = node();
    let (_cd, ct) = register(&state);
    let (pd, pt) = register(&state);
    let cid = propose(&state, &ct, &pd, 1.0);
    post(
        &state,
        &format!("/v1/contract/{cid}/accept"),
        &json!({}),
        &pt,
    );
    let (s, _) = post(
        &state,
        &format!("/v1/contract/{cid}/progress"),
        &json!({ "step": 1 }),
        &pt,
    );
    assert_ne!(s, 200, "no heartbeat before the work starts");
}

// ------------------------------------------------ deregistration (02 §2.5)

#[test]
fn an_agent_can_withdraw_and_leaves_a_tombstone() {
    let state = node();
    let (_did, tok) = register(&state);
    let caps = json!({ "capabilities": [{
        "id": "cap:me:x", "name": "withdrawable", "description": "d",
        "price": { "amount": 1.0, "currency": "EUR", "model": "fixed" } }] });
    post(&state, "/v1/announce", &caps, &tok);
    let (_, v) = route(&state, "GET", "/v1/discover?name=withdrawable", &[], None);
    assert_eq!(v["results"].as_array().unwrap().len(), 1);

    let (s, _) = post(&state, "/v1/deregister", &json!({}), &tok);
    assert_eq!(s, 200);
    let (_, v) = route(&state, "GET", "/v1/discover?name=withdrawable", &[], None);
    assert!(
        v["results"].as_array().unwrap().is_empty(),
        "a withdrawn agent must stop being discoverable"
    );
}
