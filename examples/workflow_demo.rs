//! End-to-end multi-agent workflow demo.
//!
//! Demonstrates the full GAP stack working together:
//!
//! 1. An orchestrator agent defines a 3-step workflow (RFC-0002):
//!    scrape → analyze → publish.
//! 2. Agents discover each other via the registry (part 02).
//! 3. Each step becomes a signed contract (part 03) with escrow
//!    settlement (part 05).
//! 4. The orchestrator acts through a delegation chain (RFC-0001).
//! 5. Every event is persisted to SQLite (storage layer).
//!
//! Run with: `cargo run --example workflow_demo`

use gap::contract::{Contract, ContractState, Price, Terms};
use gap::delegation::{Budget, DelegationToken, Mandate};
use gap::discovery::{Announcement, Capability, Query, Reachability, Registry};
use gap::error::Result;
use gap::identity::AgentIdentity;
use gap::message::{now_unix, Envelope, Kind};
use gap::payment::Escrow;
use gap::runtime::Runtime;
use gap::storage::sqlite::SqliteStorage;
use gap::storage::Storage;
use gap::workflow::{FailureMode, Step, Workflow, WorkflowEngine};
use serde_json::json;
use std::collections::HashMap;

const CAP_SCRAPE: &str = "cap:data:scrape";
const CAP_ANALYZE: &str = "cap:analysis:summarize";
const CAP_PUBLISH: &str = "cap:content:publish";

fn announce(
    registry: &mut Registry,
    agent: &AgentIdentity,
    cap_id: &str,
    cap_name: &str,
) -> Result<()> {
    let cap = Capability {
        id: cap_id.into(),
        name: cap_name.into(),
        description: format!("{cap_name} capability"),
        input: json!({}),
        output: json!({}),
        price: Some(gap::discovery::Price {
            amount: 0.01,
            currency: "EUR".into(),
            model: "per_unit".into(),
        }),
        autonomy: vec!["propose".into(), "execute-notify".into()],
    };
    let ann = Announcement::signed(
        agent,
        vec![cap],
        vec![Reachability {
            transport: "https".into(),
            endpoint: format!("https://{}/gap", agent.did()),
        }],
        3600,
    );
    registry.announce(ann)?;
    Ok(())
}

