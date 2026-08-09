//! RFC-0014 integration: verified delivery and public reputation,
//! driven through the real HTTP routes with a scripted judge.

use gap::server::{route, NodeState};
use gap::storage::sqlite::SqliteStorage;
use gap::verifier::{MockVerifier, Ruling};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn node_with(judge: Option<MockVerifier>) -> (Arc<Mutex<NodeState>>, Option<Arc<MockVerifier>>) {
    let mut state = NodeState::with_rate_limits(
        Box::new(SqliteStorage::open(":memory:").unwrap()),
        None,
        1_000_000,
        1_000_000,
    );
    let handle = judge.map(Arc::new);
    if let Some(j) = &handle {
        // The node holds a boxed trait object; the test keeps a handle
        // to the same judge through a shim.
        struct Shim(Arc<MockVerifier>);
        impl gap::verifier::Verifier for Shim {
            fn judge(
                &self,
                e: &gap::verifier::Evidence,
            ) -> gap::error::Result<(Ruling, Vec<String>)> {
                self.0.judge(e)
            }
            fn name(&self) -> String {
                self.0.name()
            }
        }
        state.set_verifier(Box::new(Shim(j.clone())));
    }
    (Arc::new(Mutex::new(state)), handle)
}

fn register(state: &Arc<Mutex<NodeState>>) -> (String, String) {
    let (_, v) = route(state, "POST", "/v1/identity", b"{}", None);
    (
        v["did"].as_str().unwrap().to_string(),
        v["token"].as_str().unwrap().to_string(),
    )
}

fn bearer(t: &str) -> String {
    format!("Bearer {t}")
}

fn post(state: &Arc<Mutex<NodeState>>, path: &str, body: &Value, tok: &str) -> (u16, Value) {
    route(
        state,
        "POST",
        path,
        &body.to_string().into_bytes(),
        Some(&bearer(tok)),
    )
}

const CONTENT: &str = r#"{"leads":[{"email":"a@x.com","verified":true}]}"#;

fn sha(content: &str) -> String {
    format!("sha256:{}", gap::sha256_hex(content.as_bytes()))
}

/// Drive a contract to Delivered. `confidential` sets the terms flag.
fn delivered(
    state: &Arc<Mutex<NodeState>>,
    client_tok: &str,
    provider_tok: &str,
    provider_did: &str,
    hash: &str,
    confidential: bool,
) -> String {
    let mut terms = json!({
        "input": {}, "deliverable": {},
        "acceptance_criteria": ["each lead has a verified email"],
        "deadline": 4_102_444_800u64,
        "price": { "amount": 0.05, "currency": "EUR", "model": "fixed", "cap": 1.0 },
        "autonomy": "propose"
    });
    if confidential {
        terms["confidentiality"] = json!("encrypted");
    }
    let (s, v) = post(
        state,
        "/v1/contract/propose",
        &json!({ "provider": provider_did, "capability_id": "cap:x", "terms": terms, "escrow": true }),
        client_tok,
    );
    assert_eq!(s, 200, "propose: {v}");
    let cid = v["contract_id"].as_str().unwrap().to_string();
    post(
        state,
        &format!("/v1/contract/{cid}/accept"),
        &json!({}),
        provider_tok,
    );
    post(
        state,
        "/v1/escrow/park",
        &json!({ "contract_id": cid, "amount": "0.05" }),
        client_tok,
    );
    let (s, v) = post(
        state,
        &format!("/v1/contract/{cid}/deliver"),
        &json!({ "deliverable_hash": hash }),
        provider_tok,
    );
    assert_eq!(s, 200, "deliver: {v}");
    cid
}

