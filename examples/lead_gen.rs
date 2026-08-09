//! End-to-end example: two agents discover each other, negotiate a
//! contract, execute with proof, and settle through escrow.
//!
//! Run with: `cargo run --example lead_gen`

use gap::contract::{Contract, ContractState, Price, Terms};
use gap::discovery::{Announcement, Capability, Query, Reachability, Registry};
use gap::execution::{ProofBundle, Step};
use gap::identity::AgentIdentity;
use gap::message::{now_unix, Envelope, Kind};
use gap::payment::Escrow;
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== GAP v{} — end-to-end agent economy ===\n", gap::VERSION);

    // --- Setup: three identities ---
    let client = AgentIdentity::generate(); // buys leads
    let mut provider = AgentIdentity::generate(); // sells leads
    let escrow_agent = AgentIdentity::generate(); // neutral escrow
    let escrow_did = escrow_agent.did().clone();
    println!("client    {}", client.did());
    println!("provider  {}", provider.did());
    println!("escrow    {}", escrow_agent.did());

    // --- 1. Discovery: provider announces, client queries ---
    println!("\n[1] DISCOVERY");
    let mut registry = Registry::new();
    let cap = Capability {
        id: "cap:getateam:sales:lead-gen".into(),
        name: "lead-generation".into(),
        description: "Generate and qualify sales leads from inbound channels".into(),
        input: serde_json::json!({}),
        output: serde_json::json!({}),
        price: Some(gap::discovery::Price {
            amount: 0.05,
            currency: "EUR".into(),
            model: "per_unit".into(),
        }),
        autonomy: vec!["propose".into(), "execute-notify".into()],
    };
    let ann = Announcement::signed(
        &provider,
        vec![cap],
        vec![Reachability {
            transport: "https".into(),
            endpoint: "https://provider.example/gap".into(),
        }],
        3600,
    );
    ann.verify()?;
    registry.announce(ann)?;
    println!(
        "announcement verified & registered ({} live)",
        registry.len()
    );

    let hits = registry.query(&Query {
        name: Some("lead-generation".into()),
        ..Default::default()
    });
    println!("query found {} provider(s)", hits.len());

    // --- 2. Negotiation: propose -> accept ---
    println!("\n[2] CONTRACT NEGOTIATION");
    let terms = Terms {
        input: serde_json::json!({ "budget": 50.0 }),
        deliverable: serde_json::json!({ "type": "array", "min": 10 }),
        acceptance_criteria: vec![
            "each lead has a verified email".into(),
            "no duplicates".into(),
        ],
        deadline: now_unix() + 86400,
        price: Price {
            amount: 0.05,
            currency: "EUR".into(),
            model: "per_unit".into(),
            cap: Some(100.0),
        },
        autonomy: "execute-notify".into(),
        confidentiality: None,
        human_review_above: None,
    };
    let contract = Contract::propose(
        &client,
        provider.did().clone(),
        "cap:getateam:sales:lead-gen",
        terms,
        true,
    )
    .accept_by_provider(&provider)?;
    contract.verify_signed()?;
    assert_eq!(contract.state, ContractState::Signed);
    println!("contract signed by both parties: {}", contract.contract_id);

    // --- 3. Execution with proof bundle ---
    println!("\n[3] EXECUTION");
    let payload = b"lead: alice@example.com, verified\nlead: bob@example.com, verified";
    let bundle = ProofBundle::signed(
        &provider,
        &contract.contract_id,
        payload,
        vec![Step {
            index: 1,
            description: "scanned inbound queue".into(),
            proof: None,
            ts: now_unix(),
        }],
    );
    bundle.verify(&provider, payload)?;
    println!(
        "delivery verified: {} ({} bytes)",
        bundle.deliverable_hash,
        payload.len()
    );

    // --- 4. Settlement through escrow (signed instructions only) ---
    println!("\n[4] PAYMENT (escrow)");
    let mut escrow = Escrow::for_contract(escrow_agent, contract.clone())?;
    let total = 10.0; // 200 leads * 0.05 capped

    // The client signs a pay.park instruction referencing the contract.
    let park_instruction = Envelope::new(
        client.did().clone(),
        escrow_did.clone(),
        Kind::PayPark,
        json!({ "amount": total }),
    )
    .for_contract(contract.contract_id.clone())
    .sign(&client);
    let _park = escrow.park(&park_instruction)?;

    // The client signs an exe.accept, then a pay.release instruction.
    let acceptance = Envelope::new(
        client.did().clone(),
        provider.did().clone(),
        Kind::ExeAccept,
        json!({ "verdict": "accepted" }),
    )
    .for_contract(contract.contract_id.clone())
    .sign(&client);
    let release_instruction = Envelope::new(
        client.did().clone(),
        escrow_did.clone(),
        Kind::PayRelease,
        json!({}),
    )
    .for_contract(contract.contract_id.clone())
    .sign(&client);
    let _release = escrow.release(&release_instruction, &acceptance)?;
    println!("escrow: parked {total:.2} EUR, released {total:.2} EUR");
    println!("audit trail: {} signed receipts", escrow.audit_log().len());

    // --- 5. Reputation ---
    println!("\n[5] REPUTATION");
    provider.reputation_mut().record(true, true);
    println!(
        "provider success rate: {:.2} (raw {:.2}, n={})",
        provider.reputation().success_rate(),
        provider.reputation().raw_success_rate(),
        provider.reputation().executions
    );

    println!("\n=== GAP flow completed successfully ===");
    Ok(())
}
