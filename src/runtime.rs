//! Runtime layer — the ergonomic facade that binds all six protocol
//! layers into a single agent lifecycle.
//!
//! A [`Runtime`] is one agent's view of the world: its identity, the
//! contracts it has signed, the escrow it trusts, and the supervision it
//! is under. It exposes high-level operations that internally emit the
//! signed envelopes the protocol requires — so application code never
//! touches raw wire format unless it wants to.

use crate::contract::{Contract, ContractState, Terms};
use crate::error::{Error, Result};
use crate::identity::{AgentIdentity, Did};
use crate::message::{Envelope, Kind};
use crate::payment::Escrow;
use serde_json::json;

/// One agent's runtime state.
pub struct Runtime {
    identity: AgentIdentity,
    /// Contracts this agent is a party to (id -> contract + local state).
    contracts: Vec<(Contract, ContractState)>,
    /// The escrow agent this runtime trusts (optional).
    escrow: Option<Escrow>,
    /// DIDs this agent accepts as arbitrators.
    arbitrators: Vec<Did>,
    /// Received-and-processed message ids (replay protection, in-memory).
    seen: std::collections::HashSet<String>,
    /// Optional persistent storage (event spine).
    storage: Option<Box<dyn crate::storage::Storage>>,
}

impl Runtime {
    /// Create a fresh runtime for an identity.
    pub fn new(identity: AgentIdentity) -> Self {
        Self {
            identity,
            contracts: vec![],
            escrow: None,
            arbitrators: vec![],
            seen: std::collections::HashSet::new(),
            storage: None,
        }
    }

    /// Attach a persistent storage backend (SQLite, ClickHouse, …).
    /// Every subsequent event is appended to the spine.
    pub fn set_storage(&mut self, storage: Box<dyn crate::storage::Storage>) {
        self.storage = Some(storage);
    }

    /// Append an event to the attached storage (no-op without storage).
    fn record_event(&mut self, kind: &str, payload: serde_json::Value) {
        if let Some(storage) = self.storage.as_mut() {
            let _ = storage.append_event(kind, payload);
        }
    }

    /// This agent's DID.
    pub fn did(&self) -> &Did {
        self.identity.did()
    }

    /// Underlying identity (for signing, reputation access).
    pub fn identity(&self) -> &AgentIdentity {
        &self.identity
    }

    /// Attach an escrow agent.
    pub fn set_escrow(&mut self, escrow: Escrow) {
        self.escrow = Some(escrow);
    }

    /// Trust an arbitrator DID.
    pub fn add_arbitrator(&mut self, did: Did) {
        self.arbitrators.push(did);
    }

    /// Begin negotiation: propose a contract to a provider and accept
    /// it from this side. Returns the signed contract.
    pub fn propose_contract(
        &self,
        provider: &Did,
        capability_id: &str,
        terms: Terms,
        use_escrow: bool,
    ) -> Result<Contract> {
        Ok(Contract::propose(
            &self.identity,
            provider.clone(),
            capability_id,
            terms,
            use_escrow,
        ))
    }

    /// Accept a proposed contract as the provider. Verifies the client's
    /// signature before signing.
    pub fn accept_contract(&self, contract: Contract) -> Result<Contract> {
        contract.accept_by_provider(&self.identity)
    }

    /// Record a signed contract in the runtime and register it with the
    /// attached escrow.
    pub fn bind_contract(&mut self, contract: Contract) -> Result<()> {
        contract.verify_signed()?;
        let state = contract.state;
        self.contracts.push((contract.clone(), state));
        if let Some(escrow) = self.escrow.as_mut() {
            escrow.register(contract.clone())?;
        }
        self.record_event(
            "ctr.bound",
            json!({ "contract_id": contract.contract_id, "state": format!("{state:?}") }),
        );
        Ok(())
    }

    /// Look up a contract by id.
    pub fn contract(&self, id: &str) -> Option<&Contract> {
        self.contracts
            .iter()
            .map(|(c, _)| c)
            .find(|c| c.contract_id == id)
    }

    /// Mark a contract state transition.
    pub fn transition(&mut self, id: &str, to: ContractState) -> Result<()> {
        for (c, state) in self.contracts.iter_mut() {
            if c.contract_id == id {
                c.transition(to)?;
                *state = c.state;
                self.record_event(
                    "ctr.transition",
                    json!({ "contract_id": id, "to": format!("{to:?}") }),
                );
                return Ok(());
            }
        }
        Err(Error::UnknownContract(id.to_string()))
    }

    /// Park funds for a contract: emits and signs the `pay.park`
    /// instruction, then applies it to the attached escrow.
    pub fn park_funds(&mut self, contract_id: &str, amount: f64) -> Result<()> {
        let escrow_did = self
            .escrow
            .as_ref()
            .map(|e| e.did().clone())
            .ok_or_else(|| Error::EscrowViolation("no escrow attached".into()))?;
        let instruction = Envelope::new(
            self.identity.did().clone(),
            escrow_did,
            Kind::PayPark,
            json!({ "amount": amount }),
        )
        .for_contract(contract_id)
        .sign(&self.identity);
        let escrow = self
            .escrow
            .as_mut()
            .ok_or_else(|| Error::EscrowViolation("no escrow attached".into()))?;
        escrow.park(&instruction)?;
        Ok(())
    }