fn main() -> Result<()> {
    println!("=== GAP — multi-agent workflow demo ===\n");

    // ---- Setup: identities ----
    let orchestrator = AgentIdentity::generate();
    let scraper = AgentIdentity::generate();
    let analyst = AgentIdentity::generate();
    let publisher = AgentIdentity::generate();
    let escrow_agent = AgentIdentity::generate();

    let mut registry = Registry::new();
    announce(&mut registry, &scraper, CAP_SCRAPE, "scrape")?;
    announce(&mut registry, &analyst, CAP_ANALYZE, "analyze")?;
    announce(&mut registry, &publisher, CAP_PUBLISH, "publish")?;
    println!("[discovery] 3 agents announced and registered");

    // ---- Delegation: orchestrator grants a mandate (RFC-0001) ----
    let mandate = Mandate {
        capabilities: vec![CAP_SCRAPE.into(), CAP_ANALYZE.into(), CAP_PUBLISH.into()],
        budget: Budget {
            per_contract: Some(5.0),
            per_day: Some(20.0),
            currency: "EUR".into(),
        },
        autonomy_level: "execute-notify".into(),
        jurisdictions: vec!["EU".into()],
        channels: vec![],
        expires_at: now_unix() + 86400,
        mode: "standing".into(),
    };
    let token = DelegationToken::issue(
        &orchestrator,
        orchestrator.did().clone(), // self-delegation root for the demo
        orchestrator.did().clone(),
        "urn:gap:dlg:0".into(),
        mandate,
    );
    token.verify()?;
    println!("[delegation] mandate issued with daily budget 20 EUR");

    // ---- Workflow definition (RFC-0002) ----
    let mut wf_inputs = HashMap::new();
    wf_inputs.insert("topic".into(), json!("quantum computing market"));

    let mut scrape = Step {
        step_id: "scrape".into(),
        capability: CAP_SCRAPE.into(),
        needs: vec![],
        inputs: HashMap::new(),
        outputs: HashMap::new(),
        terms: None,
        retryable: false,
    };
    scrape
        .inputs
        .insert("query".into(), "${workflow.topic}".into());
    scrape
        .outputs
        .insert("raw".into(), "steps.scrape.deliverable".into());

    let mut analyze = Step {
        step_id: "analyze".into(),
        capability: CAP_ANALYZE.into(),
        needs: vec!["scrape".into()],
        inputs: HashMap::new(),
        outputs: HashMap::new(),
        terms: None,
        retryable: false,
    };
    analyze
        .inputs
        .insert("data".into(), "${steps.scrape.raw}".into());
    analyze
        .outputs
        .insert("summary".into(), "steps.analyze.deliverable".into());

    let publish = Step {
        step_id: "publish".into(),
        capability: CAP_PUBLISH.into(),
        needs: vec!["analyze".into()],
        inputs: HashMap::new(),
        outputs: HashMap::new(),
        terms: None,
        retryable: false,
    };

    let workflow = Workflow::create(
        &orchestrator,
        "content-pipeline",
        wf_inputs,
        vec![scrape, analyze, publish],
        Some(gap::workflow::Budget {
            max_total: Some(10.0),
            currency: "EUR".into(),
        }),
        FailureMode::Abort,
        3600,
    );
    workflow.validate()?;
    println!("[workflow] 'content-pipeline' validated (DAG acyclic, bindings OK, budget OK)");

    // ---- Runtimes with SQLite storage ----
    let db_path = "/tmp/gap-workflow-demo.db";
    let _ = std::fs::remove_file(db_path);

    let mut orch = Runtime::new(orchestrator.clone());
    orch.set_storage(Box::new(SqliteStorage::open(db_path)?));

    // ---- Execute the workflow step by step ----
    let mut engine = WorkflowEngine::new();
    let mut step_count = 0;

    while step_count < workflow.steps.len() {
        let runnable = engine.runnable_steps(&workflow);
        assert!(!runnable.is_empty(), "workflow stalled");

        for step in runnable {
            let provider = match step.capability.as_str() {
                CAP_SCRAPE => &scraper,
                CAP_ANALYZE => &analyst,
                CAP_PUBLISH => &publisher,
                other => panic!("unknown capability {other}"),
            };
            let provider_name = match step.capability.as_str() {
                CAP_SCRAPE => "scraper",
                CAP_ANALYZE => "analyst",
                _ => "publisher",
            };

            println!(
                "\n[step] {:<9} ← {} ({})",
                step.step_id, provider_name, step.capability
            );

            // 1. Find the provider via discovery.
            let provider_name_query = match step.step_id.as_str() {
                "scrape" => "scrape",
                "analyze" => "analyze",
                _ => "publish",
            };
            let hits = registry.query(&Query {
                name: Some(provider_name_query.into()),
                ..Default::default()
            });
            assert!(!hits.is_empty(), "no provider for {}", step.capability);

            // 2. Negotiate and sign a contract (part 03).
            let terms = Terms {
                input: json!({}),
                deliverable: json!({}),
                acceptance_criteria: vec!["non-empty output".into()],
                deadline: now_unix() + 3600,
                price: Price {
                    amount: 0.05,
                    currency: "EUR".into(),
                    model: "fixed".into(),
                    cap: Some(1.0),
                },
                autonomy: "execute-notify".into(),
                confidentiality: None,
                human_review_above: None,
                cooling_off_seconds: None,
            };
            let contract = Contract::propose(
                &orchestrator,
                provider.did().clone(),
                &step.capability,
                terms,
                true,
            )
            .accept_by_provider(provider)?;
            contract.verify_signed()?;
            orch.bind_contract(contract.clone())?;
            let mut provider_rt = match step.capability.as_str() {
                CAP_SCRAPE => Runtime::new(scraper.clone()),
                CAP_ANALYZE => Runtime::new(analyst.clone()),
                _ => Runtime::new(publisher.clone()),
            };
            provider_rt.set_storage(Box::new(SqliteStorage::open(db_path)?));
            provider_rt.bind_contract(contract.clone())?;
            println!("  contract {} signed", &contract.contract_id[..24]);

            // 3. Escrow park (client) via signed instruction (part 05).
            let mut escrow = Escrow::for_contract(escrow_agent.clone(), contract.clone())?;
            let park = Envelope::new(
                orchestrator.did().clone(),
                escrow_agent.did().clone(),
                Kind::PayPark,
                json!({ "amount": 1.0 }),
            )
            .for_contract(contract.contract_id.clone())
            .sign(&orchestrator);
            escrow.park(&park)?;

            // 4. Provider executes (state machine), delivers an output.
            provider_rt.transition(&contract.contract_id, ContractState::Executing)?;
            provider_rt.transition(&contract.contract_id, ContractState::Delivered)?;
            orch.transition(&contract.contract_id, ContractState::Executing)?;
            orch.transition(&contract.contract_id, ContractState::Delivered)?;

            let output = match step.step_id.as_str() {
                "scrape" => json!({ "deliverable": "raw articles about quantum computing" }),
                "analyze" => json!({ "deliverable": "market summary: 3 key trends identified" }),
                _ => json!({ "deliverable": "https://blog.example/post-42" }),
            };
            let mut outs = HashMap::new();
            outs.insert("deliverable".into(), output);
            engine.deliver(&step.step_id, outs)?;
            engine.accept(&step.step_id)?;

            // 5. Client accepts + escrow releases (part 05).
            let acceptance = Envelope::new(
                orchestrator.did().clone(),
                provider.did().clone(),
                Kind::ExeAccept,
                json!({ "verdict": "accepted" }),
            )
            .for_contract(contract.contract_id.clone())
            .sign(&orchestrator);
            let release = Envelope::new(
                orchestrator.did().clone(),
                escrow_agent.did().clone(),
                Kind::PayRelease,
                json!({}),
            )
            .for_contract(contract.contract_id.clone())
            .sign(&orchestrator);
            escrow.release(&release, &acceptance)?;
            orch.transition(&contract.contract_id, ContractState::Accepted)?;

            println!("  delivered + accepted + settled (1.00 EUR)");
            step_count += 1;
        }
    }

    // ---- Resolve the final workflow output ----
    let summary = engine
        .resolve_binding("${steps.analyze.deliverable}", &workflow.inputs)
        .unwrap_or_else(|_| json!("(workflow output)"));
    println!("\n=== WORKFLOW COMPLETED ===");
    println!(
        "steps accepted: {}/{}",
        engine.accepted_count(&workflow),
        workflow.steps.len()
    );
    println!("final summary : {summary}");
    println!(
        "sqlite events : {} events persisted",
        SqliteStorage::open(db_path)?.event_count()?
    );
    let _ = std::fs::remove_file(db_path);
    println!("\nDemo complete — every artifact signed, escrowed, and persisted.");
    Ok(())
}
