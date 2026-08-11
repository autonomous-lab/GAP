//! Integration test: a full agent-economy scenario with dispute and
//! arbitration, exercising every protocol layer together.
//!
//! Scenario: a marketing agent (client) buys lead-generation from a sales
//! agent (provider). The provider delivers late and with a hash mismatch,
//! the client disputes, an arbitrator rules, and the escrow settles.

use gap::contract::{Contract, ContractState, Price, Terms};
use gap::discovery::{Announcement, Capability, Query, Reachability, Registry};
use gap::error::{Error, Result};
use gap::execution::{ProofBundle, Step};
use gap::identity::AgentIdentity;
use gap::message::{now_unix, Envelope, Kind};
use gap::payment::{Escrow, EscrowState};
use serde_json::json;

struct World {
    client: AgentIdentity,
    provider: AgentIdentity,
    escrow_agent: AgentIdentity,
    arbitrator: AgentIdentity,
    registry: Registry,
}

fn build_world() -> World {
    World {
        client: AgentIdentity::generate(),
        provider: AgentIdentity::generate(),
        escrow_agent: AgentIdentity::generate(),
        arbitrator: AgentIdentity::generate(),
        registry: Registry::new(),
    }
}

fn signed_terms(deadline_offset: u64) -> Terms {
    Terms {
        input: json!({ "budget": 100.0 }),
        deliverable: json!({ "type": "array", "min": 10 }),
        acceptance_criteria: vec![
            "each lead has a verified email".into(),
            "no duplicates".into(),
        ],
        deadline: now_unix() + deadline_offset,
        price: Price {
            amount: 0.05,
            currency: "EUR".into(),
            model: "per_unit".into(),
            cap: Some(10.0),
        },
        autonomy: "execute-notify".into(),
        confidentiality: None,
        human_review_above: None,
        cooling_off_seconds: None,
    }
}

fn park(client: &AgentIdentity, escrow: &Escrow, contract: &gap::contract::Contract) -> Envelope {
    Envelope::new(
        client.did().clone(),
        escrow.did().clone(),
        Kind::PayPark,
        json!({ "amount": 10.0 }),
    )
    .for_contract(contract.contract_id.clone())
    .sign(client)
}

#[test]
fn happy_path_full_economy() -> Result<()> {
    let mut world = build_world();

    // 1. Discovery
    let cap = Capability {
        id: "cap:getateam:sales:lead-gen".into(),
        name: "lead-generation".into(),
        description: "qualify leads".into(),
        input: json!({}),
        output: json!({}),
        price: Some(gap::discovery::Price {
            amount: 0.05,
            currency: "EUR".into(),
            model: "per_unit".into(),
        }),
        autonomy: vec!["propose".into(), "execute-notify".into()],
    };
    let ann = Announcement::signed(
        &world.provider,
        vec![cap],
        vec![Reachability {
            transport: "https".into(),
            endpoint: "https://provider.example/gap".into(),
        }],
        3600,
    );
    world.registry.announce(ann)?;
    let hits = world.registry.query(&Query {
        name: Some("lead-generation".into()),
        ..Default::default()
    });
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].agent_did, *world.provider.did());

    // 2. Negotiation + signing
    let contract = Contract::propose(
        &world.client,
        world.provider.did().clone(),
        "cap:getateam:sales:lead-gen",
        signed_terms(3600),
        true,
    )
    .accept_by_provider(&world.provider)?;
    contract.verify_signed()?;
    assert_eq!(contract.state, ContractState::Signed);

    // 3. Execution with a valid proof bundle
    let payload = b"lead: alice@example.com, verified\nlead: bob@example.com, verified";
    let bundle = ProofBundle::signed(
        &world.provider,
        &contract.contract_id,
        payload,
        vec![Step {
            index: 1,
            description: "scanned inbound queue".into(),
            proof: None,
            ts: now_unix(),
        }],
    );
    bundle.verify(&world.provider, payload)?;

    // 4. Escrow: park, accept, release (one escrow per contract)
    let mut escrow = Escrow::for_contract(world.escrow_agent.clone(), contract.clone())?;
    escrow.park(&park(&world.client, &escrow, &contract))?;
    assert_eq!(escrow.state(), EscrowState::Parked);

    let acceptance = Envelope::new(
        world.client.did().clone(),
        world.provider.did().clone(),
        Kind::ExeAccept,
        json!({ "verdict": "accepted" }),
    )
    .for_contract(contract.contract_id.clone())
    .sign(&world.client);
    let release = Envelope::new(
        world.client.did().clone(),
        world.escrow_agent.did().clone(),
        Kind::PayRelease,
        json!({}),
    )
    .for_contract(contract.contract_id.clone())
    .sign(&world.client);
    escrow.release(&release, &acceptance)?;
    assert_eq!(escrow.state(), EscrowState::Released);
    assert_eq!(escrow.audit_log().len(), 2);

    // 5. Reputation reflects the success (raw 1.0; the smoothed
    // estimator stays below 1.0 by design — one success is not proof).
    world.provider.reputation_mut().record(true, true);
    assert_eq!(world.provider.reputation().raw_success_rate(), 1.0);
    assert!(world.provider.reputation().success_rate() > 0.5);
    Ok(())
}

