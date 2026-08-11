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

/// A score is earned, and a restart must not take it back.
///
/// `Reputation` lives on the in-memory agent identity and the identity
/// table stores only a seed, so every restart rebuilt each agent with
/// an empty counter: score back to the 0.50 prior, `n` back to zero,
/// however many contracts it had settled. The public card said "0.50
/// over 0 verified job(s)" next to a job history that listed the jobs,
/// because the history is persisted and the counter was not.
#[test]
fn a_reputation_survives_a_restart() {
    let path = std::env::temp_dir().join(format!("gap-rep-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);

    let provider_did = {
        let n = node(path);
        let (id, client, _p) = delivered(&n);
        post(
            &n,
            &format!("/v1/contract/{id}/accept-delivery"),
            json!({}),
            &client,
        );
        let (_, act) = route(&n, "GET", "/v1/activity", b"", None);
        assert_eq!(act["count"], json!(1), "a job settled: {act}");

        let (_, c) = route(&n, "GET", &format!("/v1/contract/{id}"), b"", None);
        c["contract"]["provider"]
            .as_str()
            .unwrap_or_else(|| panic!("the contract must name its provider: {c}"))
            .to_string()
    };

    let before = {
        let n = node(path);
        let (_, rep) = route(
            &n,
            "GET",
            &format!("/v1/reputation/{provider_did}"),
            b"",
            None,
        );
        rep
    };

    assert_eq!(
        before["score"]["n"],
        json!(1),
        "the counter must be rebuilt from the job history: {before}"
    );
    assert_eq!(before["jobs"].as_array().map(|j| j.len()), Some(1));
    // One accepted job out of one: Laplace-smoothed that is 2/3, not
    // the 0.50 prior an agent with no history gets.
    let score = before["score"]["success_rate"]
        .as_f64()
        .unwrap_or_else(|| panic!("reputation must carry a score: {before}"));
    assert!(
        score > 0.66 && score < 0.67,
        "a settled job must move the score off the prior, got {score}"
    );

    // The directory card reads the same counter, so if this provider
    // is listed at all it must agree with the reputation page. It only
    // appears once it has announced, which this fixture does not do -
    // hence the conditional rather than a required lookup.
    let n = node(path);
    let (_, dir) = route(&n, "GET", "/v1/discover", b"", None);
    if let Some(me) = dir["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["did"] == json!(provider_did))
    {
        assert_eq!(me["n"], json!(1), "the card must not say 0 verified: {me}");
    }

    let _ = std::fs::remove_file(path);
}

/// A payout must not reduce a balance until the money has left.
///
/// The defect this pins: `withdraw` debited `available`, emitted
/// `pay.withdraw` and returned a receipt quoting a settlement SLA -
/// and nothing anywhere sent anything. No consumer of that event
/// existed in the tree, no relayer call, no payout. The agent's balance
/// fell, the money stayed, and the node's own liabilities figure said
/// it owed less than it did. It had no custody gate either, so it was
/// inert on a non-custodial node only by accident.
#[test]
fn a_payout_request_does_not_pretend_the_money_has_gone() {
    let path = std::env::temp_dir().join(format!("gap-wd-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    let n = node(path);
    custodial(&n);

    let (did, tok) = n.lock().unwrap().create_identity();
    assert_eq!(credit(&n, &did, "5.00").0, 200);

    let dest = "0x00112233445566778899aabbccddeeff00112233";
    let (s, v) = post(
        &n,
        "/v1/balance/withdraw",
        json!({ "amount": "2.00", "destination": dest }),
        &tok,
    );
    assert_eq!(s, 200, "{v}");
    // Not "withdrawn": nothing has moved.
    assert_eq!(v["state"], json!("pending"));
    assert_eq!(v["available"], json!("3.000000"));
    assert_eq!(v["withdrawing"], json!("2.000000"));
    let req = v["request_id"].as_str().unwrap().to_string();

    // The money is still owed, so it is still a liability. This is the
    // assertion the old code could not have passed.
    let (_, res) = route(&n, "GET", "/v1/reserves", b"", None);
    assert_eq!(
        res["liabilities"],
        json!("5.000000"),
        "a pending payout is still owed: {res}"
    );

    // Only an operator can say it went out, and only with a reference.
    let (s, _) = post(
        &n,
        "/v1/balance/withdraw/settle",
        json!({ "request_id": req, "reference": "0xdead" }),
        &tok,
    );
    assert_eq!(s, 401, "an agent must not settle its own payout");
    let (s, v) = post(
        &n,
        "/v1/balance/withdraw/settle",
        json!({ "request_id": req }),
        "operator-token",
    );
    assert_ne!(
        s, 200,
        "a payout with no external reference is unverifiable: {v}"
    );

    let (s, v) = post(
        &n,
        "/v1/balance/withdraw/settle",
        json!({ "request_id": req, "reference": "0xabc123" }),
        "operator-token",
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], json!("settled"));

    // Now, and only now, the node owes less.
    let (_, res) = route(&n, "GET", "/v1/reserves", b"", None);
    assert_eq!(res["liabilities"], json!("3.000000"), "{res}");

    // And it cannot be settled twice.
    let (s, _) = post(
        &n,
        "/v1/balance/withdraw/settle",
        json!({ "request_id": req, "reference": "0xabc123" }),
        "operator-token",
    );
    assert_ne!(s, 200, "paying the same request twice pays twice");

    let _ = std::fs::remove_file(path);
}

/// A payout instruction pointing nowhere must be refused.
#[test]
fn a_payout_destination_is_validated() {
    let path = std::env::temp_dir().join(format!("gap-wd2-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    let n = node(path);
    custodial(&n);
    let (did, tok) = n.lock().unwrap().create_identity();
    assert_eq!(credit(&n, &did, "5.00").0, 200);

    // The empty string was accepted before, because the route read the
    // field with unwrap_or("").
    for bad in [
        "",
        "   ",
        "not-an-address",
        &did,
        "0x1234",
        "0xZZ112233445566778899aabbccddeeff00112233",
    ] {
        let (s, v) = post(
            &n,
            "/v1/balance/withdraw",
            json!({ "amount": "1.00", "destination": bad }),
            &tok,
        );
        assert_ne!(s, 200, "destination {bad:?} must be refused: {v}");
    }
    // And nothing was taken while refusing.
    let (_, b) = route(
        &n,
        "GET",
        "/v1/balance",
        b"",
        Some(&format!("Bearer {tok}")),
    );
    let me = b["balances"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|x| x["agent_did"] == json!(did))
        .cloned()
        .unwrap_or(b.clone());
    assert!(
        me.to_string().contains("5.000000"),
        "a refused payout must not move money: {me}"
    );

    let _ = std::fs::remove_file(path);
}

/// A non-custodial node holds nothing, so it can pay nothing out.
#[test]
fn a_non_custodial_node_refuses_payouts_by_design_not_by_accident() {
    let path = std::env::temp_dir().join(format!("gap-wd3-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    // No `custodial()` call: this is the default the live node runs in.
    let n = node(path);
    let (_did, tok) = n.lock().unwrap().create_identity();

    let (s, v) = post(
        &n,
        "/v1/balance/withdraw",
        json!({ "amount": "1.00", "destination": "0x00112233445566778899aabbccddeeff00112233" }),
        &tok,
    );
    assert_ne!(s, 200, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("non-custodial"),
        "it must refuse for the right reason, not because the map happens to be empty: {v}"
    );

    let _ = std::fs::remove_file(path);
}

/// A failed payout gives the money back rather than stranding it.
#[test]
fn a_cancelled_payout_returns_the_funds() {
    let path = std::env::temp_dir().join(format!("gap-wd4-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    let n = node(path);
    custodial(&n);
    let (did, tok) = n.lock().unwrap().create_identity();
    assert_eq!(credit(&n, &did, "4.00").0, 200);

    let (_, v) = post(
        &n,
        "/v1/balance/withdraw",
        json!({ "amount": "4.00", "destination": "0x00112233445566778899aabbccddeeff00112233" }),
        &tok,
    );
    let req = v["request_id"].as_str().unwrap().to_string();

    let (s, v) = post(
        &n,
        "/v1/balance/withdraw/cancel",
        json!({ "request_id": req, "reason": "address rejected by the exchange" }),
        "operator-token",
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], json!("cancelled"));

    // Spendable again, and still exactly what it was.
    let (_, v2) = post(
        &n,
        "/v1/balance/withdraw",
        json!({ "amount": "4.00", "destination": "0x00112233445566778899aabbccddeeff00112233" }),
        &tok,
    );
    assert_eq!(v2["available"], json!("0.000000"));
    assert_eq!(v2["withdrawing"], json!("4.000000"));

    let _ = std::fs::remove_file(path);
}

/// A payout somebody is owed must survive a restart.
#[test]
fn a_pending_payout_survives_a_restart() {
    let path = std::env::temp_dir().join(format!("gap-wd5-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);

    let (did, req) = {
        let n = node(path);
        custodial(&n);
        let (did, tok) = n.lock().unwrap().create_identity();
        assert_eq!(credit(&n, &did, "3.00").0, 200);
        let (_, v) = post(
            &n,
            "/v1/balance/withdraw",
            json!({ "amount": "3.00", "destination": "0x00112233445566778899aabbccddeeff00112233" }),
            &tok,
        );
        (did, v["request_id"].as_str().unwrap().to_string())
    };

    // Same spine, new node: a queue that evaporates is a debt that
    // evaporates with it.
    let n = node(path);
    custodial(&n);
    let (s, q) = route(
        &n,
        "GET",
        "/v1/balance/withdrawals",
        b"",
        Some("Bearer operator-token"),
    );
    assert_eq!(s, 200, "{q}");
    assert_eq!(q["count"], json!(1), "the payout queue must survive: {q}");
    assert_eq!(q["withdrawals"][0]["request_id"], json!(req));
    assert_eq!(q["withdrawals"][0]["agent_did"], json!(did));

    // The queue names agents and destinations, so it is not public.
    let (s, _) = route(&n, "GET", "/v1/balance/withdrawals", b"", None);
    assert_eq!(s, 403);

    let _ = std::fs::remove_file(path);
}

/// A withdrawal a redeploy undoes is not a withdrawal.
///
/// `deregister` dropped the announcement from the in-memory registry
/// and left the stored row untouched, so the next restart hydrated it
/// again and the agent was back in the public directory. Observed, not
/// hypothesised: an agent delisted by hand reappeared on /agents when a
/// push triggered a redeploy an hour later.
#[test]
fn a_deregistration_survives_a_restart() {
    let path = std::env::temp_dir().join(format!("gap-dereg-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);

    let did = {
        let n = node(path);
        let (did, tok) = n.lock().unwrap().create_identity();
        let (s, v) = post(
            &n,
            "/v1/announce",
            json!({ "capabilities": [{
                "id": "cap:test", "name": "test-only", "description": "a throwaway",
                "price": { "amount": 0.01, "currency": "USDC", "model": "fixed", "cap": 1.0 }
            }]}),
            &tok,
        );
        assert_eq!(s, 200, "announce failed: {v}");

        let (_, dir) = route(&n, "GET", "/v1/discover", b"", None);
        assert!(dir.to_string().contains(&did), "it announced: {dir}");

        let (s, v) = post(&n, "/v1/deregister", json!({}), &tok);
        assert_eq!(s, 200, "{v}");
        let (_, dir) = route(&n, "GET", "/v1/discover", b"", None);
        assert!(!dir.to_string().contains(&did), "gone in memory: {dir}");
        did
    };

    // Same spine, new node: this is the redeploy that used to undo it.
    let n = node(path);
    let (_, dir) = route(&n, "GET", "/v1/discover", b"", None);
    assert!(
        !dir.to_string().contains(&did),
        "a deregistered agent must not come back on restart: {dir}"
    );

    // The history stays reachable under the DID: records outlive an
    // announcement, and that is deliberate - otherwise a provider could
    // erase a bad record by withdrawing and re-announcing.
    let (s, rep) = route(&n, "GET", &format!("/v1/reputation/{did}"), b"", None);
    assert_eq!(s, 200, "the track record survives delisting: {rep}");

    let _ = std::fs::remove_file(path);
}

/// Withdrawing must not be a one-way door.
#[test]
fn an_agent_can_announce_again_after_deregistering() {
    let path = std::env::temp_dir().join(format!("gap-rereg-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);

    let (did, tok) = {
        let n = node(path);
        let (did, tok) = n.lock().unwrap().create_identity();
        let ann = json!({ "capabilities": [{
            "id": "cap:test", "name": "test-only", "description": "d",
            "price": { "amount": 0.01, "currency": "USDC", "model": "fixed", "cap": 1.0 }
        }]});
        post(&n, "/v1/announce", ann.clone(), &tok);
        post(&n, "/v1/deregister", json!({}), &tok);
        // Announce again on the same node.
        let (s, v) = post(&n, "/v1/announce", ann, &tok);
        assert_eq!(s, 200, "re-announcing must work: {v}");
        (did, tok)
    };
    let _ = tok;

    // And the second announcement must outrank the tombstone across a
    // restart. A tombstone written at u64::MAX would win forever and
    // lock the agent out of its own directory entry.
    let n = node(path);
    let (_, dir) = route(&n, "GET", "/v1/discover", b"", None);
    assert!(
        dir.to_string().contains(&did),
        "the new announcement must survive the tombstone: {dir}"
    );

    let _ = std::fs::remove_file(path);
}

/// The spine must actually be a chain, not a list that says it is one.
///
/// The product claimed "every receipt is hash-chained" and "tamper
/// evident" in three places on the public site while `EventRecord` had
/// no link field at all and `receipt_chain.rs` was called from nowhere.
/// A sequence number is not tamper evidence: deleting or editing a row
/// left no cryptographic trace whatsoever.
#[test]
fn the_audit_spine_is_hash_chained_and_verifiable() {
    let path = std::env::temp_dir().join(format!("gap-chain-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    let n = node(path);

    let (id, client, _p) = delivered(&n);
    post(
        &n,
        &format!("/v1/contract/{id}/accept-delivery"),
        json!({}),
        &client,
    );

    let (s, v) = route(&n, "GET", "/v1/audit/verify", b"", None);
    assert_eq!(s, 200, "verification must be public: {v}");
    assert_eq!(v["intact"], json!(true), "{v}");
    assert!(v["links_verified"].as_u64().unwrap() > 4, "{v}");
    assert_eq!(v["broken_at_seq"], json!(null));
    assert_eq!(
        v["unchained_prefix"],
        json!(0),
        "a fresh node has no legacy: {v}"
    );
    assert!(v["tip_hash"].as_str().unwrap().starts_with("sha256:"));

    // Each link must name its predecessor, and the rule must be
    // recomputable by a stranger from what /v1/audit publishes.
    let (_, audit) = route(
        &n,
        "GET",
        "/v1/audit",
        b"",
        Some(&format!("Bearer {client}")),
    );
    let events = audit["events"].as_array().unwrap();
    assert!(events.len() > 4);
    let mut prev = String::new();
    for e in events {
        let recomputed = gap::storage::event_hash(
            e["seq"].as_u64().unwrap(),
            e["kind"].as_str().unwrap(),
            e["at"].as_u64().unwrap(),
            &e["payload"],
            &prev,
        );
        assert_eq!(
            e["hash"].as_str().unwrap(),
            recomputed,
            "seq {} does not hash to what it publishes",
            e["seq"]
        );
        assert_eq!(e["prev_hash"].as_str().unwrap_or(""), prev);
        prev = recomputed;
    }

    let _ = std::fs::remove_file(path);
}

/// Editing history must break the chain, or the chain is decoration.
#[test]
fn tampering_with_a_stored_event_is_detected() {
    let path = std::env::temp_dir().join(format!("gap-tamper-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);

    {
        let n = node(path);
        let (id, client, _p) = delivered(&n);
        post(
            &n,
            &format!("/v1/contract/{id}/accept-delivery"),
            json!({}),
            &client,
        );
        let (_, v) = route(&n, "GET", "/v1/audit/verify", b"", None);
        assert_eq!(v["intact"], json!(true));
    }

    // Someone with database access rewrites a payload. This is exactly
    // the attack the claim is about, and before the chain existed it
    // was completely undetectable.
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute(
        "UPDATE events SET payload = '{\"contract_id\":\"forged\"}' WHERE seq = 3",
        [],
    )
    .unwrap();
    drop(conn);

    let n = node(path);
    let (_, v) = route(&n, "GET", "/v1/audit/verify", b"", None);
    assert_eq!(v["intact"], json!(false), "tampering must be caught: {v}");
    assert_eq!(v["breaks_at_seq"], json!([3]), "and named: {v}");
    // Verification continues past the break: the stretches either side
    // are still worth something, and reporting only the first failure
    // would let one edit hide the state of everything after it.
    assert!(
        !v["segments"].as_array().unwrap().is_empty(),
        "the unbroken stretches must still be reported: {v}"
    );

    let _ = std::fs::remove_file(path);
}

/// A node whose history predates the chain must say so, not pretend.
#[test]
fn events_written_before_the_chain_are_declared_not_backfilled() {
    let path = std::env::temp_dir().join(format!("gap-legacy-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);

    {
        let n = node(path);
        let _ = delivered(&n);
    }
    // Strip the chain from the existing rows: this is what a database
    // written by the previous version looks like.
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute("UPDATE events SET hash = '', prev_hash = ''", [])
        .unwrap();
    drop(conn);

    let n = node(path);
    let (did, tok) = n.lock().unwrap().create_identity();
    let _ = did;
    post(&n, "/v1/deregister", json!({}), &tok);

    let (_, v) = route(&n, "GET", "/v1/audit/verify", b"", None);
    // The new events chain; the old ones are counted and excluded.
    assert_eq!(v["intact"], json!(true), "{v}");
    assert!(v["unchained_prefix"].as_u64().unwrap() > 0, "{v}");
    let first = &v["segments"][0];
    assert!(
        first["from_seq"].as_u64().unwrap() > 1,
        "it must say where the chain begins: {v}"
    );

    let _ = std::fs::remove_file(path);
}

/// The declared conformance level must match what the binary can do.
///
/// A hand-set level is exactly the kind of claim that stays true in the
/// config long after it stopped being true in the code - which is how
/// the site ended up promising a hash-chained spine it did not have.
/// So the areas are checked against the router, not trusted.
#[test]
fn conformance_areas_match_reality() {
    let path = std::env::temp_dir().join(format!("gap-conf-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    let n = node(path);

    let (s, c) = route(&n, "GET", "/v1/conformance", b"", None);
    assert_eq!(s, 200, "a node must say what it speaks: {c}");

    // Every area claimed as served must have a live route behind it.
    let probes: &[(&str, &str, &str)] = &[
        ("identity", "POST", "/v1/identity"),
        ("discovery", "GET", "/v1/discover"),
        ("agentcard", "GET", "/.well-known/gap-agent.json"),
        ("receipt_chain", "GET", "/v1/audit/verify"),
    ];
    let served: Vec<&str> = c["why"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["served"] == json!(true))
        .map(|a| a["area"].as_str().unwrap())
        .collect();
    for (area, method, p) in probes {
        assert!(served.contains(area), "{area} should be served");
        let (code, _) = route(&n, method, p, b"{}", None);
        assert_ne!(
            code, 400,
            "{area} is claimed but {method} {p} is not routed"
        );
    }

    // And the three that are NOT served must stay declared false while
    // their modules remain unreachable. This test fails the day someone
    // wires one and forgets to say so - which is the point.
    for unreachable in ["policy", "delegation", "compliance"] {
        assert!(
            !served.contains(&unreachable),
            "{unreachable} is declared served: wire it or do not claim it"
        );
    }

    // Which caps the node at L2, honestly.
    assert_eq!(c["level"], json!("L2"), "{c}");
    assert_eq!(c["missing_for_next_level"], json!(["policy"]));

    // The report is signed by the node, so the claim is attributable.
    assert!(c["sig"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(c["suite_version"], json!("self-declared"));

    let _ = std::fs::remove_file(path);
}

/// Spawning sub-agents must not buy more shelf space (RFC-0007).
///
/// The landing promises "delegation-tree aggregation, one bid per
/// tree". Nothing enforced it: src/sybil.rs had tree_root,
/// is_sub_agent, enforce_restricted and TreeBucket, and server.rs
/// imported exactly one thing from it - the rate counter struct - so
/// the Sybil defences were unreachable code behind a public claim.
#[test]
fn a_delegation_tree_gets_one_directory_entry_not_one_per_agent() {
    use gap::delegation::{Budget, DelegationToken, Mandate, TokenChain};

    let path = std::env::temp_dir().join(format!("gap-sybil-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    let n = node(path);

    let announce = |tok: &str, id: &str| {
        post(
            &n,
            "/v1/announce",
            json!({ "capabilities": [{
                "id": id, "name": "flood", "description": "d",
                "price": { "amount": 0.01, "currency": "USDC", "model": "fixed", "cap": 1.0 }
            }]}),
            tok,
        )
    };

    // The principal, and two sub-agents it delegates to.
    let (root_did, root_tok) = n.lock().unwrap().create_identity();
    let root_identity = n.lock().unwrap().agent_identity_for(&root_did).unwrap();
    announce(&root_tok, "cap:a");

    let mut subs = vec![];
    for i in 0..2 {
        let (sub_did, sub_tok) = n.lock().unwrap().create_identity();
        let mandate = Mandate {
            capabilities: vec!["cap:a".into()],
            budget: Budget::default(),
            autonomy_level: "propose".into(),
            jurisdictions: vec![],
            channels: vec![],
            expires_at: gap::message::now_unix() + 3600,
            mode: "standing".into(),
        };
        let root_id = gap::identity::Did::parse(&root_did).unwrap();
        let token = DelegationToken::issue(
            &root_identity,
            gap::identity::Did::parse(&sub_did).unwrap(),
            root_id,
            "urn:gap:dlg:0".into(),
            mandate,
        );
        let mut chain = TokenChain::default();
        chain.push(token).unwrap();

        announce(&sub_tok, &format!("cap:sub{i}"));
        let (s, v) = post(&n, "/v1/delegation", json!({ "chain": chain }), &sub_tok);
        assert_eq!(s, 200, "registering a chain must work: {v}");
        assert_eq!(v["tree_root"], json!(root_did), "{v}");
        subs.push(sub_did);
    }

    // Three agents announced. One tree, so one entry.
    let (_, dir) = route(&n, "GET", "/v1/directory", b"", None);
    let listed: Vec<&str> = dir["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["did"].as_str().unwrap())
        .collect();
    assert_eq!(
        listed.len(),
        1,
        "three agents of one tree must get one entry, got {listed:?}"
    );
    assert_eq!(
        listed[0], root_did,
        "the tree is represented by its root here"
    );
    for sub in &subs {
        assert!(!listed.contains(&sub.as_str()), "{sub} should be collapsed");
    }

    let _ = std::fs::remove_file(path);
}

/// A chain that delegates to somebody else must be refused.
#[test]
fn an_agent_cannot_file_itself_under_a_strangers_tree() {
    use gap::delegation::{Budget, DelegationToken, Mandate, TokenChain};

    let path = std::env::temp_dir().join(format!("gap-sybil2-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    let n = node(path);

    let (root_did, _root_tok) = n.lock().unwrap().create_identity();
    let root_identity = n.lock().unwrap().agent_identity_for(&root_did).unwrap();
    let (victim_did, _) = n.lock().unwrap().create_identity();
    let (_attacker_did, attacker_tok) = n.lock().unwrap().create_identity();

    // A perfectly valid chain - for somebody else.
    let mandate = Mandate {
        capabilities: vec!["cap:a".into()],
        budget: Budget::default(),
        autonomy_level: "propose".into(),
        jurisdictions: vec![],
        channels: vec![],
        expires_at: gap::message::now_unix() + 3600,
        mode: "standing".into(),
    };
    let token = DelegationToken::issue(
        &root_identity,
        gap::identity::Did::parse(&victim_did).unwrap(),
        gap::identity::Did::parse(&root_did).unwrap(),
        "urn:gap:dlg:0".into(),
        mandate,
    );
    let mut chain = TokenChain::default();
    chain.push(token).unwrap();

    let (s, v) = post(
        &n,
        "/v1/delegation",
        json!({ "chain": chain }),
        &attacker_tok,
    );
    assert_ne!(
        s, 200,
        "presenting a chain that delegates to someone else must be refused: {v}"
    );

    let _ = std::fs::remove_file(path);
}

/// A float in a payload must not break the chain.
///
/// serde_json is round-trip correct on VALUES without being byte-stable
/// on STRINGS: 0.9090909090909091 parses to a double whose shortest
/// representation is ...092. Both denote the same number, so nothing is
/// wrong - but the spine hashed a document it re-parses on the next
/// boot, so the hash written and the hash recomputed differed. A
/// reputation score is a float, so node.reputation.update broke the
/// live chain while every value in it was correct.
#[test]
fn a_float_payload_survives_a_restart_with_its_link_intact() {
    let path = std::env::temp_dir().join(format!("gap-float-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);

    {
        let n = node(path);
        // Settling produces node.reputation.update, whose payload
        // carries the smoothed score - the exact shape that broke.
        let (id, client, _p) = delivered(&n);
        post(
            &n,
            &format!("/v1/contract/{id}/accept-delivery"),
            json!({}),
            &client,
        );
        let (_, v) = route(&n, "GET", "/v1/audit/verify", b"", None);
        assert_eq!(v["intact"], json!(true), "intact when freshly written: {v}");
    }

    // The restart is what used to reveal it: the chain verified in the
    // process that wrote it and failed in the next one.
    let n = node(path);
    let (_, v) = route(&n, "GET", "/v1/audit/verify", b"", None);
    assert_eq!(
        v["intact"],
        json!(true),
        "a float payload must still verify after a reload: {v}"
    );
    assert!(v["links_verified"].as_u64().unwrap() > 4, "{v}");

    let _ = std::fs::remove_file(path);
}

/// A contract's history must not depend on how old the node is.
#[test]
fn a_contract_shows_its_events_however_busy_the_node_has_been() {
    let path = std::env::temp_dir().join(format!("gap-evwin-{}.db", std::process::id()));
    let path = path.to_str().unwrap();
    let _ = std::fs::remove_file(path);
    let n = node(path);

    // Push the spine past the old hard-coded window of 100 events.
    for _ in 0..30 {
        let (id, client, _p) = delivered(&n);
        post(
            &n,
            &format!("/v1/contract/{id}/accept-delivery"),
            json!({}),
            &client,
        );
    }
    let (id, client, _p) = delivered(&n);
    post(
        &n,
        &format!("/v1/contract/{id}/accept-delivery"),
        json!({}),
        &client,
    );

    let (s, c) = route(&n, "GET", &format!("/v1/contract/{id}"), b"", None);
    assert_eq!(s, 200);
    let events = c["events"].as_array().unwrap();
    assert!(
        !events.is_empty(),
        "the events are in storage; the endpoint scanned only the first 100 ever written: {c}"
    );
    assert!(
        events
            .iter()
            .all(|e| e["payload"]["contract_id"] == json!(id)),
        "and every one must belong to this contract"
    );

    let _ = std::fs::remove_file(path);
}