#[test]
fn conforming_delivery_is_verified_signed_and_settles() {
    let (state, judge) = node_with(Some(MockVerifier::new(Ruling::Conforms)));
    let (_c, ct) = register(&state);
    let (pd, pt) = register(&state);
    let cid = delivered(&state, &ct, &pt, &pd, &sha(CONTENT), false);

    let (s, v) = post(
        &state,
        &format!("/v1/contract/{cid}/verify"),
        &json!({ "content": CONTENT }),
        &ct,
    );
    assert_eq!(s, 200, "verify: {v}");
    assert_eq!(v["ruling"], "conforms");
    assert!(v["signature"].as_str().unwrap().starts_with("ed25519:"));
    assert_eq!(v["evaluator"], state.lock().unwrap().node_did().to_string());
    // The integrity check ran on the bytes the client actually received.
    let names: Vec<&str> = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"deliverable_hash_matches"));
    assert_eq!(judge.unwrap().calls().len(), 1);

    // Acceptance is allowed and settles.
    let (s, v) = post(
        &state,
        &format!("/v1/contract/{cid}/accept-delivery"),
        &json!({}),
        &ct,
    );
    assert_eq!(s, 200, "accept: {v}");
    assert_eq!(v["state"], "accepted");
}

#[test]
fn nonconforming_verdict_blocks_release_and_points_at_the_dispute() {
    let (state, _) = node_with(Some(MockVerifier::new(Ruling::Nonconforming)));
    let (_c, ct) = register(&state);
    let (pd, pt) = register(&state);
    let cid = delivered(&state, &ct, &pt, &pd, &sha(CONTENT), false);

    let (_, v) = post(
        &state,
        &format!("/v1/contract/{cid}/verify"),
        &json!({ "content": CONTENT }),
        &ct,
    );
    assert_eq!(v["ruling"], "nonconforming");

    // Even the client — whose money it is — cannot release against
    // signed evidence that the work does not conform.
    let (s, v) = post(
        &state,
        &format!("/v1/contract/{cid}/accept-delivery"),
        &json!({}),
        &ct,
    );
    assert_ne!(s, 200, "release must be refused: {v}");
    assert_eq!(v["error"]["code"], "escrow_violation");
    assert!(v["error"]["message"].as_str().unwrap().contains("dispute"));
}

#[test]
fn a_swapped_deliverable_is_caught_without_consulting_the_judge() {
    // The provider commits to one artifact and ships another.
    let (state, judge) = node_with(Some(MockVerifier::new(Ruling::Conforms)));
    let (_c, ct) = register(&state);
    let (pd, pt) = register(&state);
    let committed = sha("the artifact that was promised");
    let cid = delivered(&state, &ct, &pt, &pd, &committed, false);

    let (_, v) = post(
        &state,
        &format!("/v1/contract/{cid}/verify"),
        &json!({ "content": "something else entirely" }),
        &ct,
    );
    assert_eq!(v["ruling"], "nonconforming");
    assert!(
        judge.unwrap().calls().is_empty(),
        "integrity failure is decided deterministically, not by a model"
    );
}

