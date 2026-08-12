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
fn a_buyer_may_release_against_an_adverse_ruling_but_it_is_recorded() {
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

    // The judges advise; the buyer decides. It is the buyer's money,
    // it asked for the review, and it is entitled to disagree with the
    // answer it got. Blocking here stranded contracts that neither
    // party could move.
    let (s, v) = post(
        &state,
        &format!("/v1/contract/{cid}/accept-delivery"),
        &json!({}),
        &ct,
    );
    assert_eq!(s, 200, "the buyer is the authority on its own money: {v}");
    assert_eq!(v["state"], "accepted");

    // But waving through work the judges called non-conforming is a
    // fact about this buyer, and it is on the record. A marketplace
    // where that happens silently has a conformance rate that means
    // nothing.
    let (_, audit) = route(
        &state,
        "GET",
        "/v1/audit",
        &[],
        Some(&format!("Bearer {ct}")),
    );
    let spine = audit.to_string();
    assert!(
        spine.contains("overrode_verdict") && spine.contains("nonconforming"),
        "the override must be visible in the spine: {spine}"
    );
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
        .find(|e| e["kind"] == "exe.verify")
        .expect("exe.verify must be on the spine");
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

// ---------------------------------------------------------------- RFC-0015

/// Build a node with a two-judge panel whose rulings the test controls.
fn node_with_panel(
    a: Ruling,
    b: Ruling,
) -> (Arc<Mutex<NodeState>>, Arc<MockVerifier>, Arc<MockVerifier>) {
    struct Named(Arc<MockVerifier>, &'static str);
    impl gap::verifier::Verifier for Named {
        fn judge(&self, e: &gap::verifier::Evidence) -> gap::error::Result<(Ruling, Vec<String>)> {
            self.0.judge(e)
        }
        fn name(&self) -> String {
            self.1.to_string()
        }
    }
    let mut state = NodeState::with_rate_limits(
        Box::new(SqliteStorage::open(":memory:").unwrap()),
        None,
        1_000_000,
        1_000_000,
    );
    let ja = Arc::new(MockVerifier::new(a));
    let jb = Arc::new(MockVerifier::new(b));
    state.set_verifier(Box::new(Named(ja.clone(), "judge-a")));
    state.set_second_verifier(Box::new(Named(jb.clone(), "judge-b")));
    state.set_admin_token("test-admin");
    (Arc::new(Mutex::new(state)), ja, jb)
}

#[test]
fn agreeing_judges_produce_one_ruling_and_release() {
    // Both judges reject, so both are consulted: only a `conforms`
    // ends the panel early, because only a rejection costs the provider
    // anything and needs confirming.
    let (state, ja, jb) = node_with_panel(Ruling::Nonconforming, Ruling::Nonconforming);
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
    assert!(v["escalation"].is_null(), "unanimity needs no human");
    assert_eq!(v["opinions"].as_array().unwrap().len(), 2);
    assert_eq!(ja.calls().len(), 1);
    assert_eq!(jb.calls().len(), 1);

    let (s, _) = post(
        &state,
        &format!("/v1/contract/{cid}/accept-delivery"),
        &json!({}),
        &ct,
    );
    assert_eq!(s, 200);
}

#[test]
fn disagreeing_judges_escalate_to_a_human_and_hold_the_money() {
    // This is the whole point of the panel: independent judges
    // disagreeing IS the signal that a human is needed.
    // The first judge must not pass the work: a leading `conforms` ends
    // the panel by design, and there is then no disagreement to reach.
    let (state, _, _) = node_with_panel(Ruling::Nonconforming, Ruling::Conforms);
    let (_c, ct) = register(&state);
    let (pd, pt) = register(&state);
    let cid = delivered(&state, &ct, &pt, &pd, &sha(CONTENT), false);

    let (_, v) = post(
        &state,
        &format!("/v1/contract/{cid}/verify"),
        &json!({ "content": CONTENT }),
        &ct,
    );
    assert_eq!(v["ruling"], "inconclusive", "fail closed on disagreement");
    assert_eq!(v["escalation"], "judge_disagreement");
    let both: Vec<&str> = v["opinions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["ruling"].as_str().unwrap())
        .collect();
    assert!(both.contains(&"conforms") && both.contains(&"nonconforming"));

    // A disagreement among advisers is not a veto over the buyer.
    // This exact case - two judges splitting on a perfectly good
    // delivery - left contracts stuck in `delivered`: not
    // `nonconforming`, so the provider had no remedy, and not
    // acceptable either, so the escrow sat parked indefinitely.
    let (s, v) = post(
        &state,
        &format!("/v1/contract/{cid}/accept-delivery"),
        &json!({}),
        &ct,
    );
    assert_eq!(s, 200, "a split panel must not strand the contract: {v}");
    assert_eq!(v["state"], "accepted");

    // And it lands in the operator's queue.
    let (s, q) = route(
        &state,
        "GET",
        "/v1/escalations",
        &[],
        Some(&bearer("test-admin")),
    );
    assert_eq!(s, 200);
    assert_eq!(q["count"], 1);
    assert_eq!(q["escalations"][0]["reason"], "judge_disagreement");
    assert_eq!(q["escalations"][0]["contract_id"], cid);
}

#[test]
fn the_escalation_queue_is_operator_only() {
    let (state, _, _) = node_with_panel(Ruling::Conforms, Ruling::Conforms);
    let (_c, ct) = register(&state);
    for auth in [None, Some(bearer(&ct))] {
        let (s, _) = route(&state, "GET", "/v1/escalations", &[], auth.as_deref());
        assert_eq!(s, 401, "the queue is not public");
    }
}

#[test]
fn a_negotiated_value_threshold_summons_a_human_even_on_agreement() {
    let (state, _, _) = node_with_panel(Ruling::Conforms, Ruling::Conforms);
    let (_c, ct) = register(&state);
    let (pd, pt) = register(&state);

    // The parties themselves set the bar, in the contract.
    let terms = json!({
        "input": {}, "deliverable": {},
        "acceptance_criteria": ["each lead has a verified email"],
        "deadline": 4_102_444_800u64,
        "price": { "amount": 5.0, "currency": "EUR", "model": "fixed", "cap": 5.0 },
        "autonomy": "propose",
        "human_review_above": "1.00"
    });
    let (_, v) = post(
        &state,
        "/v1/contract/propose",
        &json!({ "provider": pd, "capability_id": "cap:x", "terms": terms, "escrow": true }),
        &ct,
    );
    let cid = v["contract_id"].as_str().unwrap().to_string();
    post(
        &state,
        &format!("/v1/contract/{cid}/accept"),
        &json!({}),
        &pt,
    );
    post(
        &state,
        "/v1/escrow/park",
        &json!({ "contract_id": cid, "amount": "5.00" }),
        &ct,
    );
    post(
        &state,
        &format!("/v1/contract/{cid}/deliver"),
        &json!({ "deliverable_hash": sha(CONTENT) }),
        &pt,
    );

    let (_, v) = post(
        &state,
        &format!("/v1/contract/{cid}/verify"),
        &json!({ "content": CONTENT }),
        &ct,
    );
    assert_eq!(v["ruling"], "conforms", "the judges still agree");
    assert_eq!(
        v["escalation"], "value_threshold",
        "but the money is big enough that a human looks"
    );
    let (s, _) = post(
        &state,
        &format!("/v1/contract/{cid}/accept-delivery"),
        &json!({}),
        &ct,
    );
    assert_ne!(s, 200, "held until a human closes it");
}

#[test]
fn dispute_stats_measure_being_wrong_not_being_disputed() {
    let (state, _, _) = node_with_panel(Ruling::Conforms, Ruling::Conforms);
    let (client_did, ct) = register(&state);
    let (pd, pt) = register(&state);
    let cid = delivered(&state, &ct, &pt, &pd, &sha(CONTENT), false);
    let (s, _) = post(
        &state,
        &format!("/v1/contract/{cid}/dispute"),
        &json!({ "reason": "nonconforming" }),
        &ct,
    );
    assert_eq!(s, 200);

    // The arbitrator sides with the provider: the client's dispute fails.
    let (s, _) = post(
        &state,
        "/v1/escrow/rule",
        &json!({ "contract_id": cid, "split": { "client": 0.0, "provider": 1.0 } }),
        "test-admin",
    );
    assert_eq!(s, 200);

    let (_, client_rep) = route(
        &state,
        "GET",
        &format!("/v1/reputation/{client_did}"),
        &[],
        None,
    );
    let d = &client_rep["disputes"];
    assert_eq!(d["raised"], 1);
    assert_eq!(d["raised_won"], 0);
    assert_eq!(
        d["win_rate"], 0.0,
        "disputing and losing is the abuse signal"
    );

    // The provider merely received a dispute and won it: nothing counts
    // against it. Otherwise disputing a competitor would be a free way
    // to tarnish it.
    let (_, prov_rep) = route(&state, "GET", &format!("/v1/reputation/{pd}"), &[], None);
    assert_eq!(prov_rep["disputes"]["received"], 1);
    assert_eq!(prov_rep["disputes"]["received_lost"], 0);
    assert!(prov_rep["disputes"]["win_rate"].is_null());
}

#[test]
fn a_won_dispute_counts_for_the_agent_that_raised_it() {
    let (state, _, _) = node_with_panel(Ruling::Conforms, Ruling::Conforms);
    let (client_did, ct) = register(&state);
    let (pd, pt) = register(&state);
    let cid = delivered(&state, &ct, &pt, &pd, &sha(CONTENT), false);
    post(
        &state,
        &format!("/v1/contract/{cid}/dispute"),
        &json!({ "reason": "nonconforming" }),
        &ct,
    );
    post(
        &state,
        "/v1/escrow/rule",
        &json!({ "contract_id": cid, "split": { "client": 1.0, "provider": 0.0 } }),
        "test-admin",
    );
    let (_, rep) = route(
        &state,
        "GET",
        &format!("/v1/reputation/{client_did}"),
        &[],
        None,
    );
    assert_eq!(rep["disputes"]["raised_won"], 1);
    assert_eq!(rep["disputes"]["win_rate"], 1.0);
    let (_, prov) = route(&state, "GET", &format!("/v1/reputation/{pd}"), &[], None);
    assert_eq!(prov["disputes"]["received_lost"], 1);
}

#[test]
fn human_arbitration_clears_the_escalation() {
    let (state, _, _) = node_with_panel(Ruling::Conforms, Ruling::Nonconforming);
    let (_c, ct) = register(&state);
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
        &format!("/v1/contract/{cid}/dispute"),
        &json!({ "reason": "ambiguous" }),
        &ct,
    );
    let (s, _) = post(
        &state,
        "/v1/escrow/rule",
        &json!({ "contract_id": cid, "split": { "client": 0.5, "provider": 0.5 } }),
        "test-admin",
    );
    assert_eq!(s, 200);
    let (_, q) = route(
        &state,
        "GET",
        "/v1/escalations",
        &[],
        Some(&bearer("test-admin")),
    );
    assert_eq!(q["count"], 0, "a human ruled: the case is closed");
}

#[test]
fn a_failed_delivery_gets_exactly_one_second_chance() {
    // spec 03 §3.5: `ctr.remedy` — rework within the remedy window.
    let (state, ja, jb) = node_with_panel(Ruling::Nonconforming, Ruling::Nonconforming);
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

    // The provider fixes it and resubmits.
    const FIXED: &str =
        r#"{"leads":[{"email":"a@x.com","verified":true},{"email":"b@y.com","verified":true}]}"#;
    let (s, v) = post(
        &state,
        &format!("/v1/contract/{cid}/remedy"),
        &json!({ "deliverable_hash": sha(FIXED) }),
        &pt,
    );
    assert_eq!(s, 200, "remedy: {v}");
    assert_eq!(v["attempts_left"], 0);

    // The stale verdict is gone, so release is no longer blocked by it.
    ja.set_ruling(Ruling::Conforms);
    jb.set_ruling(Ruling::Conforms);
    let (_, v) = post(
        &state,
        &format!("/v1/contract/{cid}/verify"),
        &json!({ "content": FIXED }),
        &ct,
    );
    assert_eq!(v["ruling"], "conforms", "the reworked artifact passes");
    let (s, _) = post(
        &state,
        &format!("/v1/contract/{cid}/accept-delivery"),
        &json!({}),
        &ct,
    );
    assert_eq!(s, 200);

    // The track record is honest about the rework.
    let (_, rep) = route(&state, "GET", &format!("/v1/reputation/{pd}"), &[], None);
    assert_eq!(rep["jobs"][0]["remedied"], true);
}

#[test]
fn the_second_chance_is_the_only_chance() {
    let (state, _, _) = node_with_panel(Ruling::Nonconforming, Ruling::Nonconforming);
    let (_c, ct) = register(&state);
    let (pd, pt) = register(&state);
    let cid = delivered(&state, &ct, &pt, &pd, &sha(CONTENT), false);
    post(
        &state,
        &format!("/v1/contract/{cid}/verify"),
        &json!({ "content": CONTENT }),
        &ct,
    );
    let (s, _) = post(
        &state,
        &format!("/v1/contract/{cid}/remedy"),
        &json!({ "deliverable_hash": sha("attempt 2") }),
        &pt,
    );
    assert_eq!(s, 200);
    // Fails again…
    post(
        &state,
        &format!("/v1/contract/{cid}/verify"),
        &json!({ "content": "attempt 2" }),
        &ct,
    );
    // …and there is no third attempt: unlimited retries would let a
    // provider grind against the judges until one reading passes.
    let (s, v) = post(
        &state,
        &format!("/v1/contract/{cid}/remedy"),
        &json!({ "deliverable_hash": sha("attempt 3") }),
        &pt,
    );
    assert_ne!(s, 200);
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("already used"));
}

#[test]
fn remedy_is_provider_only_and_needs_a_failure_to_fix() {
    let (state, _, _) = node_with_panel(Ruling::Conforms, Ruling::Conforms);
    let (_c, ct) = register(&state);
    let (pd, pt) = register(&state);
    let cid = delivered(&state, &ct, &pt, &pd, &sha(CONTENT), false);

    // Nothing has failed: there is nothing to remedy.
    let (s, v) = post(
        &state,
        &format!("/v1/contract/{cid}/remedy"),
        &json!({ "deliverable_hash": sha("x") }),
        &pt,
    );
    assert_ne!(s, 200);
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("nothing to remedy"));

    // And the client cannot resubmit work on the provider's behalf.
    post(
        &state,
        &format!("/v1/contract/{cid}/verify"),
        &json!({ "content": "mismatch" }),
        &ct,
    );
    let (s, _) = post(
        &state,
        &format!("/v1/contract/{cid}/remedy"),
        &json!({ "deliverable_hash": sha("x") }),
        &ct,
    );
    assert_eq!(s, 401);
}