#[test]
fn disputed_delivery_is_arbitrated_and_ruled() -> Result<()> {
    let mut world = build_world();

    let contract = Contract::propose(
        &world.client,
        world.provider.did().clone(),
        "cap:getateam:sales:lead-gen",
        signed_terms(3600),
        true,
    )
    .accept_by_provider(&world.provider)?;

    // Provider delivers a payload that does NOT match the accepted
    // deliverable (wrong content — hash mismatch detected by client).
    let claimed = b"lead: alice@example.com, verified";
    let bundle = ProofBundle::signed(&world.provider, &contract.contract_id, claimed, vec![]);
    // The client re-checks against what was actually received.
    let received = b"lead: mallory@example.com, NOT verified";
    assert!(bundle.verify(&world.provider, received).is_err());

    // Client disputes; funds are parked and held.
    let mut escrow = Escrow::for_contract(world.escrow_agent.clone(), contract.clone())?;
    escrow.park(&park(&world.client, &escrow, &contract))?;
    let dispute = Envelope::new(
        world.client.did().clone(),
        world.escrow_agent.did().clone(),
        Kind::PayDispute,
        json!({ "reason": "nonconforming" }),
    )
    .for_contract(contract.contract_id.clone())
    .sign(&world.client);
    escrow.dispute(&dispute)?;
    assert_eq!(escrow.state(), EscrowState::Disputed);

    // Arbitrator rules: provider at fault -> full refund to client.
    let ruling = Envelope::new(
        world.arbitrator.did().clone(),
        world.escrow_agent.did().clone(),
        Kind::CtrRuling,
        json!({ "verdict": "provider_at_fault", "split": { "client": 1.0, "provider": 0.0 } }),
    )
    .for_contract(contract.contract_id.clone())
    .sign(&world.arbitrator);
    // The escrow executes the ruling: full share to the client.
    let ruled = escrow.rule(&ruling, world.arbitrator.did())?;
    assert_eq!(ruled.event, "pay.ruled");
    assert_eq!(escrow.state(), EscrowState::Ruled);
    assert_eq!(escrow.held(), gap::amount::Amount::ZERO);

    // A ruling from an unapproved arbitrator is rejected.
    let impostor = AgentIdentity::generate();
    let fake_ruling = Envelope::new(
        impostor.did().clone(),
        world.escrow_agent.did().clone(),
        Kind::CtrRuling,
        json!({ "verdict": "provider_at_fault", "split": { "client": 1.0, "provider": 0.0 } }),
    )
    .for_contract(contract.contract_id.clone())
    .sign(&impostor);
    // Need a fresh disputed state for the attempt.
    let world2 = build_world();
    let contract2 = Contract::propose(
        &world2.client,
        world2.provider.did().clone(),
        "cap:getateam:sales:lead-gen",
        signed_terms(3600),
        true,
    )
    .accept_by_provider(&world2.provider)?;
    let mut escrow2 = Escrow::for_contract(world2.escrow_agent.clone(), contract2.clone())?;
    escrow2.park(&park(&world2.client, &escrow2, &contract2))?;
    let dispute2 = Envelope::new(
        world2.client.did().clone(),
        world2.escrow_agent.did().clone(),
        Kind::PayDispute,
        json!({ "reason": "nonconforming" }),
    )
    .for_contract(contract2.contract_id.clone())
    .sign(&world2.client);
    escrow2.dispute(&dispute2)?;
    assert!(escrow2.rule(&fake_ruling, world2.arbitrator.did()).is_err());

    // Provider's reputation takes a hit (attested by the ruling).
    world.provider.reputation_mut().record(false, false);
    assert!(world.provider.reputation().success_rate() < 1.0);
    Ok(())
}

#[test]
fn replay_attack_is_rejected() -> Result<()> {
    let world = build_world();
    let env = Envelope::new(
        world.client.did().clone(),
        world.provider.did().clone(),
        Kind::ExeAccept,
        json!({}),
    )
    .sign(&world.client);
    // Fresh message passes validation.
    assert!(env.validate(300).is_ok());
    // Old message fails (replay).
    let mut old = env;
    old.timestamp = now_unix().saturating_sub(10_000);
    let old = old.sign(&world.client);
    assert!(matches!(old.validate(300), Err(Error::StaleTimestamp)));

    // A replay INSIDE the freshness window is caught by the guard.
    let mut guard = gap::message::ReplayGuard::new();
    let fresh = Envelope::new(
        world.client.did().clone(),
        world.provider.did().clone(),
        Kind::ExeAccept,
        json!({}),
    )
    .sign(&world.client);
    assert!(guard.check(&fresh, 300).is_ok());
    assert!(matches!(
        guard.check(&fresh, 300),
        Err(Error::ReplayedMessage(_))
    ));
    Ok(())
}