    /// Accept a delivery: emits `exe.accept`, then releases escrow funds.
    pub fn accept_delivery(&mut self, contract_id: &str) -> Result<()> {
        let contract = self
            .contract(contract_id)
            .cloned()
            .ok_or_else(|| Error::UnknownContract(contract_id.to_string()))?;
        let provider = contract.provider.clone();
        let escrow_did = self
            .escrow
            .as_ref()
            .map(|e| e.did().clone())
            .ok_or_else(|| Error::EscrowViolation("no escrow attached".into()))?;

        let acceptance = Envelope::new(
            self.identity.did().clone(),
            provider,
            Kind::ExeAccept,
            json!({ "verdict": "accepted" }),
        )
        .for_contract(contract_id)
        .sign(&self.identity);
        let release = Envelope::new(
            self.identity.did().clone(),
            escrow_did,
            Kind::PayRelease,
            json!({}),
        )
        .for_contract(contract_id)
        .sign(&self.identity);

        let escrow = self
            .escrow
            .as_mut()
            .ok_or_else(|| Error::EscrowViolation("no escrow attached".into()))?;
        escrow.release(&release, &acceptance)?;
        self.transition(contract_id, ContractState::Accepted)?;
        Ok(())
    }

    /// Dispute a delivery: emits `pay.dispute`.
    pub fn dispute_delivery(&mut self, contract_id: &str, reason: &str) -> Result<()> {
        let escrow_did = self
            .escrow
            .as_ref()
            .map(|e| e.did().clone())
            .ok_or_else(|| Error::EscrowViolation("no escrow attached".into()))?;
        let instruction = Envelope::new(
            self.identity.did().clone(),
            escrow_did,
            Kind::PayDispute,
            json!({ "reason": reason }),
        )
        .for_contract(contract_id)
        .sign(&self.identity);
        let escrow = self
            .escrow
            .as_mut()
            .ok_or_else(|| Error::EscrowViolation("no escrow attached".into()))?;
        escrow.dispute(&instruction)?;
        self.transition(contract_id, ContractState::Disputed)?;
        Ok(())
    }

    /// Request arbitration: emits a signed `ctr.ruling` request to the
    /// escrow, which executes it (split is decided by the arbitrator).
    pub fn submit_ruling(
        &mut self,
        contract_id: &str,
        split_client: f64,
        split_provider: f64,
    ) -> Result<()> {
        let escrow_did = self
            .escrow
            .as_ref()
            .map(|e| e.did().clone())
            .ok_or_else(|| Error::EscrowViolation("no escrow attached".into()))?;
        let ruling = Envelope::new(
            self.identity.did().clone(),
            escrow_did,
            Kind::CtrRuling,
            json!({ "split": { "client": split_client, "provider": split_provider } }),
        )
        .for_contract(contract_id)
        .sign(&self.identity);
        let escrow = self
            .escrow
            .as_mut()
            .ok_or_else(|| Error::EscrowViolation("no escrow attached".into()))?;
        // The arbitrator is this runtime's identity for the purpose of
        // this call; escrow validates `from` against the trusted list.
        let my_did = self.identity.did().clone();
        escrow.rule(&ruling, &my_did)?;
        self.transition(contract_id, ContractState::Ruled)?;
        Ok(())
    }

    /// Receive and process an incoming envelope with full validation
    /// (protocol, version, signature, freshness, replay). Returns the
    /// envelope if it is valid and was not seen before.
    pub fn receive(&mut self, envelope: &Envelope, max_age_secs: u64) -> Result<()> {
        envelope.validate(max_age_secs)?;
        if !self.seen.insert(envelope.message_id.clone()) {
            return Err(Error::StaleTimestamp); // replay
        }
        Ok(())
    }

