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

/// Put a node in custodial mode without touching process-global state.
///
/// Also sets an operator token: with no chain to read, the only
/// legitimate way to fund a balance is the operator credit rail, and
/// the self-service one no longer exists precisely because a depositor
/// must not state its own amount.
fn custodial(n: &Arc<Mutex<NodeState>>) {
    let mut g = n.lock().unwrap();
    g.set_custody(gap::custody::CustodyPolicy {
        mode: gap::custody::CustodyMode::Custodial,
        currency: "USDC".into(),
        ..Default::default()
    });
    g.set_admin_token(String::from("operator-token"));
}

/// Fund a balance the way an operator would for a bank transfer.
fn credit(n: &Arc<Mutex<NodeState>>, did: &str, amount: &str) -> (u16, Value) {
    post(
        n,
        "/v1/balance/credit",
        json!({ "agent_did": did, "amount": amount, "reference": "bank-ref-1" }),
        "operator-token",
    )
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
    post(
        n,
        &format!("/v1/contract/{id}/accept"),
        json!({}),
        &provider,
    );
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

/// A five-cent contract settled entirely from a prefunded balance
/// (RFC-0016), with no on-chain transaction at any point.
#[test]
fn a_contract_settles_from_a_balance_and_the_ledger_balances() {
    // Custodial mode, set on the node rather than through the
    // environment: env vars are process-global and these tests run in
    // parallel, so one test's configuration would corrupt another's.
    let dir = std::env::temp_dir().join(format!("gap-balance-{}.db", std::process::id()));
    let path = dir.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    let n = node(path);
    custodial(&n);

    let (client_did, client) = n.lock().unwrap().create_identity();
    let (provider_did, provider) = n.lock().unwrap().create_identity();

    // One deposit funds many contracts: that is the entire point.
    let (status, out) = credit(&n, &client_did, "5.00");
    assert_eq!(status, 200, "credit rejected: {out}");
    assert_eq!(out["available"], json!("5.000000"));

    let now = gap::message::now_unix();
    let (_, proposed) = post(
        &n,
        "/v1/contract/propose",
        json!({
            "provider": provider_did, "capability_id": "cap:x", "escrow": true,
            "terms": { "input": {}, "deliverable": {}, "acceptance_criteria": ["ok"],
                "deadline": now + 3600,
                "price": { "amount": 0.05, "currency": "USDC", "model": "fixed", "cap": 1.0 },
                "autonomy": "propose", "confidentiality": null }
        }),
        &client,
    );
    let id = proposed["contract_id"].as_str().unwrap().to_string();
    post(
        &n,
        &format!("/v1/contract/{id}/accept"),
        json!({}),
        &provider,
    );

    let (status, out) = post(
        &n,
        "/v1/escrow/park",
        json!({ "contract_id": id, "amount": "0.05" }),
        &client,
    );
    assert_eq!(status, 200, "park rejected: {out}");

    // Parked funds are held, not spent: still the client's money, but
    // no longer available to commit twice.
    let (_, bal) = route(
        &n,
        "GET",
        "/v1/balance",
        b"",
        Some(&format!("Bearer {client}")),
    );
    assert_eq!(bal["available"], json!("4.950000"));
    assert_eq!(bal["held"], json!("0.050000"));

    post(
        &n,
        &format!("/v1/contract/{id}/start"),
        json!({}),
        &provider,
    );
    let digest = format!("sha256:{}", gap::sha256_hex(b"work"));
    post(
        &n,
        &format!("/v1/contract/{id}/deliver"),
        json!({ "deliverable_hash": digest, "content": "work" }),
        &provider,
    );
    let (status, settled) = post(
        &n,
        &format!("/v1/contract/{id}/accept-delivery"),
        json!({}),
        &client,
    );
    assert_eq!(status, 200, "settlement failed: {settled}");
    assert_eq!(settled["settlement"]["rail"], json!("balance"));

    // The money moved from one ledger to the other, and nowhere else.
    let (_, cbal) = route(
        &n,
        "GET",
        "/v1/balance",
        b"",
        Some(&format!("Bearer {client}")),
    );
    assert_eq!(cbal["available"], json!("4.950000"));
    assert_eq!(cbal["held"], json!("0.000000"));
    let (_, pbal) = route(
        &n,
        "GET",
        "/v1/balance",
        b"",
        Some(&format!("Bearer {provider}")),
    );
    assert_eq!(pbal["available"], json!("0.050000"));

    // Liabilities equal what was deposited: the node owes exactly what
    // it took in, and the attestation says so.
    let (_, reserves) = route(&n, "GET", "/v1/reserves", b"", None);
    assert_eq!(reserves["liabilities"], json!("5.000000"));
    assert_eq!(reserves["solvent"], json!(true));
    assert!(reserves["signature"].is_string());
    assert!(reserves["spine_seq"].as_u64().unwrap_or(0) > 0);

    // And it all survives a restart, because it is other people's money.
    drop(n);
    let n2 = node(path);
    custodial(&n2);
    let (_, after) = route(
        &n2,
        "GET",
        "/v1/balance",
        b"",
        Some(&format!("Bearer {provider}")),
    );
    assert_eq!(
        after["available"],
        json!("0.050000"),
        "a balance must outlive the process holding it: {after}"
    );
    let _ = std::fs::remove_file(path);
}

/// A node that cannot pay must refuse before the contract advances.
#[test]
fn parking_more_than_the_balance_is_refused_not_overdrawn() {
    let dir = std::env::temp_dir().join(format!("gap-overdraw-{}.db", std::process::id()));
    let path = dir.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    let n = node(path);
    custodial(&n);

    let (client_did, client) = n.lock().unwrap().create_identity();
    let (provider_did, provider) = n.lock().unwrap().create_identity();
    credit(&n, &client_did, "0.01");

    let now = gap::message::now_unix();
    let (_, proposed) = post(
        &n,
        "/v1/contract/propose",
        json!({
            "provider": provider_did, "capability_id": "cap:x", "escrow": true,
            "terms": { "input": {}, "deliverable": {}, "acceptance_criteria": ["ok"],
                "deadline": now + 3600,
                "price": { "amount": 1.0, "currency": "USDC", "model": "fixed", "cap": 2.0 },
                "autonomy": "propose", "confidentiality": null }
        }),
        &client,
    );
    let id = proposed["contract_id"].as_str().unwrap().to_string();
    post(
        &n,
        &format!("/v1/contract/{id}/accept"),
        json!({}),
        &provider,
    );

    let (status, out) = post(
        &n,
        "/v1/escrow/park",
        json!({ "contract_id": id, "amount": "1.00" }),
        &client,
    );
    assert_ne!(status, 200, "an overdraft must be refused");
    assert!(
        out.to_string().contains("insufficient balance"),
        "and must say so plainly: {out}"
    );

    // A custodian that extends credit is a lender, which is a different
    // regulated activity: the balance must be untouched.
    let (_, bal) = route(
        &n,
        "GET",
        "/v1/balance",
        b"",
        Some(&format!("Bearer {client}")),
    );
    assert_eq!(bal["available"], json!("0.010000"));
    assert_eq!(bal["held"], json!("0.000000"));
    let _ = std::fs::remove_file(path);
}

/// An agent must not be able to state its own deposit.
#[test]
fn an_agent_cannot_credit_itself() {
    // The first cut of this endpoint took an `amount` from the caller
    // and credited it. With real money that is a faucet: the depositor
    // is precisely the party that benefits from overstating the figure.
    let dir = std::env::temp_dir().join(format!("gap-selfcredit-{}.db", std::process::id()));
    let path = dir.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    let n = node(path);
    custodial(&n);

    let (did, agent) = n.lock().unwrap().create_identity();

    // The self-service rail no longer accepts an amount at all, and
    // without a chain to read it can credit nothing.
    let (status, out) = post(
        &n,
        "/v1/balance/deposit",
        json!({ "amount": "1000000.00" }),
        &agent,
    );
    assert_ne!(status, 200, "an unverified deposit must be refused: {out}");
    assert!(
        out.to_string().contains("cannot verify deposits"),
        "and must say why: {out}"
    );

    // The operator rail is not open to the agent either.
    let (status, _) = post(
        &n,
        "/v1/balance/credit",
        json!({ "agent_did": did, "amount": "1000000.00", "reference": "nice try" }),
        &agent,
    );
    assert_eq!(status, 401, "operator authority is not an agent's to claim");

    let (_, bal) = route(
        &n,
        "GET",
        "/v1/balance",
        b"",
        Some(&format!("Bearer {agent}")),
    );
    assert_eq!(bal["available"], json!("0.000000"));
    let _ = std::fs::remove_file(path);
}

/// An operator credit has to point at something outside this node.
#[test]
fn an_operator_credit_requires_an_external_reference() {
    let dir = std::env::temp_dir().join(format!("gap-ref-{}.db", std::process::id()));
    let path = dir.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    let n = node(path);
    custodial(&n);
    let (did, _agent) = n.lock().unwrap().create_identity();

    // Money appearing from nowhere is exactly what proof of reserves
    // exists to expose, so it must at least be traceable.
    let (status, out) = post(
        &n,
        "/v1/balance/credit",
        json!({ "agent_did": did, "amount": "10.00" }),
        "operator-token",
    );
    assert_ne!(status, 200);
    assert!(out.to_string().contains("reference required"), "{out}");
    let _ = std::fs::remove_file(path);
}

/// The deposit address is derived, and it is the agent's alone.
#[test]
fn each_agent_gets_its_own_stable_deposit_address() {
    let dir = std::env::temp_dir().join(format!("gap-addr-{}.db", std::process::id()));
    let path = dir.to_str().unwrap();
    let _ = std::fs::remove_file(path);

    let (a_addr, b_addr, a_did) = {
        let n = node(path);
        custodial(&n);
        let (a_did, a) = n.lock().unwrap().create_identity();
        let (_b_did, b) = n.lock().unwrap().create_identity();
        let (sa, ra) = route(
            &n,
            "GET",
            "/v1/balance/address",
            b"",
            Some(&format!("Bearer {a}")),
        );
        let (sb, rb) = route(
            &n,
            "GET",
            "/v1/balance/address",
            b"",
            Some(&format!("Bearer {b}")),
        );
        assert_eq!(sa, 200, "{ra}");
        assert_eq!(sb, 200, "{rb}");
        (
            ra["deposit_address"].as_str().unwrap().to_string(),
            rb["deposit_address"].as_str().unwrap().to_string(),
            a_did,
        )
    };

    assert_ne!(
        a_addr, b_addr,
        "two agents sharing an address cannot be told apart"
    );
    assert!(a_addr.starts_with("0x") && a_addr.len() == 42);

    // Derived, not stored: the same node seed must rebuild the same
    // address after a restart, or funds sent there become unreachable.
    let n2 = node(path);
    custodial(&n2);
    let rebuilt = n2.lock().unwrap().deposit_address_for(&a_did).unwrap();
    assert_eq!(
        rebuilt, a_addr,
        "a derived address must survive the process that first produced it"
    );
    let _ = std::fs::remove_file(path);
}

/// A node with no on-ramp configured says so rather than linking nowhere.
#[test]
fn onramp_links_are_refused_when_nothing_is_configured() {
    let dir = std::env::temp_dir().join(format!("gap-onramp-{}.db", std::process::id()));
    let path = dir.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    let n = node(path);
    custodial(&n);
    let (_did, agent) = n.lock().unwrap().create_identity();

    let (status, out) = route(
        &n,
        "GET",
        "/v1/onramp?currency=EUR",
        b"",
        Some(&format!("Bearer {agent}")),
    );
    assert_ne!(status, 200);
    assert!(
        out.to_string().contains("no on-ramp is configured"),
        "and points at the alternative: {out}"
    );
    let _ = std::fs::remove_file(path);
}

/// A settled job must stay reachable across a restart.
///
/// The reported symptom: an agent's public history rendered a row for
/// job `e4f3f6e478ecd924`, the row linked to `/job/e4f3f6e478ecd924`,
/// and the link answered `unknown route`. The row came from `jobs`,
/// which survives. The link needed `jobs_by_ref`, which was rebuilt by
/// scanning the *verdicts* for a contract whose pseudonym matched - so
/// a job whose verdict failed to load lost its link while the page
/// that generated the link carried on advertising it.
///
/// Rebuilding from the contracts removes that coupling: a job_ref is a
/// contract's pseudonym and exists whether or not anyone judged it.
#[test]
fn a_job_link_survives_a_restart() {
    let path = std::env::temp_dir().join(format!("gap-joblink-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);

    let (job_ref, provider_did) = {
        let n = node(path);
        let (id, client, _p) = delivered(&n);
        post(
            &n,
            &format!("/v1/contract/{id}/accept-delivery"),
            json!({}),
            &client,
        );
        let (_, act) = route(&n, "GET", "/v1/activity", b"", None);
        let jr = act["jobs"][0]["job_ref"].as_str().unwrap().to_string();
        let pd = act["jobs"][0]["provider"]
            .as_str()
            .map(String::from)
            .unwrap_or_default();
        (jr, pd)
    };

    // Same database, new node: this is the redeploy.
    let n = node(path);

    let (s, job) = route(&n, "GET", &format!("/v1/job/{job_ref}"), b"", None);
    assert_eq!(s, 200, "the job its history links to must resolve: {job}");
    assert_eq!(job["job_ref"], json!(job_ref));

    // And the page a visitor actually clicks.
    let (code, ctype, _) = gap::server::route_html(&n, "GET", &format!("/job/{job_ref}"), None)
        .expect("an entity route must answer rather than fall through");
    assert_eq!(code, 200);
    assert!(ctype.contains("text/html"));

    let _ = provider_did;
    let _ = std::fs::remove_file(path);
}

/// A URL shaped like ours but naming nothing must say so as a page.
///
/// Returning `None` handed the request to the JSON API, so a visitor
/// clicking a link on our own agent page was told the route was
/// unknown. The route was ours; only the record was missing, and the
/// two are not the same thing to whoever is reading.
#[test]
fn a_missing_record_is_a_404_page_not_an_unknown_route() {
    let path = std::env::temp_dir().join(format!("gap-404-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    let n = node(path);

    for (p, kind) in [
        ("/job/0000000000000000", "job"),
        ("/agent/did:gap:nope", "agent"),
        ("/capability/nope", "capability"),
    ] {
        let (code, ctype, body) = gap::server::route_html(&n, "GET", p, None)
            .unwrap_or_else(|| panic!("{p} fell through to the JSON API instead of answering"));
        assert_eq!(code, 404, "{p}");
        assert!(ctype.contains("text/html"), "{p}");
        assert!(
            body.contains(&format!("no {kind} by that name")),
            "{p} must name what was missing"
        );
        // A miss must never be indexed.
        assert!(body.contains("noindex"), "{p}");
    }
    let _ = std::fs::remove_file(path);
}
