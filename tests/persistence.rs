//! A restart must not be an amnesia event.
//!
//! Contracts, escrows, identities and announcements each had a typed
//! table and survived. Everything else - verdicts, job history, dispute
//! counters, principal vetoes, budgets, subscriptions, escalations, the
//! daily spend counter - lived only in RAM, so a redeploy silently
//! discarded it. A veto that evaporates unfreezes an agent its operator
//! froze; a spend counter that resets grants a fresh daily allowance to
//! anyone who can trigger a deploy.
//!
//! These tests drive a node through real requests, drop it, rebuild it
//! over the SAME storage, and check what came back. Reopening the file
//! is what makes them honest: an in-process assertion would pass on a
//! node that never persisted anything.

use gap::server::{route, NodeState};
use gap::storage::sqlite::SqliteStorage;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn node(path: &str) -> Arc<Mutex<NodeState>> {
    Arc::new(Mutex::new(NodeState::with_vault(
        Box::new(SqliteStorage::open(path).unwrap()),
        Some([7u8; 32]),
        100_000,
        100_000,
        None,
    )))
}

fn post(n: &Arc<Mutex<NodeState>>, path: &str, body: Value, tok: &str) -> (u16, Value) {
    route(
        n,
        "POST",
        path,
        body.to_string().as_bytes(),
        Some(&format!("Bearer {tok}")),
    )
}

/// A funded, delivered contract. Returns (contract id, client, provider).
fn delivered(n: &Arc<Mutex<NodeState>>) -> (String, String, String) {
    let (_, client) = n.lock().unwrap().create_identity();
    let (pdid, provider) = n.lock().unwrap().create_identity();
    let now = gap::message::now_unix();
    let (_, out) = post(
        n,
        "/v1/contract/propose",
        json!({
            "provider": pdid, "capability_id": "cap:x", "escrow": true,
            "terms": { "input": {}, "deliverable": {}, "acceptance_criteria": ["ok"],
                "deadline": now + 3600,
                "price": { "amount": 0.2, "currency": "USDC", "model": "fixed", "cap": 1.0 },
                "autonomy": "propose", "confidentiality": null }
        }),
        &client,
    );
    let id = out["contract_id"].as_str().unwrap().to_string();
    post(n, &format!("/v1/contract/{id}/accept"), json!({}), &provider);
    post(
        n,
        "/v1/escrow/park",
        json!({ "contract_id": id, "amount": "0.20" }),
        &client,
    );
    post(n, &format!("/v1/contract/{id}/start"), json!({}), &provider);
    let digest = format!("sha256:{}", gap::sha256_hex(b"the work"));
    post(
        n,
        &format!("/v1/contract/{id}/deliver"),
        json!({ "deliverable_hash": digest, "content": "the work" }),
        &provider,
    );
    (id, client, provider)
}

#[test]
fn a_settled_job_and_its_verdict_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!("gap-persist-{}.db", std::process::id()));
    let path = dir.to_str().unwrap();
    let _ = std::fs::remove_file(path);

    let before = {
        let n = node(path);
        let (id, client, _p) = delivered(&n);
        post(
            &n,
            &format!("/v1/contract/{id}/accept-delivery"),
            json!({}),
            &client,
        );
        let (_, act) = route(&n, "GET", "/v1/activity", b"", None);
        act
    };
    assert_eq!(before["count"], json!(1), "a job was recorded: {before}");
    assert_eq!(before["jobs"][0]["verdict"], json!("conforms"));

    // Same database, new node: this is the redeploy.
    let n = node(path);
    let (_, after) = route(&n, "GET", "/v1/activity", b"", None);
    assert_eq!(
        after["count"],
        json!(1),
        "the public track record must not reset on restart: {after}"
    );
    assert_eq!(after["jobs"][0]["verdict"], json!("conforms"));
    assert_eq!(after["jobs"][0]["job_ref"], before["jobs"][0]["job_ref"]);
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_principal_veto_survives_a_restart() {
    // The worst of the losses: an operator freezes its agent, the node
    // restarts, and the agent is quietly live again.
    let dir = std::env::temp_dir().join(format!("gap-veto-{}.db", std::process::id()));
    let path = dir.to_str().unwrap();
    let _ = std::fs::remove_file(path);

    let agent_did = {
        let n = node(path);
        let (did, _tok) = n.lock().unwrap().create_identity();

        // Bind a principal, the way custody actually works: the
        // principal signs, and the node co-signs for the agent whose key
        // it holds.
        let principal = gap::identity::AgentIdentity::generate();
        let mut b = gap::principal::PrincipalBinding::draft(
            gap::identity::Did::parse(&did).unwrap(),
            principal.did().clone(),
            gap::principal::Principal {
                kind: "organization".into(),
                name: "Geta.Team".into(),
                jurisdiction: "FR".into(),
                id: "FR-1".into(),
            },
            gap::governance::AutonomyLevel::ExecuteNotify,
            86_400,
        );
        b.sign_as_principal(&principal).unwrap();
        let agent_identity = n.lock().unwrap().agent_identity_for(&did).unwrap();
        b.sign_as_agent(&agent_identity).unwrap();
        let (status, out) = route(
            &n,
            "POST",
            "/v1/principal/bind",
            serde_json::to_string(&b).unwrap().as_bytes(),
            None,
        );
        assert_eq!(status, 200, "bind rejected: {out}");

        let veto = gap::principal::Veto::signed(
            &principal,
            gap::identity::Did::parse(&did).unwrap(),
            "all",
            "spending too fast",
        );
        let (status, out) = route(
            &n,
            "POST",
            "/v1/principal/veto",
            serde_json::to_string(&veto).unwrap().as_bytes(),
            None,
        );
        assert_eq!(status, 200, "veto rejected: {out}");
        did
    };

    // Same database, new node.
    let n = node(path);
    let (_, status_body) = route(&n, "GET", &format!("/v1/principal/{agent_did}"), b"", None);
    let vetoes = status_body["vetoes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !vetoes.is_empty(),
        "a veto must outlive the node that recorded it: {status_body}"
    );
    let _ = std::fs::remove_file(path);
}