    /// Sign an arbitrary envelope (helper for protocol extensions).
    pub fn sign(&self, envelope: Envelope) -> Envelope {
        envelope.sign(&self.identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::Price;
    use crate::message::now_unix;

    fn terms() -> Terms {
        Terms {
            input: json!({}),
            deliverable: json!({}),
            acceptance_criteria: vec!["ok".into()],
            deadline: now_unix() + 3600,
            price: Price {
                amount: 1.0,
                currency: "EUR".into(),
                model: "fixed".into(),
                cap: Some(10.0),
            },
            autonomy: "execute-notify".into(),
            confidentiality: None,
        }
    }

    #[test]
    fn runtime_full_lifecycle() -> Result<()> {
        // Two runtimes + shared escrow.
        let client_runtime = Runtime::new(AgentIdentity::generate());
        let provider_runtime = Runtime::new(AgentIdentity::generate());
        let escrow_agent = AgentIdentity::generate();

        // Provider binds the escrow.
        let mut provider = provider_runtime;
        let mut escrow = Escrow::new(escrow_agent.clone());
        provider.set_escrow(escrow);

        // Client proposes; provider accepts; both bind.
        let contract =
            client_runtime.propose_contract(provider.did(), "cap:lead-gen", terms(), true)?;
        let contract = provider.accept_contract(contract)?;
        assert_eq!(contract.state, ContractState::Signed);

        // Attach the escrow to the client runtime too, then bind contract.
        let mut client = client_runtime;
        escrow = Escrow::new(escrow_agent);
        client.set_escrow(escrow);
        client.bind_contract(contract.clone())?;
        provider.bind_contract(contract.clone())?;

        // Park funds on the client side.
        client.park_funds(&contract.contract_id, 5.0)?;
        assert_eq!(
            client
                .escrow
                .as_ref()
                .map(|e| e.state())
                .unwrap_or(crate::payment::EscrowState::Empty),
            crate::payment::EscrowState::Parked
        );

        // The provider executes and delivers (state machine steps).
        provider.transition(&contract.contract_id, ContractState::Executing)?;
        provider.transition(&contract.contract_id, ContractState::Delivered)?;
        // The client observes the same steps on its own copy.
        client.transition(&contract.contract_id, ContractState::Executing)?;
        client.transition(&contract.contract_id, ContractState::Delivered)?;

        // Accept delivery -> escrow released, contract Accepted.
        client.accept_delivery(&contract.contract_id)?;
        assert_eq!(
            client.contract(&contract.contract_id).unwrap().state,
            ContractState::Accepted
        );
        assert_eq!(
            client.escrow.as_ref().map(|e| e.state()).unwrap(),
            crate::payment::EscrowState::Released
        );
        Ok(())
    }

    #[test]
    fn runtime_rejects_replay() -> Result<()> {
        let mut runtime = Runtime::new(AgentIdentity::generate());
        let env = Envelope::new(
            runtime.did().clone(),
            runtime.did().clone(),
            Kind::CapQuery,
            json!({}),
        )
        .sign(runtime.identity());
        runtime.receive(&env, 300)?;
        assert!(runtime.receive(&env, 300).is_err());
        Ok(())
    }

    #[test]
    fn runtime_park_requires_escrow() {
        let mut runtime = Runtime::new(AgentIdentity::generate());
        assert!(runtime.park_funds("urn:gap:ctr:x", 5.0).is_err());
    }

    #[test]
    fn runtime_transition_unknown_contract_fails() {
        let mut runtime = Runtime::new(AgentIdentity::generate());
        assert!(runtime
            .transition("urn:gap:ctr:nope", ContractState::Signed)
            .is_err());
    }

    #[test]
    fn runtime_dispute_and_ruling_flow() -> Result<()> {
        let client_runtime = Runtime::new(AgentIdentity::generate());
        let provider_runtime = Runtime::new(AgentIdentity::generate());
        let escrow_agent = AgentIdentity::generate();

        let mut provider = provider_runtime;
        let mut escrow = Escrow::new(escrow_agent.clone());
        provider.set_escrow(escrow);

        let contract =
            client_runtime.propose_contract(provider.did(), "cap:lead-gen", terms(), true)?;
        let contract = provider.accept_contract(contract)?;

        let mut client = client_runtime;
        escrow = Escrow::new(escrow_agent);
        client.set_escrow(escrow);
        client.bind_contract(contract.clone())?;
        provider.bind_contract(contract.clone())?;

        client.park_funds(&contract.contract_id, 5.0)?;
        // Provider delivers; client disputes.
        provider.transition(&contract.contract_id, ContractState::Executing)?;
        provider.transition(&contract.contract_id, ContractState::Delivered)?;
        client.transition(&contract.contract_id, ContractState::Executing)?;
        client.transition(&contract.contract_id, ContractState::Delivered)?;
        client.dispute_delivery(&contract.contract_id, "nonconforming")?;
        assert_eq!(
            client.contract(&contract.contract_id).unwrap().state,
            ContractState::Disputed
        );

        // Client is also the arbitrator here (single-actor test); ruling
        // returns 100% to the client.
        client.submit_ruling(&contract.contract_id, 1.0, 0.0)?;
        assert_eq!(
            client.contract(&contract.contract_id).unwrap().state,
            ContractState::Ruled
        );
        Ok(())
    }

    #[test]
    fn runtime_sign_helper_roundtrip() {
        let runtime = Runtime::new(AgentIdentity::generate());
        let env = Envelope::new(
            runtime.did().clone(),
            runtime.did().clone(),
            Kind::CapQuery,
            json!({}),
        );
        let signed = runtime.sign(env);
        assert!(signed.verify().is_ok());
    }
}