#[test]
fn confidential_contracts_never_send_content_to_the_judge() {
    let (state, judge) = node_with(Some(MockVerifier::new(Ruling::Conforms)));
    let (_c, ct) = register(&state);
    let (pd, pt) = register(&state);
    let cid = delivered(&state, &ct, &pt, &pd, &sha(CONTENT), true);

    let (_, v) = post(
        &state,
        &format!("/v1/contract/{cid}/verify"),
        &json!({ "content": CONTENT }),
        &ct,
    );
    assert_eq!(v["ruling"], "inconclusive");
    assert!(v["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r.as_str().unwrap().contains("confidential")));
    assert!(
        judge.unwrap().calls().is_empty(),
        "an NDA contract must not reach an external model"
    );

    // Inconclusive is not a block: the client may still accept its own
    // delivery — it simply does so on its own judgement.
    let (s, _) = post(
        &state,
        &format!("/v1/contract/{cid}/accept-delivery"),
        &json!({}),
        &ct,
    );
    assert_eq!(s, 200);
}

#[test]
fn without_a_judge_the_node_still_proves_integrity_but_stays_inconclusive() {
    let (state, _) = node_with(None);
    let (_c, ct) = register(&state);
    let (pd, pt) = register(&state);
    let cid = delivered(&state, &ct, &pt, &pd, &sha(CONTENT), false);

    let (_, v) = post(
        &state,
        &format!("/v1/contract/{cid}/verify"),
        &json!({ "content": CONTENT }),
        &ct,
    );
    assert_eq!(v["ruling"], "inconclusive");
    assert!(v["model"].is_null());
    let checks = v["checks"].as_array().unwrap();
    assert!(checks.iter().all(|c| c["passed"].as_bool().unwrap()));
}

#[test]
fn only_the_parties_may_request_verification() {
    let (state, _) = node_with(Some(MockVerifier::new(Ruling::Conforms)));
    let (_c, ct) = register(&state);
    let (pd, pt) = register(&state);
    let (_s, stranger) = register(&state);
    let cid = delivered(&state, &ct, &pt, &pd, &sha(CONTENT), false);

    let (s, _) = post(
        &state,
        &format!("/v1/contract/{cid}/verify"),
        &json!({ "content": CONTENT }),
        &stranger,
    );
    assert_eq!(s, 401);
    // Both parties may: the provider can prove it delivered.
    let (s, _) = post(
        &state,
        &format!("/v1/contract/{cid}/verify"),
        &json!({ "content": CONTENT }),
        &pt,
    );
    assert_eq!(s, 200);
}

#[test]
fn verdicts_are_recorded_on_the_audit_spine() {
    let (state, _) = node_with(Some(MockVerifier::new(Ruling::Conforms)));
    let (_c, ct) = register(&state);
    let (pd, pt) = register(&state);
    let cid = delivered(&state, &ct, &pt, &pd, &sha(CONTENT), false);
    post(
        &state,
        &format!("/v1/contract/{cid}/verify"),
        &json!({ "content": CONTENT }),
        &ct,
    );
    let (_, v) = route(
        &state,
        "GET",
        "/v1/audit?after=0&limit=200",
        &[],
        Some(&bearer(&ct)),
    );
    let verified = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "exe.verified")
        .expect("exe.verified must be on the spine");
    assert_eq!(verified["payload"]["ruling"], "conforms");
    assert!(verified["payload"]["evidence_digest"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
}

#[test]
fn reputation_exposes_an_anonymised_job_history() {
    let (state, _) = node_with(Some(MockVerifier::new(Ruling::Conforms)));
    let (client_did, ct) = register(&state);
    let (pd, pt) = register(&state);
    let cid = delivered(&state, &ct, &pt, &pd, &sha(CONTENT), false);
    post(
        &state,
        &format!("/v1/contract/{cid}/verify"),
        &json!({ "content": CONTENT }),
        &ct,
    );
    post(
        &state,
        &format!("/v1/contract/{cid}/accept-delivery"),
        &json!({}),
        &ct,
    );

    // Readable without a token: a track record you cannot read before
    // hiring is not a track record.
    let (s, v) = route(&state, "GET", &format!("/v1/reputation/{pd}"), &[], None);
    assert_eq!(s, 200, "reputation: {v}");
    assert_eq!(v["agent_did"], pd);
    assert_eq!(v["score"]["n"], 1);
    assert!(v["score"]["success_rate"].as_f64().unwrap() > 0.5);

    let jobs = v["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert_eq!(job["outcome"], "accepted");
    assert_eq!(job["verdict"], "conforms");
    assert_eq!(job["judged_by"], "mock-verifier");
    assert_eq!(job["capability_id"], "cap:x");

    // Anonymised: neither the contract id nor the counterparty DID leak.
    let raw = serde_json::to_string(&v).unwrap();
    assert!(!raw.contains(&cid), "contract id must not be exposed");
    assert!(
        !raw.contains(&client_did),
        "counterparty DID must not be exposed"
    );
    assert_eq!(job["job_ref"].as_str().unwrap().len(), 16);
    assert_eq!(job["counterparty_ref"].as_str().unwrap().len(), 16);
}

#[test]
fn reputation_of_an_unknown_agent_is_empty_not_an_error() {
    let (state, _) = node_with(None);
    let unknown = format!("did:gap:{}", "ab".repeat(32));
    let (s, v) = route(
        &state,
        "GET",
        &format!("/v1/reputation/{unknown}"),
        &[],
        None,
    );
    assert_eq!(s, 200);
    assert_eq!(v["score"]["n"], 0);
    assert!(v["jobs"].as_array().unwrap().is_empty());
    // A brand-new agent is 0.5, never a free 1.0.
    assert_eq!(v["score"]["success_rate"], 0.5);

    // A malformed DID is a client error, distinct from "no history".
    let (s, _) = route(&state, "GET", "/v1/reputation/not-a-did", &[], None);
    assert_eq!(s, 400);
}
