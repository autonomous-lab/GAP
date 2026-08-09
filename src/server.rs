//! GAP node — the HTTP server agents point at.
//!
//! Implements the API documented in `docs/node-api.md`:
//! identity, announce, discover, contracts, escrow, workflows, audit.
//!
//! The node holds agent identities (key custody), the registry, the
//! escrows, and the audit spine. Agents speak HTTPS to it; they never
//! implement GAP themselves.

use crate::contract::{Contract, ContractState, Terms};
use crate::discovery::{Announcement, Capability, Query, Reachability, Registry};
use crate::error::{Error, Result};
use crate::identity::AgentIdentity;
use crate::message::{now_unix, Envelope, Kind};
use crate::payment::Escrow;
use crate::relayer::{Chain, Relayer};
use crate::storage::{AnnouncementRecord, ContractRecord, EscrowRecord, IdentityRecord, Storage};
use crate::sybil::RateCounters;
use crate::workflow::{Budget, FailureMode, Workflow, WorkflowEngine};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The node's public identity (its own DID).
#[derive(Clone)]
pub struct NodeIdentity {
    pub identity: AgentIdentity,
}

/// A registered agent on the node.
pub struct RegisteredAgent {
    pub identity: AgentIdentity,
    /// The agent's announcement (for the registry).
    pub announcement: Option<Announcement>,
}

/// One entry of an agent's public track record.
///
/// Deliberately pseudonymous: the capability, the outcome and the timing
/// are public evidence, while the contract and the counterparty appear
/// only as stable digests. A reader can verify an agent's history and
/// count repeat business without learning who its clients are
/// (spec 01 §1.4 rule 2: selective disclosure, no fabrication).
#[derive(Debug, Clone, serde::Serialize)]
pub struct JobRecord {
    /// sha256(contract_id), truncated — stable, non-reversible.
    pub job_ref: String,
    pub capability_id: String,
    /// sha256(counterparty DID), truncated.
    pub counterparty_ref: String,
    /// accepted | disputed | ruled
    pub outcome: String,
    /// Whether the work had to be reworked before it was accepted.
    /// Published because a buyer deserves to know the difference
    /// between right-first-time and right-on-the-second-try.
    pub remedied: bool,
    /// The verifier's ruling, when one was produced.
    pub verdict: Option<String>,
    /// Which judge ruled, when one did.
    pub judged_by: Option<String>,
    pub on_time: bool,
    pub at: u64,
    /// Audit-spine sequence at the moment this job settled. Gives the
    /// public feed the same resumable cursor the protocol already uses.
    #[serde(default)]
    pub seq: u64,
}

/// An agent's dispute record (RFC-0015 §3.3).
///
/// Counting raw disputes would punish the honest agent that bad
/// counterparties keep challenging, and it would hand anyone a griefing
/// weapon: dispute a competitor's every contract to tarnish it. The
/// signal of abuse is **disputing and being wrong**, so the published
/// figure is a win rate, and disputes merely received are tracked
/// separately.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DisputeStats {
    /// Disputes this agent opened.
    pub raised: u64,
    /// …of which an arbitrator ruled in its favour.
    pub raised_won: u64,
    /// Disputes opened against this agent.
    pub received: u64,
    /// …of which it lost.
    pub received_lost: u64,
}

impl DisputeStats {
    /// Share of this agent's own disputes that were upheld. A careful
    /// buyer scores high; a freeloader who challenges everything scores
    /// low. `None` until it has actually disputed something.
    pub fn win_rate(&self) -> Option<f64> {
        if self.raised == 0 {
            None
        } else {
            Some(self.raised_won as f64 / self.raised as f64)
        }
    }
}

fn pseudonym(input: &str) -> String {
    crate::sha256_hex(input.as_bytes())[..16].to_string()
}

/// The node state — shared behind a mutex, one process, one order.
pub struct NodeState {
    /// The node's own identity.
    pub node: NodeIdentity,
    /// token -> registered agent (key custody).
    agents: HashMap<String, RegisteredAgent>,
    /// did -> token. O(1) provider lookup for propose; without this
    /// index every propose scanned all agents (O(n) with an
    /// allocation per entry) — the throughput killer under load and a
    /// DoS vector via the unauthenticated identity endpoint.
    agents_by_did: HashMap<String, String>,
    /// The discovery registry.
    pub registry: Registry,
    /// contract_id -> contract (materialized state).
    contracts: HashMap<String, Contract>,
    /// contract_id -> escrow instance.
    escrows: HashMap<String, Escrow>,
    /// workflow_id -> workflow manifest and engine state.
    workflows: HashMap<String, (Workflow, WorkflowEngine)>,
    /// Optional on-chain relayer (when configured, escrow goes on-chain).
    relayer: Option<Relayer>,
    /// The audit spine.
    pub storage: Box<dyn Storage>,
    /// Per-token rate counters (audit H-03: API rate limiting).
    rate_limits: std::collections::HashMap<String, RateCounters>,
    /// Per-source-IP rate counters (audit H-03).
    ip_limits: std::collections::HashMap<String, RateCounters>,
    /// Per-token cap (requests per minute). Configurable via env
    /// (`GAP_RATE_TOKEN_CAP`) so operators can trade security vs.
    /// throughput; default 120.
    token_cap: u32,
    /// Per-IP cap (requests per minute); default 600.
    ip_cap: u32,
    /// Optional admin token required for node-arbitrated settlement.
    admin_token: Option<String>,
    /// Seed vault (encryption at rest for custodied identity seeds),
    /// keyed by `GAP_MASTER_KEY` when set.
    vault: Option<crate::vault::Vault>,
    /// Optional delivery judge (RFC-0014). None = deterministic checks only.
    verifier: Option<Box<dyn crate::verifier::Verifier>>,
    /// contract_id -> the signed verdict produced for it.
    verdicts: HashMap<String, crate::verifier::Verdict>,
    /// Per-agent job history, the raw material of reputation (RFC-0014 §5).
    jobs: HashMap<String, Vec<JobRecord>>,
    /// Per-agent dispute record (RFC-0015).
    disputes: HashMap<String, DisputeStats>,
    /// Pseudonymous job reference -> contract id (never published).
    jobs_by_ref: HashMap<String, String>,
    /// agent DID -> its principal binding (spec 01 §1.3).
    bindings: HashMap<String, crate::principal::PrincipalBinding>,
    /// agent DID -> active vetoes (spec 06 §6.5).
    vetoes: HashMap<String, Vec<crate::principal::Veto>>,
    /// agent DID -> signed daily budget cap.
    budgets: HashMap<String, crate::principal::BudgetGrant>,
    /// (agent DID, unix day) -> already parked that day.
    spend_today: HashMap<(String, u64), crate::amount::Amount>,
    /// Second, independent judge (RFC-0015): a different model on a
    /// different host, so the two do not share a failure mode.
    verifier_b: Option<Box<dyn crate::verifier::Verifier>>,
    /// Verdicts awaiting a human: contract_id -> why.
    escalations: HashMap<String, crate::verifier::Escalation>,
    /// Delivery subscriptions, id -> subscription (RFC-0013).
    subscriptions: HashMap<String, crate::delivery::Subscription>,
    /// Pending webhook deliveries, drained outside the state lock.
    outbox: Vec<crate::delivery::PendingDelivery>,
}

impl NodeState {
    pub fn new(storage: Box<dyn Storage>) -> Self {
        Self::with_seed(storage, None)
    }

    /// Create the node state with an optional persisted identity seed
    /// (audit fix H-01: without persistence the node DID rotates on
    /// every restart, breaking trust continuity).
    pub fn with_seed(storage: Box<dyn Storage>, seed: Option<[u8; 32]>) -> Self {
        Self::with_rate_limits(storage, seed, 120, 600)
    }

    /// Full constructor with explicit rate caps (used by tests and the
    /// HTTP benchmark to lift the security caps when measuring raw
    /// capacity).
    pub fn with_rate_limits(
        storage: Box<dyn Storage>,
        seed: Option<[u8; 32]>,
        token_cap: u32,
        ip_cap: u32,
    ) -> Self {
        // Seed vault (encryption at rest). A malformed master key is a
        // boot-time configuration error: fail loudly, not silently
        // plaintext.
        let vault = crate::vault::Vault::from_env()
            .map(|r| r.expect("invalid GAP_MASTER_KEY (need 64 hex chars)"));
        Self::with_vault(storage, seed, token_cap, ip_cap, vault)
    }

    /// Core constructor with an explicit seed vault (testable without
    /// touching process environment).
    pub fn with_vault(
        storage: Box<dyn Storage>,
        seed: Option<[u8; 32]>,
        token_cap: u32,
        ip_cap: u32,
        vault: Option<crate::vault::Vault>,
    ) -> Self {
        let identity = match seed {
            Some(seed_bytes) => AgentIdentity::from_seed(&seed_bytes),
            None => AgentIdentity::generate(),
        };
        let mut agents = HashMap::new();
        let mut agents_by_did = HashMap::new();
        for rec in storage.list_identities().unwrap_or_default() {
            let seed_hex = match vault.as_ref() {
                Some(v) => match v.open(&rec.seed_hex) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("gap-node: skipping identity {}: {e}", rec.did);
                        continue;
                    }
                },
                None if crate::vault::Vault::is_sealed(&rec.seed_hex) => {
                    eprintln!(
                        "gap-node: identity {} is sealed but GAP_MASTER_KEY is unset — skipping",
                        rec.did
                    );
                    continue;
                }
                None => rec.seed_hex.clone(),
            };
            if let Ok(seed_bytes) = decode_seed_hex(&seed_hex) {
                let agent_identity = AgentIdentity::from_seed(&seed_bytes);
                let did = agent_identity.did().to_string();
                agents.insert(
                    rec.token.clone(),
                    RegisteredAgent {
                        identity: agent_identity,
                        announcement: None,
                    },
                );
                agents_by_did.insert(did, rec.token);
            }
        }

        let mut registry = Registry::new();
        for rec in storage.list_announcements().unwrap_or_default() {
            if let Ok(ann) = serde_json::from_str::<Announcement>(&rec.announcement_json) {
                let _ = registry.announce(ann);
            }
        }

        let mut contracts = HashMap::new();
        for rec in storage.list_contracts().unwrap_or_default() {
            if let Ok(mut contract) = serde_json::from_str::<Contract>(&rec.contract_json) {
                if let Ok(state) = ContractState::parse(&rec.state) {
                    contract.state = state;
                }
                contracts.insert(contract.contract_id.clone(), contract);
            }
        }

        let mut escrows = HashMap::new();
        for rec in storage.list_escrows().unwrap_or_default() {
            if let Some(contract) = contracts.get(&rec.contract_id).cloned() {
                if let (Ok(state), Ok(held)) = (
                    crate::payment::EscrowState::parse(&rec.state),
                    crate::amount::Amount::parse(&rec.held),
                ) {
                    if let Ok(escrow) =
                        Escrow::restore(identity.clone(), contract, state, held, rec.currency)
                    {
                        escrows.insert(rec.contract_id, escrow);
                    }
                }
            }
        }

        Self {
            node: NodeIdentity { identity },
            agents,
            agents_by_did,
            registry,
            contracts,
            escrows,
            workflows: HashMap::new(),
            relayer: None,
            storage,
            rate_limits: std::collections::HashMap::new(),
            ip_limits: std::collections::HashMap::new(),
            token_cap,
            ip_cap,
            admin_token: None,
            vault,
            verifier: crate::verifier::OpenRouterVerifier::from_env()
                .map(|v| Box::new(v) as Box<dyn crate::verifier::Verifier>),
            verifier_b: crate::verifier::OpenRouterVerifier::second_from_env()
                .map(|v| Box::new(v) as Box<dyn crate::verifier::Verifier>),
            verdicts: HashMap::new(),
            jobs: HashMap::new(),
            disputes: HashMap::new(),
            jobs_by_ref: HashMap::new(),
            bindings: HashMap::new(),
            vetoes: HashMap::new(),
            budgets: HashMap::new(),
            spend_today: HashMap::new(),
            escalations: HashMap::new(),
            subscriptions: HashMap::new(),
            outbox: Vec::new(),
        }
    }

    /// Configure the node-arbitration admin token.
    pub fn set_admin_token(&mut self, token: impl Into<String>) {
        self.admin_token = Some(token.into());
    }

    fn persist_contract(&mut self, contract: &Contract) {
        let record = ContractRecord {
            contract_id: contract.contract_id.clone(),
            client: contract.client.to_string(),
            provider: contract.provider.to_string(),
            capability_id: contract.capability_id.clone(),
            state: contract.state.wire_name().into(),
            contract_json: serde_json::to_string(contract).unwrap_or_else(|_| "{}".into()),
            updated_at: now_unix(),
        };
        let _ = self.storage.upsert_contract(&record);
    }

    fn persist_escrow(
        &mut self,
        contract_id: &str,
        state: crate::payment::EscrowState,
        held: crate::amount::Amount,
        currency: &str,
    ) {
        let record = EscrowRecord {
            contract_id: contract_id.into(),
            state: state.wire_name().into(),
            held: held.to_string_decimal(),
            currency: currency.into(),
            updated_at: now_unix(),
        };
        let _ = self.storage.upsert_escrow(&record);
    }

    /// Enforce per-token and per-IP rate limits before processing a
    /// request (audit fix H-03). Returns an error when over limit.
    /// Evict idle rate counters so the maps stay bounded: an attacker
    /// rotating source IPs (or tokens) must not grow node memory
    /// without limit. A counter is idle once its window has lapsed —
    /// evicting it loses nothing, the fresh entry starts at zero.
    const RATE_MAP_SOFT_CAP: usize = 4096;

    fn evict_idle_counters(&mut self, now: u64) {
        if self.rate_limits.len() > Self::RATE_MAP_SOFT_CAP {
            self.rate_limits.retain(|_, c| !c.is_idle(now));
        }
        if self.ip_limits.len() > Self::RATE_MAP_SOFT_CAP {
            self.ip_limits.retain(|_, c| !c.is_idle(now));
        }
    }

    pub fn check_rate_limit(&mut self, token: Option<&str>, ip: Option<&str>) -> Result<()> {
        let now = crate::message::now_unix();
        self.evict_idle_counters(now);
        if let Some(t) = token {
            self.rate_limits
                .entry(t.to_string())
                .or_default()
                .record_invocation(now, self.token_cap)?; // per-token cap
        }
        if let Some(src) = ip {
            self.ip_limits
                .entry(src.to_string())
                .or_default()
                .record_invocation(now, self.ip_cap)?; // per-IP cap
        }
        Ok(())
    }

    /// The node's identity seed (hex) — persist this to keep the same
    /// DID across restarts.
    pub fn export_seed_hex(&self) -> String {
        // The seed is not recoverable from the public AgentIdentity API
        // in this reference; production should store the seed at
        // creation. The env-var path in main.rs covers persistence.
        String::new()
    }

    /// Attach the on-chain relayer (GapEscrow contract).
    pub fn set_relayer(&mut self, chain: Box<dyn Chain>, escrow_address: &str) {
        self.relayer = Some(Relayer::new(chain, escrow_address));
    }

    /// Hash a contract id for on-chain use (keccak256, matching the
    /// GapEscrow contract's hashContract semantics).
    pub fn contract_hash(id: &str) -> [u8; 32] {
        use tiny_keccak::{Hasher, Keccak};
        let mut keccak = Keccak::v256();
        keccak.update(id.as_bytes());
        let mut out = [0u8; 32];
        keccak.finalize(&mut out);
        out
    }

    /// The node's DID.
    pub fn node_did(&self) -> crate::identity::Did {
        self.node.identity.did().clone()
    }

    /// Record an execution outcome on an agent's reputation and push the
    /// smoothed score into the discovery registry, so `min_reputation`
    /// filters work against earned scores. Before this, the node never
    /// fed the registry: every agent sat at the 0.0 default and
    /// reputation-filtered queries returned nothing.
    fn credit_reputation(
        &mut self,
        agent_did: &crate::identity::Did,
        accepted: bool,
        on_time: bool,
    ) {
        let did_str = agent_did.to_string();
        if let Some(token) = self.agents_by_did.get(&did_str).cloned() {
            if let Some(agent) = self.agents.get_mut(&token) {
                agent.identity.reputation_mut().record(accepted, on_time);
                let score = agent.identity.reputation().success_rate();
                self.registry.set_reputation(agent_did.clone(), score);
                self.record(
                    "node.reputation.update",
                    json!({ "agent_did": did_str, "score": score, "accepted": accepted, "on_time": on_time }),
                );
            }
        }
    }

    fn issue_token(&mut self) -> String {
        // Audit fix C-01: CSPRNG 256-bit token — sequential tokens were
        // guessable (full account takeover).
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        format!("gat_{}", hex::encode(bytes))
    }

    /// Create a new agent identity, returning (did, token).
    ///
    /// The token is the only credential (audit L-02: removed the
    /// misleading empty "secret" field). Key custody is server-side;
    /// a production deployment backs it with a KMS.
    pub fn create_identity(&mut self) -> (String, String) {
        let identity = AgentIdentity::generate();
        let did = identity.did().to_string();
        let token = self.issue_token();
        // Seal the custodied seed when a vault is configured; a database
        // copy must not be a copy of every identity on the node.
        let seed_hex = match self.vault.as_ref() {
            Some(v) => v.seal(&identity.seed_hex()),
            None => identity.seed_hex(),
        };
        self.agents.insert(
            token.clone(),
            RegisteredAgent {
                identity,
                announcement: None,
            },
        );
        self.agents_by_did.insert(did.clone(), token.clone());
        let _ = self.storage.upsert_identity(&IdentityRecord {
            token: token.clone(),
            did: did.clone(),
            seed_hex,
            created_at: now_unix(),
        });
        (did, token)
    }

    /// The custodied identity behind a DID, when this node holds it.
    /// Used where the node must sign on the agent's behalf.
    pub fn agent_identity_for(&self, did: &str) -> Option<AgentIdentity> {
        self.agents_by_did
            .get(did)
            .and_then(|t| self.agents.get(t))
            .map(|a| a.identity.clone())
    }

    /// Look up an agent by bearer token.
    pub fn agent_by_token(&self, token: &str) -> Result<&RegisteredAgent> {
        self.agents
            .get(token)
            .ok_or_else(|| Error::Unauthorized("invalid bearer token".into()))
    }

    /// Announce capabilities for the authenticated agent.
    pub fn announce(
        &mut self,
        token: &str,
        capabilities: Vec<Capability>,
        _languages: Vec<String>,
        _regions: Vec<String>,
        ttl_seconds: u64,
    ) -> Result<String> {
        self.announce_with_reachability(
            token,
            capabilities,
            _languages,
            _regions,
            ttl_seconds,
            vec![],
        )
    }

    /// Announce with the agent's declared reachability (spec 02 §2.2).
    ///
    /// The node used to overwrite whatever the agent declared with a
    /// placeholder (`https://agent/<did>/gap`, not a routable host),
    /// which discarded the very data event delivery needs — spec §2.4.4
    /// requires a registry to support a transport *listed by the agent*
    /// (RFC-0013 §2.2). Declared entries are now stored verbatim; the
    /// node-mediated entry is appended, clearly marked.
    pub fn announce_with_reachability(
        &mut self,
        token: &str,
        capabilities: Vec<Capability>,
        _languages: Vec<String>,
        _regions: Vec<String>,
        ttl_seconds: u64,
        declared: Vec<Reachability>,
    ) -> Result<String> {
        let agent = self
            .agents
            .get_mut(token)
            .ok_or_else(|| Error::Unauthorized("invalid bearer token".into()))?;
        let mut reachability = declared;
        reachability.push(Reachability {
            transport: "gap-node".into(),
            endpoint: format!("/v1/contract/?agent={}", agent.identity.did()),
        });
        let mut ann =
            Announcement::signed(&agent.identity, capabilities, reachability, ttl_seconds);
        ann.languages = _languages;
        ann.regions = _regions;
        ann.resign(&agent.identity);
        ann.verify()?;
        self.registry.announce(ann.clone())?;
        let id = format!("urn:gap:ann:{}", &ann.agent_did.to_string()[..16]);
        agent.announcement = Some(ann);
        if let Some(saved) = &agent.announcement {
            let _ = self.storage.upsert_announcement(&AnnouncementRecord {
                agent_did: saved.agent_did.to_string(),
                announcement_json: serde_json::to_string(saved).unwrap_or_else(|_| "{}".into()),
                expires_at: now_unix().saturating_add(saved.ttl_seconds),
            });
        }
        self.record("cap.announce", json!({ "agent_did": self.agent_by_token(token).map(|a| a.identity.did().to_string()).unwrap_or_default() }));
        Ok(id)
    }

    /// Query the registry.
    pub fn discover(&self, query: &Query) -> Vec<Announcement> {
        self.registry.query(query)
    }

    /// Propose a contract from the authenticated client.
    pub fn propose_contract(
        &mut self,
        client_token: &str,
        provider_did: &str,
        capability_id: &str,
        terms: Terms,
        use_escrow: bool,
    ) -> Result<String> {
        let client = self.agent_by_token(client_token)?;
        let client_did = client.identity.did().clone();
        self.check_veto(&client_did, "")?;
        let client = self.agent_by_token(client_token)?;
        let provider = crate::identity::Did::parse(provider_did)?;
        // O(1) provider lookup via the DID index (previously a scan of
        // every registered agent — quadratic once the node serves
        // many identities).
        if !self.agents_by_did.contains_key(provider_did) {
            return Err(Error::UnknownContract(format!(
                "provider {provider_did} not registered on this node"
            )));
        }
        let contract =
            Contract::propose(&client.identity, provider, capability_id, terms, use_escrow);
        let id = contract.contract_id.clone();
        self.persist_contract(&contract);
        self.contracts.insert(id.clone(), contract);
        self.record("ctr.propose", json!({ "contract_id": id }));
        Ok(id)
    }

    /// The provider accepts a proposed contract.
    pub fn accept_contract(&mut self, provider_token: &str, contract_id: &str) -> Result<()> {
        let provider = self.agent_by_token(provider_token)?;
        let identity = provider.identity.clone();
        let contract = self
            .contracts
            .get(contract_id)
            .cloned()
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        // A rejected or cancelled negotiation is over (spec 03 §3.3).
        if contract.state != ContractState::Draft {
            return Err(Error::InvalidTransition {
                from: contract.state.wire_name().into(),
                to: "signed".into(),
            });
        }
        // After a counter it is the *other* party's turn to accept, so
        // acceptance is not provider-only: whoever did not sign last
        // closes the deal.
        if contract.client == *identity.did() {
            let mut c = contract.clone();
            c.resign_as(&identity)?;
            if c.provider_sig.is_some() && c.client_sig.is_some() {
                c.verify_signed()?;
                c.transition(ContractState::Signed)?;
            }
            self.contracts.insert(contract_id.into(), c);
            if let Some(saved) = self.contracts.get(contract_id).cloned() {
                self.persist_contract(&saved);
            }
            self.record("ctr.accept", json!({ "contract_id": contract_id }));
            return Ok(());
        }
        let signed = contract.accept_by_provider(&provider.identity)?;
        self.contracts.insert(contract_id.into(), signed);
        if let Some(saved) = self.contracts.get(contract_id).cloned() {
            self.persist_contract(&saved);
        }
        self.record("ctr.accept", json!({ "contract_id": contract_id }));
        Ok(())
    }

    /// Provider delivers with a proof bundle.
    pub fn deliver(
        &mut self,
        provider_token: &str,
        contract_id: &str,
        deliverable_hash: &str,
    ) -> Result<()> {
        let provider_did = self.agent_by_token(provider_token)?.identity.did().clone();
        self.check_veto(&provider_did, contract_id)?;
        let contract = self
            .contracts
            .get_mut(contract_id)
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.provider != provider_did {
            return Err(Error::Unauthorized("only the provider may deliver".into()));
        }
        if contract.escrow && !self.escrows.contains_key(contract_id) && self.relayer.is_none() {
            return Err(Error::EscrowViolation(
                "escrow must be parked before delivery".into(),
            ));
        }
        if deliverable_hash.trim().is_empty() {
            return Err(Error::Other("deliverable_hash required".into()));
        }
        // Keep the commitment: verification later checks what the client
        // actually received against this exact digest.
        contract.deliverable_hash = Some(deliverable_hash.to_string());
        // `exe.start` is optional (spec 04 §4.2): a provider that
        // announced its plan is already Executing, one that went
        // straight to delivery still needs the intermediate step.
        if contract.state == ContractState::Signed {
            contract.transition(ContractState::Executing)?;
        }
        contract.transition(ContractState::Delivered)?;
        let saved = contract.clone();
        self.persist_contract(&saved);
        self.record("exe.deliver", json!({ "contract_id": contract_id }));
        Ok(())
    }

    /// Client accepts delivery; escrow releases automatically.
    pub fn accept_delivery(&mut self, client_token: &str, contract_id: &str) -> Result<Value> {
        let client = self.agent_by_token(client_token)?;
        let contract = self
            .contracts
            .get(contract_id)
            .cloned()
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.client != *client.identity.did() {
            return Err(Error::Unauthorized("only the client may accept".into()));
        }
        if contract.state != ContractState::Delivered {
            return Err(Error::InvalidTransition {
                from: contract.state.wire_name().into(),
                to: "accepted".into(),
            });
        }
        // RFC-0014: a recorded non-conforming verdict blocks release.
        // The client may accept an *inconclusive* delivery (that is its
        // prerogative — it is its money), but it cannot release funds
        // against signed evidence that the work does not conform; that
        // path is the dispute, where an arbitrator splits.
        if let Some(v) = self.verdicts.get(contract_id) {
            if v.ruling == crate::verifier::Ruling::Nonconforming {
                return Err(Error::EscrowViolation(format!(
                    "delivery was verified non-conforming ({}); dispute it instead of releasing",
                    v.reasons.first().cloned().unwrap_or_default()
                )));
            }
            // An escalated case is provisional: a human closes it
            // (RFC-0015). Releasing early would make the escalation
            // decorative.
            if let Some(e) = v.escalation {
                return Err(Error::EscrowViolation(format!(
                    "verdict escalated for human review ({}); await arbitration",
                    e.as_str()
                )));
            }
        }

        // Build the signed acceptance + release envelopes.
        let acceptance = Envelope::new(
            client.identity.did().clone(),
            contract.provider.clone(),
            Kind::ExeAccept,
            json!({ "verdict": "accepted" }),
        )
        .for_contract(contract_id)
        .sign(&client.identity);
        let release = Envelope::new(
            client.identity.did().clone(),
            self.node_did(),
            Kind::PayRelease,
            json!({}),
        )
        .for_contract(contract_id)
        .sign(&client.identity);

        // On-chain path: submit release() to the GapEscrow contract.
        if let Some(relayer) = &self.relayer {
            let hash = Self::contract_hash(contract_id);
            relayer.release(&hash)?;
            self.record(
                "pay.released.onchain",
                json!({ "contract_id": contract_id }),
            );
            let contract = self
                .contracts
                .get_mut(contract_id)
                .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
            contract.transition(ContractState::Accepted)?;
            let saved = contract.clone();
            self.persist_contract(&saved);
            self.persist_escrow(
                contract_id,
                crate::payment::EscrowState::Released,
                crate::amount::Amount::ZERO,
                "USDC",
            );
            self.record("exe.accept", json!({ "contract_id": contract_id }));
            let on_time = crate::message::now_unix() <= saved.terms.deadline;
            self.credit_reputation(&saved.provider, true, on_time);
            return Ok(json!({
                "state": "accepted",
                "settlement": { "amount": "0.000000", "currency": "USDC", "chain": "onchain" }
            }));
        }

        // Settle through the contract's escrow.
        let escrow = self
            .escrows
            .get_mut(contract_id)
            .ok_or_else(|| Error::EscrowViolation("no escrow for contract".into()))?;
        let receipt = escrow.release(&release, &acceptance)?;
        let amount = receipt.amount;
        let currency = receipt.currency;

        let contract = self
            .contracts
            .get_mut(contract_id)
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        contract.transition(ContractState::Accepted)?;
        let saved = contract.clone();
        self.persist_contract(&saved);
        self.persist_escrow(
            contract_id,
            crate::payment::EscrowState::Released,
            crate::amount::Amount::ZERO,
            &currency,
        );

        self.record("exe.accept", json!({ "contract_id": contract_id }));
        let on_time = crate::message::now_unix() <= saved.terms.deadline;
        self.credit_reputation(&saved.provider, true, on_time);
        self.record_job(
            &saved.provider.clone(),
            &saved.client.clone(),
            contract_id,
            &saved.capability_id.clone(),
            "accepted",
            on_time,
        );
        Ok(json!({
            "state": "accepted",
            "settlement": { "amount": amount, "currency": currency }
        }))
    }

    /// Client parks funds into escrow for a contract.
    pub fn escrow_park(
        &mut self,
        client_token: &str,
        contract_id: &str,
        amount: &crate::amount::Amount,
    ) -> Result<()> {
        let client_did = self.agent_by_token(client_token)?.identity.did().clone();
        // Two inalienable principal rights, checked before any money
        // moves: the veto, and the daily budget cap (spec 06 §6.5).
        self.check_veto(&client_did, contract_id)?;
        self.check_budget(&client_did, *amount)?;
        let client = self.agent_by_token(client_token)?;
        let contract = self
            .contracts
            .get(contract_id)
            .cloned()
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.client != *client.identity.did() {
            return Err(Error::Unauthorized("only the client may park".into()));
        }
        if contract.state != ContractState::Signed {
            return Err(Error::EscrowViolation(format!(
                "contract must be signed before parking (state: {})",
                contract.state.wire_name()
            )));
        }

        // On-chain path: submit park() to the GapEscrow contract.
        if let Some(relayer) = &self.relayer {
            let hash = Self::contract_hash(contract_id);
            // In production the provider/arbitrator EVM addresses come
            // from the agents' EVM keys; the reference uses derived
            // addresses from the relayer's key custody.
            let provider_addr = relayer.key_for(&contract.provider.to_string()).address();
            let arb_addr = relayer.key_for(&self.node_did().to_string()).address();
            let amount_units = amount.minor_units(); // exact minor units
            relayer.park(&hash, &provider_addr, &arb_addr, amount_units)?;
            self.persist_escrow(
                contract_id,
                crate::payment::EscrowState::Parked,
                *amount,
                "USDC",
            );
            self.record(
                "pay.parked.onchain",
                json!({ "contract_id": contract_id, "amount": amount }),
            );
            return Ok(());
        }

        // Off-chain path (reference escrow).
        let instruction = Envelope::new(
            client.identity.did().clone(),
            self.node_did(),
            Kind::PayPark,
            json!({ "amount": amount.to_string_decimal() }),
        )
        .for_contract(contract_id)
        .sign(&client.identity);
        let mut escrow = Escrow::for_contract(self.node.identity.clone(), contract)?;
        escrow.park(&instruction)?;
        self.persist_escrow(
            contract_id,
            escrow.state(),
            escrow.held(),
            escrow.currency(),
        );
        self.escrows.insert(contract_id.into(), escrow);
        self.record(
            "pay.parked",
            json!({ "contract_id": contract_id, "amount": amount.to_string_decimal() }),
        );
        Ok(())
    }

    /// Client explicitly releases escrow. The automatic path
    /// (`accept-delivery`) already releases; this route exists for
    /// clients that settle through the release endpoint directly and
    /// confirms the release state.
    pub fn escrow_release(&mut self, client_token: &str, contract_id: &str) -> Result<Value> {
        let client = self.agent_by_token(client_token)?;
        let escrow = self
            .escrows
            .get(contract_id)
            .ok_or_else(|| Error::EscrowViolation("no escrow for contract".into()))?;
        if escrow.state() != crate::payment::EscrowState::Released {
            return Err(Error::EscrowViolation(
                "funds not released; accept delivery first".into(),
            ));
        }
        let _ = client;
        self.record("pay.released", json!({ "contract_id": contract_id }));
        Ok(json!({ "state": "released", "contract_id": contract_id }))
    }

    /// Client-driven refund (parked or disputed state).
    pub fn escrow_refund(&mut self, client_token: &str, contract_id: &str) -> Result<Value> {
        let client = self.agent_by_token(client_token)?;
        let contract = self
            .contracts
            .get(contract_id)
            .cloned()
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.client != *client.identity.did() {
            return Err(Error::Unauthorized("only the client may refund".into()));
        }
        if contract.state != ContractState::Signed {
            return Err(Error::InvalidTransition {
                from: contract.state.wire_name().into(),
                to: "refunded".into(),
            });
        }
        let instruction = Envelope::new(
            client.identity.did().clone(),
            self.node_did(),
            Kind::PayRefund,
            json!({}),
        )
        .for_contract(contract_id)
        .sign(&client.identity);
        let escrow = self
            .escrows
            .get_mut(contract_id)
            .ok_or_else(|| Error::EscrowViolation("no escrow for contract".into()))?;
        let receipt = escrow.refund(&instruction)?;
        let mut contract = contract;
        contract.transition(ContractState::Cancelled)?;
        self.contracts.insert(contract_id.into(), contract.clone());
        self.persist_contract(&contract);
        self.persist_escrow(
            contract_id,
            crate::payment::EscrowState::Refunded,
            crate::amount::Amount::ZERO,
            &receipt.currency,
        );
        self.record(
            "pay.refund",
            json!({ "contract_id": contract_id, "amount": receipt.amount }),
        );
        Ok(json!({ "receipt": { "event": "pay.refund", "amount": receipt.amount } }))
    }

    /// Client disputes a contract; funds move to `disputed`.
    pub fn contract_dispute(
        &mut self,
        client_token: &str,
        contract_id: &str,
        reason: &str,
    ) -> Result<Value> {
        let client = self.agent_by_token(client_token)?;
        let contract = self
            .contracts
            .get(contract_id)
            .cloned()
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.client != *client.identity.did() {
            return Err(Error::Unauthorized("only the client may dispute".into()));
        }
        if contract.state != ContractState::Delivered {
            return Err(Error::InvalidTransition {
                from: contract.state.wire_name().into(),
                to: "disputed".into(),
            });
        }
        let instruction = Envelope::new(
            client.identity.did().clone(),
            self.node_did(),
            Kind::PayDispute,
            json!({ "reason": reason }),
        )
        .for_contract(contract_id)
        .sign(&client.identity);
        let escrow = self
            .escrows
            .get_mut(contract_id)
            .ok_or_else(|| Error::EscrowViolation("no escrow for contract".into()))?;
        let receipt = escrow.dispute(&instruction)?;
        let escrow_state = escrow.state();
        // Track both sides: raising a dispute is not itself a fault
        // (that depends on the outcome), and receiving one must never
        // damage an agent by itself — otherwise disputing a competitor
        // becomes a cheap way to tarnish it (RFC-0015 §3.3).
        self.disputes
            .entry(contract.client.to_string())
            .or_default()
            .raised += 1;
        self.disputes
            .entry(contract.provider.to_string())
            .or_default()
            .received += 1;
        let escrow_held = escrow.held();
        let mut contract = contract;
        contract.transition(ContractState::Disputed)?;
        self.contracts.insert(contract_id.into(), contract.clone());
        self.persist_contract(&contract);
        self.persist_escrow(contract_id, escrow_state, escrow_held, &receipt.currency);
        self.record(
            "ctr.dispute",
            json!({ "contract_id": contract_id, "reason": reason }),
        );
        Ok(json!({ "state": "disputed", "amount": receipt.amount }))
    }

    /// Arbitrator (the node) executes a signed ruling: splits disputed
    /// funds between client and provider. Fractions must sum to 1.0.
    pub fn escrow_rule(
        &mut self,
        contract_id: &str,
        client_share: f64,
        provider_share: f64,
    ) -> Result<Value> {
        let ruling = Envelope::new(
            self.node_did(),
            self.node_did(),
            Kind::CtrRuling,
            json!({ "split": { "client": client_share, "provider": provider_share } }),
        )
        .for_contract(contract_id)
        .sign(&self.node.identity);
        let node_did = self.node_did();
        let escrow = self
            .escrows
            .get_mut(contract_id)
            .ok_or_else(|| Error::EscrowViolation("no escrow for contract".into()))?;
        let receipt = escrow.rule(&ruling, &node_did)?;
        if let Some(contract) = self.contracts.get_mut(contract_id) {
            contract.transition(ContractState::Ruled)?;
            let saved = contract.clone();
            self.persist_contract(&saved);
        }
        self.persist_escrow(
            contract_id,
            crate::payment::EscrowState::Ruled,
            crate::amount::Amount::ZERO,
            &receipt.currency,
        );
        self.record(
            "pay.ruled",
            json!({ "contract_id": contract_id, "client_share": client_share, "provider_share": provider_share }),
        );
        // The ruling is the attested outcome: a majority share to the
        // provider counts as a success, otherwise as a failure.
        if let Some(c) = self.contracts.get(contract_id).cloned() {
            self.credit_reputation(&c.provider, provider_share >= 0.5, false);
            self.record_job(
                &c.provider,
                &c.client,
                contract_id,
                &c.capability_id,
                "ruled",
                false,
            );
            // The outcome is what makes a dispute honest or abusive.
            if client_share > provider_share {
                self.disputes
                    .entry(c.client.to_string())
                    .or_default()
                    .raised_won += 1;
                self.disputes
                    .entry(c.provider.to_string())
                    .or_default()
                    .received_lost += 1;
            }
            // A human has now ruled: the case is closed.
            self.escalations.remove(contract_id);
        }
        Ok(json!({
            "state": "ruled",
            "client_share": client_share,
            "provider_share": provider_share,
            "amount": receipt.amount,
        }))
    }

    /// Create and validate a signed workflow manifest for the sponsor.
    pub fn create_workflow(&mut self, sponsor_token: &str, body: &Value) -> Result<Value> {
        let sponsor = self.agent_by_token(sponsor_token)?;
        let name = body
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("workflow");
        let inputs: HashMap<String, Value> = body
            .get("inputs")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let steps = body
            .get("steps")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .ok_or_else(|| Error::Other("workflow steps required".into()))?;
        let budget: Option<Budget> = body
            .get("budget")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        let on_failure: FailureMode = body
            .get("on_failure")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or(FailureMode::Abort);
        let expires_in = body
            .get("expires_in_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(86_400);
        let workflow = Workflow::create(
            &sponsor.identity,
            name,
            inputs,
            steps,
            budget,
            on_failure,
            expires_in,
        );
        workflow.validate()?;
        let id = workflow.workflow_id.clone();
        self.workflows
            .insert(id.clone(), (workflow, WorkflowEngine::new()));
        self.record("node.workflow.create", json!({ "workflow_id": id }));
        Ok(json!({ "workflow_id": id, "state": "pending" }))
    }

    /// Return a workflow's materialized step status.
    pub fn workflow_status(&self, workflow_id: &str) -> Result<Value> {
        let (workflow, engine) = self
            .workflows
            .get(workflow_id)
            .ok_or_else(|| Error::Other(format!("unknown workflow: {workflow_id}")))?;
        let steps: Vec<Value> = workflow
            .steps
            .iter()
            .map(|step| {
                json!({
                    "step_id": step.step_id,
                    "state": engine.state_of(&step.step_id),
                    "needs": step.needs,
                    "capability": step.capability,
                })
            })
            .collect();
        Ok(json!({
            "workflow_id": workflow.workflow_id,
            "state": workflow.state,
            "steps": steps,
        }))
    }

    /// Export the caller's portable node-held data.
    pub fn identity_export(&mut self, token: &str) -> Result<Value> {
        let did = self.agent_by_token(token)?.identity.did().to_string();
        let contracts: Vec<_> = self
            .contracts
            .values()
            .filter(|c| c.client.to_string() == did || c.provider.to_string() == did)
            .cloned()
            .collect();
        let events = self.storage.events_after(0, 1000).unwrap_or_default();
        self.record("node.identity.export", json!({ "did": did }));
        Ok(json!({
            "did": did,
            "contracts": contracts,
            "events": events,
            "exported_at": now_unix(),
        }))
    }

    /// Record an event on the audit spine.
    pub fn record(&mut self, kind: &str, payload: Value) {
        // Fan the event out to subscribers (RFC-0013). This only
        // enqueues — network I/O happens in `drain_outbox`, outside the
        // state lock, so delivery can never stall the protocol.
        if let Ok(seq) = self.storage.append_event(kind, payload.clone()) {
            self.enqueue_event(seq, kind, payload);
        }
    }

    /// Queue one event for every subscription that is active, wants the
    /// kind, and is **in scope** for its owner (RFC-0013 §3.2 rule 2:
    /// an agent never receives events for contracts it is not a party
    /// to — this is an access-control boundary, not a filter).
    fn enqueue_event(&mut self, seq: u64, kind: &str, payload: Value) {
        if self.subscriptions.is_empty() {
            return;
        }
        let event = crate::delivery::DeliveredEvent {
            seq,
            kind: kind.to_string(),
            payload: payload.clone(),
            at: now_unix(),
        };
        let node_identity = self.node.identity.clone();
        let mut queued = Vec::new();
        for sub in self.subscriptions.values() {
            if !sub.active || !sub.wants(kind) {
                continue;
            }
            if !self.event_in_scope(&sub.agent_did, &payload) {
                continue;
            }
            if sub.transport != crate::delivery::Transport::Webhook {
                continue; // stream subscribers pull via /v1/events
            }
            let mut body = crate::delivery::DeliveryBody::new(
                &node_identity,
                &sub.subscription_id,
                event.clone(),
            );
            body.sign(&node_identity);
            queued.push(crate::delivery::PendingDelivery {
                subscription_id: sub.subscription_id.clone(),
                url: sub.url.clone(),
                body,
                not_before: 0,
            });
        }
        self.outbox.extend(queued);
    }

    /// Whether `agent` may see an event with this payload: it must
    /// reference a contract the agent is a party to, or name the agent
    /// directly. Events with neither are node-lifecycle events and are
    /// visible to all authenticated subscribers.
    fn event_in_scope(&self, agent: &crate::identity::Did, payload: &Value) -> bool {
        let did = agent.to_string();
        if let Some(cid) = payload.get("contract_id").and_then(|v| v.as_str()) {
            return match self.contracts.get(cid) {
                Some(c) => c.client.to_string() == did || c.provider.to_string() == did,
                // Unknown contract: fail closed rather than leak.
                None => false,
            };
        }
        if let Some(other) = payload.get("agent_did").and_then(|v| v.as_str()) {
            return other == did;
        }
        true
    }

    /// Attach a delivery judge explicitly (tests, or a non-hosted judge).
    pub fn set_verifier(&mut self, verifier: Box<dyn crate::verifier::Verifier>) {
        self.verifier = Some(verifier);
    }

    /// Attach the second, independent judge (RFC-0015).
    pub fn set_second_verifier(&mut self, verifier: Box<dyn crate::verifier::Verifier>) {
        self.verifier_b = Some(verifier);
    }

    /// The configured judge's name, for the docs page.
    pub fn verifier_name(&self) -> Option<String> {
        self.verifier.as_ref().map(|v| v.name())
    }

    /// The admin token, for UI gating.
    pub fn admin_token_ref(&self) -> Option<&str> {
        self.admin_token.as_deref()
    }

    /// Whether a judge is configured (reported by the agent card).
    pub fn has_verifier(&self) -> bool {
        self.verifier.is_some()
    }

    /// Verify a delivery against the contract's acceptance criteria
    /// (RFC-0014). Either party may ask; the verdict is signed by the
    /// node and appended to the spine, so neither can quietly discard
    /// one it dislikes.
    ///
    /// `content` is what the client says it received: supplying it lets
    /// the node recompute the digest and prove integrity, rather than
    /// taking the provider's committed hash on faith.
    pub fn verify_delivery(
        &mut self,
        token: &str,
        contract_id: &str,
        content: Option<&str>,
    ) -> Result<Value> {
        let caller = self.agent_by_token(token)?.identity.did().clone();
        let contract = self
            .contracts
            .get(contract_id)
            .cloned()
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.client != caller && contract.provider != caller {
            return Err(Error::Unauthorized(
                "only the contract's parties may request verification".into(),
            ));
        }
        if contract.state != ContractState::Delivered {
            return Err(Error::InvalidTransition {
                from: contract.state.wire_name().into(),
                to: "verified".into(),
            });
        }

        // A contract that negotiated confidentiality, or carries a
        // compliance context, must never have its content leave the
        // node — enforced here, not left to the judge.
        let confidential = contract.terms.confidentiality.is_some();
        let evidence = crate::verifier::Evidence {
            contract_id: contract_id.to_string(),
            capability_id: contract.capability_id.clone(),
            acceptance_criteria: contract.terms.acceptance_criteria.clone(),
            deadline: contract.terms.deadline,
            delivered_at: now_unix(),
            declared_hash: contract.deliverable_hash.clone().unwrap_or_default(),
            computed_hash: content.map(|c| format!("sha256:{}", crate::sha256_hex(c.as_bytes()))),
            deliverable_excerpt: if confidential {
                None
            } else {
                content.map(|c| c.to_string())
            },
            confidential,
        };

        // The parties may negotiate a value above which a human looks,
        // whatever the judges concluded; otherwise the operator default.
        let threshold = contract
            .terms
            .human_review_above
            .clone()
            .or_else(|| std::env::var("GAP_HUMAN_REVIEW_ABOVE").ok())
            .and_then(|v| crate::amount::Amount::parse(&v).ok());
        let value = crate::amount::Amount::from_f64_rounding(
            contract
                .terms
                .price
                .cap
                .unwrap_or(contract.terms.price.amount),
        );
        let escalate = match threshold {
            Some(t) if value >= t => Some(crate::verifier::Escalation::ValueThreshold),
            _ => None,
        };

        let mut panel: Vec<&dyn crate::verifier::Verifier> = Vec::new();
        if let Some(v) = self.verifier.as_deref() {
            panel.push(v);
        }
        if let Some(v) = self.verifier_b.as_deref() {
            panel.push(v);
        }
        let verdict =
            crate::verifier::verify_panel(&self.node.identity, &evidence, &panel, escalate);
        if let Some(e) = verdict.escalation {
            self.escalations.insert(contract_id.to_string(), e);
        }
        self.verdicts
            .insert(contract_id.to_string(), verdict.clone());
        self.record(
            "exe.verify",
            json!({
                "contract_id": contract_id,
                "ruling": verdict.ruling.as_str(),
                "model": verdict.model,
                "escalation": verdict.escalation.map(|e| e.as_str()),
                "opinions": verdict.opinions.len(),
                "evidence_digest": verdict.evidence_digest,
            }),
        );
        Ok(serde_json::to_value(&verdict).unwrap_or_default())
    }

    /// Maximum rework attempts (spec 03 §3.5). One.
    pub const MAX_REMEDIES: u8 = 1;

    /// `ctr.remedy` — the provider fixes the work and resubmits, once.
    ///
    /// A failed verification should not end the deal: the honest case
    /// is a provider that misread a criterion and can correct it in
    /// seconds. But the retry must be bounded — with unlimited
    /// attempts a provider can grind against the judges until a
    /// borderline reading passes, which turns verification into a
    /// slot machine.
    pub fn remedy(
        &mut self,
        provider_token: &str,
        contract_id: &str,
        deliverable_hash: &str,
    ) -> Result<Value> {
        let provider_did = self.agent_by_token(provider_token)?.identity.did().clone();
        let verdict_was_bad = self
            .verdicts
            .get(contract_id)
            .map(|v| v.ruling == crate::verifier::Ruling::Nonconforming)
            .unwrap_or(false);
        let contract = self
            .contracts
            .get_mut(contract_id)
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.provider != provider_did {
            return Err(Error::Unauthorized("only the provider may remedy".into()));
        }
        if !verdict_was_bad {
            return Err(Error::Other(
                "nothing to remedy: no non-conforming verdict on this contract".into(),
            ));
        }
        if contract.remedies_used >= Self::MAX_REMEDIES {
            return Err(Error::Other(format!(
                "remedy already used ({} allowed); the remedy window is closed — dispute or refund",
                Self::MAX_REMEDIES
            )));
        }
        if deliverable_hash.trim().is_empty() {
            return Err(Error::Other("deliverable_hash required".into()));
        }
        contract.remedies_used += 1;
        contract.deliverable_hash = Some(deliverable_hash.to_string());
        if contract.state == ContractState::Disputed {
            contract.transition(ContractState::Delivered)?;
        }
        let saved = contract.clone();
        let remaining = Self::MAX_REMEDIES - saved.remedies_used;
        self.persist_contract(&saved);
        // The stale verdict must go: it judged the previous artifact.
        self.verdicts.remove(contract_id);
        self.escalations.remove(contract_id);
        self.record(
            "ctr.remedy",
            json!({ "contract_id": contract_id, "attempts_left": remaining }),
        );
        Ok(json!({
            "state": saved.state.wire_name(),
            "remedies_used": saved.remedies_used,
            "attempts_left": remaining,
            "note": "resubmitted; ask for verification again"
        }))
    }

    /// The verdict recorded for a contract, if any.
    pub fn verdict_of(&self, contract_id: &str) -> Option<&crate::verifier::Verdict> {
        self.verdicts.get(contract_id)
    }

    /// Append an entry to an agent's public track record.
    fn record_job(
        &mut self,
        agent: &crate::identity::Did,
        counterparty: &crate::identity::Did,
        contract_id: &str,
        capability_id: &str,
        outcome: &str,
        on_time: bool,
    ) {
        let remedied = self
            .contracts
            .get(contract_id)
            .map(|c| c.remedies_used > 0)
            .unwrap_or(false);
        let verdict = self.verdicts.get(contract_id);
        let record = JobRecord {
            job_ref: pseudonym(contract_id),
            capability_id: capability_id.to_string(),
            counterparty_ref: pseudonym(&counterparty.to_string()),
            outcome: outcome.to_string(),
            remedied,
            verdict: verdict.map(|v| v.ruling.as_str().to_string()),
            judged_by: verdict.and_then(|v| v.model.clone()),
            on_time,
            at: now_unix(),
            seq: self.storage.event_count().unwrap_or(0),
        };
        let job_ref = record.job_ref.clone();
        self.jobs.entry(agent.to_string()).or_default().push(record);
        // job_ref -> contract, so a pseudonymous reference can be
        // resolved to its verdict without ever publishing the id.
        self.jobs_by_ref.insert(job_ref, contract_id.to_string());
    }

    /// An agent's public reputation: aggregate score plus the
    /// pseudonymous job history behind it (RFC-0014 §5). Unauthenticated
    /// on purpose — a track record you cannot read before hiring is not
    /// a track record.
    pub fn reputation_of(&self, did: &str) -> Result<Value> {
        let did = crate::identity::Did::parse(did)?;
        let key = did.to_string();
        let token = self.agents_by_did.get(&key);
        let (executions, successes, on_time, endorsements) =
            match token.and_then(|t| self.agents.get(t)) {
                Some(agent) => {
                    let r = agent.identity.reputation();
                    (r.executions, r.successes, r.on_time, r.endorsements.len())
                }
                // Unknown to this node: report an empty record rather than
                // an error, so a client can tell "no history here" apart
                // from "this DID is invalid".
                None => (0, 0, 0, 0),
            };
        let jobs = self.jobs.get(&key).cloned().unwrap_or_default();
        let d = self.disputes.get(&key).cloned().unwrap_or_default();
        let smoothed = (successes as f64 + 1.0) / (executions as f64 + 2.0);
        Ok(json!({
            "agent_did": key,
            "score": {
                "success_rate": smoothed,
                "raw_success_rate": if executions == 0 { 1.0 } else { successes as f64 / executions as f64 },
                "on_time_rate": if executions == 0 { 1.0 } else { on_time as f64 / executions as f64 },
                "n": executions,
                "note": "success_rate is Laplace-smoothed: a new agent scores 0.5, not 1.0"
            },
            "endorsements": endorsements,
            "disputes": {
                "raised": d.raised,
                "raised_won": d.raised_won,
                "win_rate": d.win_rate(),
                "received": d.received,
                "received_lost": d.received_lost,
                "note": "win_rate is the abuse signal: disputing often and losing is what counts against an agent, not being disputed"
            },
            "jobs": jobs,
            "verified_by_node": self.node_did().to_string(),
        }))
    }

    /// `ctr.counter` — revise the terms of a draft (spec 03 §3.3).
    ///
    /// Counter-offers were specified from v0.1 and never implemented:
    /// the node could only propose and accept, so a provider who wanted
    /// a different price had to refuse and start over. A counter
    /// replaces the terms and re-signs, keeping the contract id and the
    /// negotiation history on the spine.
    pub fn counter_contract(
        &mut self,
        token: &str,
        contract_id: &str,
        terms: Terms,
    ) -> Result<Value> {
        let did = self.agent_by_token(token)?.identity.did().clone();
        self.check_veto(&did, contract_id)?;
        let identity = self.agent_by_token(token)?.identity.clone();
        let contract = self
            .contracts
            .get_mut(contract_id)
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.client != did && contract.provider != did {
            return Err(Error::Unauthorized("not a party to this contract".into()));
        }
        if contract.state != ContractState::Draft {
            return Err(Error::InvalidTransition {
                from: contract.state.wire_name().into(),
                to: "countered".into(),
            });
        }
        // New terms invalidate both signatures — nobody edits a live
        // document (spec 03 §3.3 rule 2). The counter-party signs the
        // revision, and the other side must accept it afresh.
        contract.terms = terms;
        contract.client_sig = None;
        contract.provider_sig = None;
        contract.resign_as(&identity)?;
        let saved = contract.clone();
        self.persist_contract(&saved);
        self.record(
            "ctr.counter",
            json!({ "contract_id": contract_id, "by": did.to_string() }),
        );
        Ok(json!({ "contract_id": contract_id, "state": "draft", "countered_by": did.to_string() }))
    }

    /// `ctr.reject` — decline the current terms (spec 03 §3.3).
    pub fn reject_contract(
        &mut self,
        token: &str,
        contract_id: &str,
        reason: &str,
    ) -> Result<Value> {
        let did = self.agent_by_token(token)?.identity.did().clone();
        let contract = self
            .contracts
            .get_mut(contract_id)
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.client != did && contract.provider != did {
            return Err(Error::Unauthorized("not a party to this contract".into()));
        }
        contract.transition(ContractState::Rejected)?;
        let saved = contract.clone();
        self.persist_contract(&saved);
        self.record(
            "ctr.reject",
            json!({ "contract_id": contract_id, "by": did.to_string(), "reason": reason }),
        );
        Ok(json!({ "contract_id": contract_id, "state": "rejected" }))
    }

    /// `ctr.cancel` — call the deal off before execution (spec 03 §3.3
    /// rule 3: valid only while nothing has been executed).
    pub fn cancel_contract(
        &mut self,
        token: &str,
        contract_id: &str,
        reason: &str,
    ) -> Result<Value> {
        let did = self.agent_by_token(token)?.identity.did().clone();
        let contract = self
            .contracts
            .get_mut(contract_id)
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.client != did && contract.provider != did {
            return Err(Error::Unauthorized("not a party to this contract".into()));
        }
        contract.transition(ContractState::Cancelled)?;
        let saved = contract.clone();
        self.persist_contract(&saved);
        // Parked funds go home: a cancelled deal must not strand money.
        let refunded = {
            let node_did = self.node_did();
            let client_identity = self
                .agents_by_did
                .get(&saved.client.to_string())
                .and_then(|t| self.agents.get(t))
                .map(|a| a.identity.clone());
            match (client_identity, self.escrows.get_mut(contract_id)) {
                (Some(identity), Some(escrow)) => {
                    let instruction = Envelope::new(
                        identity.did().clone(),
                        node_did,
                        Kind::PayRefund,
                        json!({ "reason": "contract cancelled" }),
                    )
                    .for_contract(contract_id)
                    .sign(&identity);
                    escrow.refund(&instruction).is_ok()
                }
                _ => false,
            }
        };
        self.record(
            "ctr.cancel",
            json!({ "contract_id": contract_id, "by": did.to_string(), "reason": reason, "refunded": refunded }),
        );
        Ok(json!({ "contract_id": contract_id, "state": "cancelled", "escrow_refunded": refunded }))
    }

    /// `exe.start` — the provider announces it has begun, with a plan
    /// and an ETA (spec 04 §4.2). Until now a contract jumped straight
    /// from signed to delivered and the client was blind in between.
    pub fn start_execution(
        &mut self,
        provider_token: &str,
        contract_id: &str,
        plan: &Value,
    ) -> Result<Value> {
        let did = self.agent_by_token(provider_token)?.identity.did().clone();
        self.check_veto(&did, contract_id)?;
        let contract = self
            .contracts
            .get_mut(contract_id)
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.provider != did {
            return Err(Error::Unauthorized("only the provider may start".into()));
        }
        contract.transition(ContractState::Executing)?;
        let saved = contract.clone();
        self.persist_contract(&saved);
        self.record(
            "exe.start",
            json!({ "contract_id": contract_id, "plan": plan }),
        );
        Ok(json!({ "state": "executing" }))
    }

    /// `exe.progress` — a heartbeat while the work runs (spec 04 §4.2).
    pub fn report_progress(
        &mut self,
        provider_token: &str,
        contract_id: &str,
        step: u64,
        note: &str,
    ) -> Result<Value> {
        let did = self.agent_by_token(provider_token)?.identity.did().clone();
        let contract = self
            .contracts
            .get(contract_id)
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.provider != did {
            return Err(Error::Unauthorized("only the provider may report".into()));
        }
        if contract.state != ContractState::Executing {
            return Err(Error::InvalidTransition {
                from: contract.state.wire_name().into(),
                to: "progress".into(),
            });
        }
        self.record(
            "exe.progress",
            json!({ "contract_id": contract_id, "step": step, "note": note }),
        );
        Ok(json!({ "state": "executing", "step": step }))
    }

    /// `cap.deregister` — withdraw from the registry (spec 02 §2.5).
    pub fn deregister(&mut self, token: &str) -> Result<Value> {
        let did = self.agent_by_token(token)?.identity.did().clone();
        self.registry.deregister(&did);
        if let Some(agent) = self.agents.get_mut(token) {
            agent.announcement = None;
        }
        self.record("cap.deregister", json!({ "agent_did": did.to_string() }));
        Ok(json!({ "deregistered": did.to_string() }))
    }

    /// Register a bilateral principal binding (spec 01 §1.3). The two
    /// signatures are the authority; no bearer token is involved.
    pub fn bind_principal(&mut self, body: &Value) -> Result<Value> {
        let binding: crate::principal::PrincipalBinding =
            serde_json::from_value(body.clone()).map_err(|e| Error::Json(e.to_string()))?;
        binding.verify()?;
        let agent = binding.agent_did.to_string();
        self.bindings.insert(agent.clone(), binding);
        self.record("principal.bind", json!({ "agent_did": agent }));
        Ok(json!({ "bound": agent }))
    }

    /// Release an agent from its binding, and with it any veto or
    /// budget that depended on it.
    pub fn unbind_principal(&mut self, body: &Value) -> Result<Value> {
        let unbind: crate::principal::Unbind =
            serde_json::from_value(body.clone()).map_err(|e| Error::Json(e.to_string()))?;
        unbind.verify()?;
        let agent = unbind.agent_did.to_string();
        match self.bindings.get(&agent) {
            Some(b) if b.principal_did == unbind.principal_did => {}
            Some(_) => return Err(Error::Unauthorized("not this agent's principal".into())),
            None => return Err(Error::Other("no binding for this agent".into())),
        }
        self.bindings.remove(&agent);
        self.vetoes.remove(&agent);
        self.budgets.remove(&agent);
        self.record("principal.unbind", json!({ "agent_did": agent }));
        Ok(json!({ "unbound": agent }))
    }

    /// A principal vetoes its agent (spec 06 §6.5). Inalienable: it
    /// needs no cooperation from the agent, and no contract can waive it.
    pub fn principal_veto(&mut self, body: &Value) -> Result<Value> {
        let veto: crate::principal::Veto =
            serde_json::from_value(body.clone()).map_err(|e| Error::Json(e.to_string()))?;
        let agent = veto.agent_did.to_string();
        let binding = self
            .bindings
            .get(&agent)
            .ok_or_else(|| Error::Unauthorized("agent has no principal binding".into()))?;
        veto.verify_for(binding)?;
        let scope = veto.scope.clone();
        self.vetoes.entry(agent.clone()).or_default().push(veto);
        self.record(
            "gov.halt",
            json!({ "agent_did": agent, "scope": scope, "source": "principal" }),
        );
        Ok(json!({ "vetoed": agent, "scope": scope }))
    }

    /// A principal sets its agent's hard daily spending cap.
    pub fn principal_budget(&mut self, body: &Value) -> Result<Value> {
        let grant: crate::principal::BudgetGrant =
            serde_json::from_value(body.clone()).map_err(|e| Error::Json(e.to_string()))?;
        let agent = grant.agent_did.to_string();
        let binding = self
            .bindings
            .get(&agent)
            .ok_or_else(|| Error::Unauthorized("agent has no principal binding".into()))?;
        grant.verify_for(binding)?;
        grant.cap()?;
        let per_day = grant.per_day.clone();
        self.budgets.insert(agent.clone(), grant);
        self.record(
            "gov.certify",
            json!({ "agent_did": agent, "per_day": per_day, "kind": "budget" }),
        );
        Ok(json!({ "agent_did": agent, "per_day": per_day }))
    }

    /// Refuse anything a vetoed agent tries to do. Called on every
    /// state-changing action, because a veto that only covered some
    /// routes would not be a veto.
    fn check_veto(&self, agent: &crate::identity::Did, contract_id: &str) -> Result<()> {
        if let Some(vetoes) = self.vetoes.get(&agent.to_string()) {
            if let Some(v) = vetoes.iter().find(|v| v.covers(contract_id)) {
                return Err(Error::AutonomyViolation(format!(
                    "principal veto in force ({}): {}",
                    v.scope, v.reason
                )));
            }
        }
        Ok(())
    }

    /// Enforce the principal's daily budget before funds are committed.
    fn check_budget(
        &mut self,
        agent: &crate::identity::Did,
        amount: crate::amount::Amount,
    ) -> Result<()> {
        let key = agent.to_string();
        let Some(grant) = self.budgets.get(&key) else {
            return Ok(());
        };
        let cap = grant.cap()?;
        let day = now_unix() / 86_400;
        let spent = self
            .spend_today
            .get(&(key.clone(), day))
            .copied()
            .unwrap_or(crate::amount::Amount::ZERO);
        let after = spent.checked_add(amount)?;
        if after > cap {
            return Err(Error::AutonomyViolation(format!(
                "principal budget exceeded: {after} would pass the {cap} daily cap"
            )));
        }
        self.spend_today.insert((key, day), after);
        Ok(())
    }

    /// The principal-rights view of an agent (spec 06 §6.5).
    pub fn principal_status(&self, did: &str) -> Result<Value> {
        let did = crate::identity::Did::parse(did)?.to_string();
        Ok(json!({
            "agent_did": did,
            "bound": self.bindings.get(&did).map(|b| json!({
                "principal": b.principal.name,
                "principal_did": b.principal_did.to_string(),
                "autonomy_grant": format!("{:?}", b.autonomy_grant),
                "expires_at": b.expires_at,
            })),
            "vetoes": self.vetoes.get(&did).map(|v| v.iter()
                .map(|x| json!({ "scope": x.scope, "reason": x.reason, "at": x.at }))
                .collect::<Vec<_>>()).unwrap_or_default(),
            "budget": self.budgets.get(&did).map(|g| json!({
                "per_day": g.per_day, "currency": g.currency
            })),
        }))
    }

    /// Cases waiting on a human (RFC-0015). Admin-only: this is the
    /// operator's work queue, not public data.
    pub fn escalations(&self) -> Value {
        let items: Vec<Value> = self
            .escalations
            .iter()
            .map(|(cid, why)| {
                let v = self.verdicts.get(cid);
                json!({
                    "contract_id": cid,
                    "reason": why.as_str(),
                    "ruling": v.map(|v| v.ruling.as_str()),
                    "opinions": v.map(|v| v.opinions.clone()),
                    "evidence_digest": v.map(|v| v.evidence_digest.clone()),
                })
            })
            .collect();
        json!({ "escalations": items, "count": items.len() })
    }

    /// Register a delivery subscription for the authenticated agent.
    pub fn subscribe(&mut self, token: &str, body: &Value) -> Result<Value> {
        let agent_did = self.agent_by_token(token)?.identity.did().clone();
        let transport = crate::delivery::Transport::parse(
            body.get("transport")
                .and_then(|v| v.as_str())
                .unwrap_or("webhook"),
        )?;
        let url = body
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if transport == crate::delivery::Transport::Webhook {
            if url.is_empty() {
                return Err(Error::Other("webhook subscription requires a url".into()));
            }
            // SSRF boundary: refuse before storing, so a hostile URL
            // never reaches the outbox (RFC-0013 §4).
            crate::delivery::validate_webhook_url(&url)?;
        }
        let kinds: Vec<String> = body
            .get("kinds")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|k| k.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let sub = crate::delivery::Subscription::new(agent_did, transport, url, kinds);
        let view = sub.to_json();
        let id = sub.subscription_id.clone();
        self.subscriptions.insert(id.clone(), sub);
        self.record("node.sub.register", json!({ "subscription_id": id }));
        Ok(view)
    }

    /// List the caller's subscriptions.
    pub fn list_subscriptions(&self, token: &str) -> Result<Value> {
        let did = self.agent_by_token(token)?.identity.did().to_string();
        let subs: Vec<Value> = self
            .subscriptions
            .values()
            .filter(|s| s.agent_did.to_string() == did)
            .map(|s| s.to_json())
            .collect();
        Ok(json!({ "subscriptions": subs }))
    }

    /// Delete a subscription. Only its owner may delete it.
    pub fn unsubscribe(&mut self, token: &str, id: &str) -> Result<Value> {
        let did = self.agent_by_token(token)?.identity.did().to_string();
        match self.subscriptions.get(id) {
            Some(s) if s.agent_did.to_string() == did => {
                self.subscriptions.remove(id);
                self.record("node.sub.delete", json!({ "subscription_id": id }));
                Ok(json!({ "deleted": id }))
            }
            Some(_) => Err(Error::Unauthorized(
                "subscription belongs to another agent".into(),
            )),
            None => Err(Error::Other("unknown subscription".into())),
        }
    }

    /// Events visible to the caller after `after`, for the SSE stream
    /// and for cursor catch-up.
    pub fn events_for(&self, token: &str, after: u64, limit: u64) -> Result<Vec<Value>> {
        let did = self.agent_by_token(token)?.identity.did().clone();
        let events = self.storage.events_after(after, limit).unwrap_or_default();
        Ok(events
            .into_iter()
            .filter(|e| self.event_in_scope(&did, &e.payload))
            .map(|e| json!({ "seq": e.seq, "kind": e.kind, "payload": e.payload, "at": e.at }))
            .collect())
    }

    /// Take the deliveries that are due now, leaving the rest queued.
    /// Called by `drain_outbox` while holding the lock — deliberately
    /// cheap, so the network round-trips happen unlocked.
    pub fn take_due_deliveries(&mut self, now: u64) -> Vec<crate::delivery::PendingDelivery> {
        let mut due = Vec::new();
        let mut kept = Vec::new();
        for d in self.outbox.drain(..) {
            if d.not_before <= now {
                due.push(d);
            } else {
                kept.push(d);
            }
        }
        self.outbox = kept;
        due
    }

    /// Apply a delivery outcome: reset or increment the failure count,
    /// re-queue with backoff, and disable a subscription that keeps
    /// failing (RFC-0013 §3.4).
    pub fn settle_delivery(
        &mut self,
        mut pending: crate::delivery::PendingDelivery,
        success: bool,
    ) {
        let id = pending.subscription_id.clone();
        if success {
            if let Some(sub) = self.subscriptions.get_mut(&id) {
                sub.failures = 0;
            }
            return;
        }
        if pending.body.attempt < crate::delivery::MAX_ATTEMPTS {
            let attempt = pending.body.attempt + 1;
            pending.body.attempt = attempt;
            // The signature covers `attempt`, so it must be renewed.
            pending.body.sign(&self.node.identity);
            pending.not_before = now_unix() + crate::delivery::backoff_secs(attempt);
            self.outbox.push(pending);
            return;
        }
        // Attempts exhausted for this event: count it against the
        // subscription and disable it if it keeps failing.
        let mut disable = false;
        if let Some(sub) = self.subscriptions.get_mut(&id) {
            sub.failures += 1;
            if sub.failures >= crate::delivery::MAX_CONSECUTIVE_FAILURES {
                sub.active = false;
                disable = true;
            }
        }
        if disable {
            self.record(
                "node.sub.disable",
                json!({ "subscription_id": id, "reason": "consecutive delivery failures" }),
            );
        }
    }

    /// Number of queued deliveries (tests and operators).
    pub fn outbox_len(&self) -> usize {
        self.outbox.len()
    }

    /// Read a subscription (tests and operators).
    pub fn subscription(&self, id: &str) -> Option<&crate::delivery::Subscription> {
        self.subscriptions.get(id)
    }

    /// Public directory data for the web UI: every live announcement
    /// with its agent's score, plus recent settled jobs.
    pub fn public_directory(&self) -> Value {
        self.public_directory_filtered("", None, None)
    }

    /// The directory, filtered server-side: a search that only works
    /// after JavaScript runs is a search a crawler never sees.
    pub fn public_directory_filtered(
        &self,
        q: &str,
        min_score: Option<f64>,
        max_price: Option<f64>,
    ) -> Value {
        let anns = self.registry.query(&Query::default());
        let needle = q.trim().to_lowercase();
        let agents: Vec<Value> = anns
            .iter()
            .filter(|a| {
                if needle.is_empty() {
                    return true;
                }
                a.capabilities.iter().any(|c| {
                    c.name.to_lowercase().contains(&needle)
                        || c.description.to_lowercase().contains(&needle)
                        || c.id.to_lowercase().contains(&needle)
                }) || a.agent_did.to_string().contains(&needle)
            })
            .filter(|a| {
                max_price.is_none_or(|max| {
                    a.capabilities
                        .iter()
                        .any(|c| c.price.as_ref().map(|p| p.amount <= max).unwrap_or(true))
                })
            })
            .map(|a| {
                let did = a.agent_did.to_string();
                let rep = self
                    .agents_by_did
                    .get(&did)
                    .and_then(|t| self.agents.get(t))
                    .map(|ag| ag.identity.reputation().clone())
                    .unwrap_or_default();
                let jobs = self.jobs.get(&did).map(|j| j.len()).unwrap_or(0);
                json!({
                    "did": did,
                    "capabilities": a.capabilities,
                    "languages": a.languages,
                    "regions": a.regions,
                    "reachability": a.reachability,
                    "score": rep.success_rate(),
                    "n": rep.executions,
                    "jobs": jobs,
                })
            })
            .filter(|a| min_score.is_none_or(|min| a["score"].as_f64().unwrap_or(0.0) >= min))
            .collect();
        json!({
            "node": self.node_did().to_string(),
            "query": q,
            "agents": agents,
            "count": agents.len(),
            "verifier": self.verifier.as_ref().map(|v| v.name()),
            "second_verifier": self.verifier_b.as_ref().map(|v| v.name()),
        })
    }

    /// Recent public activity: settled jobs across all agents, already
    /// pseudonymous (RFC-0014 §5), newest first.
    pub fn public_activity_after(&self, after: u64, limit: usize) -> Value {
        let mut all: Vec<Value> = self
            .jobs
            .iter()
            .flat_map(|(agent, records)| {
                records.iter().filter(|r| r.seq > after).map(move |r| {
                    json!({
                        "seq": r.seq,
                        "job_ref": r.job_ref,
                        "agent_ref": pseudonym(agent),
                        "capability_id": r.capability_id,
                        "outcome": r.outcome,
                        "verdict": r.verdict,
                        "judged_by": r.judged_by,
                        "remedied": r.remedied,
                        "on_time": r.on_time,
                        "at": r.at,
                    })
                })
            })
            .collect();
        all.sort_by_key(|v| v["seq"].as_u64().unwrap_or(0));
        all.truncate(limit);
        json!({ "jobs": all, "count": all.len() })
    }

    /// One settled job in public, pseudonymous form: the full verdict —
    /// checks, reasons and each judge's opinion — with the contract id
    /// and both parties stripped. This is what makes a score auditable
    /// rather than merely asserted.
    pub fn public_job(&self, job_ref: &str) -> Result<Value> {
        let contract_id = self
            .jobs_by_ref
            .get(job_ref)
            .ok_or_else(|| Error::Other("unknown job reference".into()))?;
        let record = self
            .jobs
            .values()
            .flatten()
            .find(|r| r.job_ref == job_ref)
            .ok_or_else(|| Error::Other("unknown job reference".into()))?;
        let contract = self.contracts.get(contract_id);
        let verdict = self.verdicts.get(contract_id);
        Ok(json!({
            "job_ref": job_ref,
            "capability_id": record.capability_id,
            "outcome": record.outcome,
            "on_time": record.on_time,
            "remedied": record.remedied,
            "at": record.at,
            // The criteria are public: they are what the verdict judged.
            "acceptance_criteria": contract.map(|c| c.terms.acceptance_criteria.clone()),
            "verdict": verdict.map(|v| json!({
                "ruling": v.ruling.as_str(),
                "reasons": v.reasons,
                "checks": v.checks,
                "opinions": v.opinions,
                "escalation": v.escalation.map(|e| e.as_str()),
                "evidence_digest": v.evidence_digest,
                "evaluated_at": v.evaluated_at,
                "evaluator": v.evaluator,
                "signature": v.signature,
            })),
        }))
    }

    /// The most recent settlements, newest first. Same projection as
    /// the cursor form — one shape, so the page and the live stream can
    /// never disagree about what a settlement looks like.
    pub fn public_activity(&self, limit: usize) -> Value {
        let all = self.public_activity_after(0, usize::MAX);
        let mut jobs = all["jobs"].as_array().cloned().unwrap_or_default();
        jobs.reverse();
        jobs.truncate(limit);
        json!({ "jobs": jobs, "count": jobs.len() })
    }

    /// The node's AgentCard (RFC-0010 well-known discovery).
    pub fn agent_card(&self) -> Value {
        json!({
            "gap_version": crate::VERSION,
            "agent": {
                "did": self.node_did(),
                "name": "GAP Node",
                "description_for_agents": "Reference GAP node: identity, discovery, contracts, escrow.",
                "provider": { "did": self.node_did(), "legal_name": "Geta.Team" }
            },
            "capabilities": [],
            "endpoints": {
                "invoke": "/v1/contract/propose",
                "discover": "/v1/discover",
                "billing": "/v1/escrow/park"
            },
            "auth": ["bearer"],
            "updated_at": now_unix()
        })
    }
}

/// Route an HTTP request to the node state.
/// Returns (status, json body).
pub fn route(
    state: &Arc<Mutex<NodeState>>,
    method: &str,
    path: &str,
    body: &[u8],
    auth: Option<&str>,
) -> (u16, Value) {
    route_with_ip(state, method, path, body, auth, None)
}

/// Serve the web UI (`src/ui`). Returns `None` for non-UI paths.
///
/// HTML lives outside `route()` because that function is the JSON API
/// contract; mixing content types into it would force every caller to
/// sniff. Returns `(status, content_type, body)`.
pub fn route_html(
    state: &Arc<Mutex<NodeState>>,
    method: &str,
    path: &str,
    auth: Option<&str>,
) -> Option<(u16, &'static str, String)> {
    if method != "GET" {
        return None;
    }
    let clean = path.split('?').next().unwrap_or(path);
    let guard = state.lock().ok()?;
    let base =
        std::env::var("GAP_PUBLIC_URL").unwrap_or_else(|_| String::from("http://localhost:8080"));

    let html = |b: String| Some((200u16, "text/html; charset=utf-8", b));
    match clean {
        "/" => {
            let params = parse_url_params(path);
            let q = params.get("q").cloned().unwrap_or_default();
            let min_score = params.get("min_score").and_then(|v| v.parse::<f64>().ok());
            let max_price = params.get("max_price").and_then(|v| v.parse::<f64>().ok());
            let mut dir = guard.public_directory_filtered(&q, min_score, max_price);
            // Echo the filters so the form keeps what the visitor typed.
            dir["min_score"] =
                serde_json::Value::String(params.get("min_score").cloned().unwrap_or_default());
            dir["max_price"] =
                serde_json::Value::String(params.get("max_price").cloned().unwrap_or_default());
            html(crate::ui::directory(&dir))
        }
        "/activity" => {
            let recent = guard.public_activity(50);
            html(crate::ui::activity_page(&recent))
        }
        "/docs" => {
            let did = guard.node_did().to_string();
            let v = guard.verifier_name();
            html(crate::ui::docs_page(&did, v.as_deref()))
        }
        "/robots.txt" => Some((200, "text/plain; charset=utf-8", crate::ui::robots(&base))),
        "/sitemap.xml" => {
            let dir = guard.public_directory();
            Some((
                200,
                "application/xml; charset=utf-8",
                crate::ui::sitemap(&base, &dir),
            ))
        }
        "/admin" => {
            // The console is operator-only; a public escalation queue
            // would leak which deals are in trouble.
            let token = auth.and_then(|a| a.strip_prefix("Bearer "));
            let ok = matches!((guard.admin_token_ref(), token), (Some(a), Some(t)) if a == t);
            if !ok {
                return Some((
                    401,
                    "text/html; charset=utf-8",
                    "<!DOCTYPE html><meta charset=utf-8><title>401</title><body style=\"font-family:system-ui;background:#05070c;color:#edf4ff;padding:40px\"><h1>401</h1><p>Operator token required: <code>Authorization: Bearer $GAP_ADMIN_TOKEN</code>.</p>".into(),
                ));
            }
            let e = guard.escalations();
            let d = guard.public_directory();
            let a = guard.public_activity(1000);
            html(crate::ui::admin_page(&e, &d, &a))
        }
        p if p.starts_with("/job/") => {
            let job_ref = percent_decode(p.trim_start_matches("/job/"));
            let job = guard.public_job(&job_ref).ok()?;
            html(crate::ui::job_page(&job))
        }
        p if p.starts_with("/agent/") => {
            let did = percent_decode(p.trim_start_matches("/agent/"));
            let rep = guard.reputation_of(&did).ok()?;
            let ann = guard
                .registry
                .query(&Query::default())
                .into_iter()
                .find(|a| a.agent_did.to_string() == did)
                .and_then(|a| serde_json::to_value(a).ok());
            html(crate::ui::agent_page(&did, &rep, ann.as_ref()))
        }
        _ => None,
    }
}

/// Send every due webhook delivery (RFC-0013).
///
/// The lock is taken twice — once to take the due batch, once to record
/// outcomes — and **released across the network round-trips**. A slow or
/// hostile subscriber therefore cannot stall contract processing, which
/// is the whole reason delivery is queued rather than inline.
///
/// Returns the number of deliveries attempted.
pub fn drain_outbox(
    state: &Arc<Mutex<NodeState>>,
    sender: &dyn crate::delivery::WebhookSender,
) -> usize {
    let now = crate::message::now_unix();
    let (due, node_did) = {
        let mut guard = match state.lock() {
            Ok(g) => g,
            Err(_) => return 0,
        };
        let did = guard.node_did().to_string();
        (guard.take_due_deliveries(now), did)
    };
    let attempted = due.len();
    for pending in due {
        let bytes = serde_json::to_vec(&pending.body).unwrap_or_default();
        let headers = [
            ("Content-Type", "application/json".to_string()),
            ("X-Gap-Node", node_did.clone()),
            (
                "X-Gap-Signature",
                pending.body.signature.clone().unwrap_or_default(),
            ),
            ("X-Gap-Delivery", pending.body.delivery_id.clone()),
            ("X-Gap-Event-Seq", pending.body.event.seq.to_string()),
        ];
        let outcome = sender.post(&pending.url, &headers, &bytes);
        let success = matches!(outcome, Ok(status) if (200..300).contains(&status));
        if let Ok(mut guard) = state.lock() {
            guard.settle_delivery(pending, success);
        }
    }
    attempted
}

/// Route with client IP for rate limiting (audit H-03).
pub fn route_with_ip(
    state: &Arc<Mutex<NodeState>>,
    method: &str,
    path: &str,
    body: &[u8],
    auth: Option<&str>,
    client_ip: Option<&str>,
) -> (u16, Value) {
    // Parse the JSON body BEFORE taking the state lock: with the worker
    // pool this keeps the global lock's critical section minimal
    // (parsing and serialization are the parallelizable parts).
    let raw_path = path;
    let path = raw_path.split('?').next().unwrap_or(raw_path);
    let body: Value = if body.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(_) => {
                return (
                    400,
                    json!({ "error": { "code": "invalid_request", "message": "invalid JSON body" } }),
                )
            }
        }
    };

    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(_) => {
            return (
                500,
                json!({ "error": { "code": "internal", "message": "state lock poisoned" } }),
            )
        }
    };

    // Rate limiting first (audit H-03): 429 when over limit.
    let token = auth.and_then(|h| h.strip_prefix("Bearer "));
    if guard.check_rate_limit(token, client_ip).is_err() {
        return (
            429,
            json!({ "error": { "code": "rate_limited", "message": "too many requests" } }),
        );
    }

    let response = match (method, path) {
        // ---- health & card ----
        ("GET", "/health") => Ok(json!({ "status": "ok", "node": guard.node_did() })),
        // ---- public directory data (consumed by the web UI) ----
        ("GET", "/v1/directory") => Ok(guard.public_directory()),
        ("GET", p) if p.starts_with("/v1/job/") => {
            let job_ref = percent_decode(p.trim_start_matches("/v1/job/"));
            guard.public_job(&job_ref)
        }
        ("GET", "/v1/activity") => {
            let params = parse_url_params(raw_path);
            let limit = params
                .get("limit")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(50)
                .min(500);
            match params.get("after").and_then(|v| v.parse::<u64>().ok()) {
                Some(after) => Ok(guard.public_activity_after(after, limit)),
                None => Ok(guard.public_activity(limit)),
            }
        }
        ("GET", "/.well-known/gap-agent.json") => Ok(guard.agent_card()),

        // ---- identity ----
        ("POST", "/v1/identity") => {
            let (did, tok) = guard.create_identity();
            Ok(json!({ "did": did, "token": tok }))
        }

        // ---- announce & discover ----
        ("POST", "/v1/announce") => {
            let caps: Vec<Capability> = body
                .get("capabilities")
                .and_then(|c| serde_json::from_value(c.clone()).ok())
                .unwrap_or_default();
            let languages = body
                .get("languages")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let regions = body
                .get("regions")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let ttl = body
                .get("ttl_seconds")
                .and_then(|v| v.as_u64())
                .unwrap_or(86400);
            // Spec 02 §2.2: the agent declares how it can be reached.
            let reachability: Vec<Reachability> = body
                .get("reachability")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            match (token, caps.is_empty()) {
                (Some(t), false) => guard
                    .announce_with_reachability(t, caps, languages, regions, ttl, reachability)
                    .map(|id| json!({ "announcement_id": id })),
                (None, _) => Err(Error::Unauthorized("missing bearer token".into())),
                (_, true) => Err(Error::Other("capabilities required".into())),
            }
        }
        ("GET", "/v1/discover") => {
            let q = parse_query(&body, raw_path);
            let results = guard.discover(&q);
            Ok(json!({ "results": results }))
        }

        // ---- contracts ----
        ("POST", "/v1/contract/propose") => {
            let provider = body.get("provider").and_then(|v| v.as_str()).unwrap_or("");
            let capability_id = body
                .get("capability_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let terms_parsed: Option<Terms> = body
                .get("terms")
                .and_then(|t| serde_json::from_value(t.clone()).ok());
            let escrow = body.get("escrow").and_then(|v| v.as_bool()).unwrap_or(true);
            match (token, terms_parsed) {
                (Some(t), Some(terms)) => guard
                    .propose_contract(t, provider, capability_id, terms, escrow)
                    .map(|id| json!({ "contract_id": id, "state": "draft" })),
                (None, _) => Err(Error::Unauthorized("missing bearer token".into())),
                (_, None) => Err(Error::Other("terms required".into())),
            }
        }
        (m, p) if m == "POST" && p.starts_with("/v1/contract/") && p.ends_with("/accept") => {
            let id = p
                .trim_start_matches("/v1/contract/")
                .trim_end_matches("/accept");
            match token {
                Some(t) => guard
                    .accept_contract(t, id)
                    .map(|_| json!({ "state": "signed" })),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        (m, p) if m == "POST" && p.starts_with("/v1/contract/") && p.ends_with("/deliver") => {
            let id = p
                .trim_start_matches("/v1/contract/")
                .trim_end_matches("/deliver");
            let hash = body
                .get("deliverable_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match token {
                Some(t) => guard
                    .deliver(t, id, hash)
                    .map(|_| json!({ "state": "delivered" })),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        (m, p)
            if m == "POST" && p.starts_with("/v1/contract/") && p.ends_with("/accept-delivery") =>
        {
            let id = p
                .trim_start_matches("/v1/contract/")
                .trim_end_matches("/accept-delivery");
            match token {
                Some(t) => guard.accept_delivery(t, id),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        ("GET", p) if p.starts_with("/v1/contract/") => {
            let id = p.trim_start_matches("/v1/contract/");
            match guard.contracts.get(id) {
                Some(c) => Ok(json!({
                    "contract_id": c.contract_id,
                    "state": c.state.wire_name(),
                    "contract": c,
                    "events": guard.storage.events_after(0, 100).unwrap_or_default()
                        .into_iter()
                        .filter(|e| e.payload.get("contract_id").and_then(|v| v.as_str()) == Some(id))
                        .collect::<Vec<_>>(),
                })),
                None => Err(Error::UnknownContract(id.into())),
            }
        }

        // ---- escrow ----
        ("POST", "/v1/escrow/park") => {
            let id = body
                .get("contract_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let amount = match body.get("amount") {
                Some(v) if v.is_string() => crate::amount::Amount::parse(v.as_str().unwrap_or("")),
                Some(v) if v.is_number() => Ok(crate::amount::Amount::from_f64_rounding(
                    v.as_f64().unwrap_or(0.0),
                )),
                _ => Err(Error::Other("amount required (decimal string)".into())),
            };
            match (token, amount) {
                (Some(t), Ok(amt)) => guard
                    .escrow_park(t, id, &amt)
                    .map(|_| json!({ "receipt": { "event": "pay.parked" } })),
                (None, _) => Err(Error::Unauthorized("missing bearer token".into())),
                (_, Err(e)) => Err(e),
            }
        }

        // ---- audit ----
        ("GET", "/v1/audit") => {
            // Authenticated endpoint: the audit spine is tamper-evident
            // evidence; anonymous read access would leak every contract
            // event (found by the exhaustive route tests).
            let authed = token
                .map(|t| guard.agent_by_token(t).is_ok())
                .unwrap_or(false);
            if !authed {
                Err(Error::Unauthorized("authentication required".into()))
            } else {
                let params = parse_url_params(raw_path);
                let after = params
                    .get("after")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                let limit = params
                    .get("limit")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(100)
                    .min(1000);
                let events = guard.storage.events_after(after, limit).unwrap_or_default();
                Ok(json!({ "events": events }))
            }
        }

        // ---- escrow: explicit settlement routes ----
        ("POST", "/v1/escrow/release") => {
            let id = body
                .get("contract_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match token {
                Some(t) => guard.escrow_release(t, id),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        ("POST", "/v1/escrow/refund") => {
            let id = body
                .get("contract_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match token {
                Some(t) => guard.escrow_refund(t, id),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        ("POST", "/v1/escrow/rule") => {
            let id = body
                .get("contract_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let split = body.get("split").cloned().unwrap_or_default();
            let client_share = split.get("client").and_then(|v| v.as_f64()).unwrap_or(-1.0);
            let provider_share = split
                .get("provider")
                .and_then(|v| v.as_f64())
                .unwrap_or(-1.0);
            match (&guard.admin_token, token) {
                (Some(admin), Some(t)) if admin == t => {
                    guard.escrow_rule(id, client_share, provider_share)
                }
                (None, _) => Err(Error::Unauthorized("admin token not configured".into())),
                _ => Err(Error::Unauthorized("admin token required".into())),
            }
        }
        (m, p) if m == "POST" && p.starts_with("/v1/contract/") && p.ends_with("/dispute") => {
            let id = p
                .trim_start_matches("/v1/contract/")
                .trim_end_matches("/dispute");
            let reason = body
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unspecified");
            match token {
                Some(t) => guard.contract_dispute(t, id, reason),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }

        // ---- workflows ----
        ("POST", "/v1/workflows") => match token {
            Some(t) => guard.create_workflow(t, &body),
            None => Err(Error::Unauthorized("missing bearer token".into())),
        },
        ("GET", p) if p.starts_with("/v1/workflows/") => {
            let id = p.trim_start_matches("/v1/workflows/");
            guard.workflow_status(id)
        }

        // ---- principal rights (spec 01 §1.3, 06 §6.5) ----
        // Signature-authenticated, not token-authenticated: a principal
        // must be able to stop its agent even if the agent's own
        // credentials are compromised.
        ("POST", "/v1/principal/bind") => guard.bind_principal(&body),
        ("POST", "/v1/principal/unbind") => guard.unbind_principal(&body),
        ("POST", "/v1/principal/veto") => guard.principal_veto(&body),
        ("POST", "/v1/principal/budget") => guard.principal_budget(&body),
        ("GET", p) if p.starts_with("/v1/principal/") => {
            let did = percent_decode(p.trim_start_matches("/v1/principal/"));
            guard.principal_status(&did)
        }

        // ---- verification & reputation (RFC-0014 / RFC-0015) ----
        (m, p) if m == "POST" && p.starts_with("/v1/contract/") && p.ends_with("/counter") => {
            let id = p
                .trim_start_matches("/v1/contract/")
                .trim_end_matches("/counter");
            let terms = body.get("terms").and_then(terms_from_json);
            match (token, terms) {
                (Some(t), Some(terms)) => guard.counter_contract(t, id, terms),
                (None, _) => Err(Error::Unauthorized("missing bearer token".into())),
                (_, None) => Err(Error::Other("terms required".into())),
            }
        }
        (m, p) if m == "POST" && p.starts_with("/v1/contract/") && p.ends_with("/reject") => {
            let id = p
                .trim_start_matches("/v1/contract/")
                .trim_end_matches("/reject");
            let reason = body
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unspecified");
            match token {
                Some(t) => guard.reject_contract(t, id, reason),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        (m, p) if m == "POST" && p.starts_with("/v1/contract/") && p.ends_with("/cancel") => {
            let id = p
                .trim_start_matches("/v1/contract/")
                .trim_end_matches("/cancel");
            let reason = body
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unspecified");
            match token {
                Some(t) => guard.cancel_contract(t, id, reason),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        (m, p) if m == "POST" && p.starts_with("/v1/contract/") && p.ends_with("/start") => {
            let id = p
                .trim_start_matches("/v1/contract/")
                .trim_end_matches("/start");
            let plan = body.get("plan").cloned().unwrap_or(json!({}));
            match token {
                Some(t) => guard.start_execution(t, id, &plan),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        (m, p) if m == "POST" && p.starts_with("/v1/contract/") && p.ends_with("/progress") => {
            let id = p
                .trim_start_matches("/v1/contract/")
                .trim_end_matches("/progress");
            let step = body.get("step").and_then(|v| v.as_u64()).unwrap_or(0);
            let note = body.get("note").and_then(|v| v.as_str()).unwrap_or("");
            match token {
                Some(t) => guard.report_progress(t, id, step, note),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        ("POST", "/v1/deregister") => match token {
            Some(t) => guard.deregister(t),
            None => Err(Error::Unauthorized("missing bearer token".into())),
        },
        (m, p) if m == "POST" && p.starts_with("/v1/contract/") && p.ends_with("/remedy") => {
            let id = p
                .trim_start_matches("/v1/contract/")
                .trim_end_matches("/remedy");
            let hash = body
                .get("deliverable_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match token {
                Some(t) => guard.remedy(t, id, hash),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        (m, p) if m == "POST" && p.starts_with("/v1/contract/") && p.ends_with("/verify") => {
            let id = p
                .trim_start_matches("/v1/contract/")
                .trim_end_matches("/verify");
            let content = body.get("content").and_then(|v| v.as_str());
            match token {
                Some(t) => guard.verify_delivery(t, id, content),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        ("GET", "/v1/escalations") => match (&guard.admin_token, token) {
            (Some(admin), Some(t)) if admin == t => Ok(guard.escalations()),
            (None, _) => Err(Error::Unauthorized("admin token not configured".into())),
            _ => Err(Error::Unauthorized("admin token required".into())),
        },
        ("GET", p) if p.starts_with("/v1/reputation/") => {
            let did = percent_decode(p.trim_start_matches("/v1/reputation/"));
            guard.reputation_of(&did)
        }

        // ---- event delivery (RFC-0013) ----
        ("POST", "/v1/subscriptions") => match token {
            Some(t) => guard.subscribe(t, &body),
            None => Err(Error::Unauthorized("missing bearer token".into())),
        },
        ("GET", "/v1/subscriptions") => match token {
            Some(t) => guard.list_subscriptions(t),
            None => Err(Error::Unauthorized("missing bearer token".into())),
        },
        (m, p) if m == "DELETE" && p.starts_with("/v1/subscriptions/") => {
            let id = p.trim_start_matches("/v1/subscriptions/");
            match token {
                Some(t) => guard.unsubscribe(t, id),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        // Cursor form of the event stream. The streaming SSE responder
        // lives in main.rs (it must own the connection); this JSON form
        // is the same data for clients that prefer to poll the cursor.
        ("GET", p) if p.starts_with("/v1/events") => {
            let params = parse_url_params(raw_path);
            let after = params
                .get("after")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let limit = params
                .get("limit")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(100)
                .min(1000);
            match token {
                Some(t) => guard
                    .events_for(t, after, limit)
                    .map(|events| json!({ "events": events })),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }

        // ---- portability ----
        ("POST", "/v1/identity/export") => match token {
            Some(t) => guard.identity_export(t),
            None => Err(Error::Unauthorized("missing bearer token".into())),
        },

        _ => Err(Error::Other("unknown route".into())),
    };

    match response {
        Ok(v) => (200, v),
        Err(e) => {
            let code = match e {
                Error::BadSignature => "bad_signature",
                Error::Unauthorized(_) => "unauthorized",
                Error::AutonomyViolation(_) => "budget_exceeded",
                Error::UnknownContract(_) => "contract_not_found",
                Error::InvalidTransition { .. } => "invalid_transition",
                Error::EscrowViolation(_) => "escrow_violation",
                _ => "invalid_request",
            };
            // Status classes, not a blanket 400: clients (and the
            // OpenAPI contract) distinguish "you are not allowed" from
            // "your request was malformed" from "it does not exist".
            let status = match e {
                Error::Unauthorized(_) => 401,
                Error::UnknownContract(_) => 404,
                _ => 400,
            };
            (
                status,
                json!({ "error": { "code": code, "message": e.to_string() } }),
            )
        }
    }
}

/// Parse a discovery query from JSON body or URL query string.
fn parse_query(body: &Value, path: &str) -> Query {
    let mut q = Query::default();
    if let Some(name) = body.get("name").and_then(|v| v.as_str()) {
        q.name = Some(name.into());
    }
    if let Some(mp) = body.get("max_price").and_then(|v| v.as_f64()) {
        q.max_price = Some(mp);
    }
    if let Some(mr) = body.get("min_reputation").and_then(|v| v.as_f64()) {
        q.min_reputation = Some(mr);
    }
    if let Some(a) = body.get("required_autonomy").and_then(|v| v.as_str()) {
        q.required_autonomy = Some(a.into());
    }
    if let Some(langs) = body
        .get("languages")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        q.languages = langs;
    }
    if let Some(regions) = body
        .get("regions")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        q.regions = regions;
    }
    if let Some(max) = body.get("max_results").and_then(|v| v.as_u64()) {
        q.max_results = (max as usize).min(1000);
    }
    // Also parse from the query string (curl-style).
    for (k, v) in parse_url_params(path) {
        match k.as_str() {
            "name" => q.name = Some(v),
            "max_price" => q.max_price = v.parse().ok(),
            "min_reputation" => q.min_reputation = v.parse().ok(),
            "required_autonomy" => q.required_autonomy = Some(v),
            "languages" => q.languages = split_csv(&v),
            "regions" => q.regions = split_csv(&v),
            "max_results" => {
                if let Ok(max) = v.parse::<usize>() {
                    q.max_results = max.min(1000);
                }
            }
            _ => {}
        }
    }
    q
}

fn parse_url_params(path: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(qs) = path.split('?').nth(1) {
        for pair in qs.split('&').filter(|p| !p.is_empty()) {
            let mut it = pair.splitn(2, '=');
            let k = percent_decode(it.next().unwrap_or(""));
            let v = percent_decode(it.next().unwrap_or(""));
            out.insert(k, v);
        }
    }
    out
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = &value[i + 1..i + 3];
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn decode_seed_hex(seed_hex: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(seed_hex).map_err(|_| Error::Other("invalid identity seed".into()))?;
    bytes
        .try_into()
        .map_err(|_| Error::Other("invalid identity seed length".into()))
}

/// Build a canned Terms from a JSON value (helper for the demo node).
pub fn terms_from_json(v: &Value) -> Option<Terms> {
    serde_json::from_value(v.clone()).ok()
}

// Re-export for the binary.
pub use crate::discovery::Price as DiscoveryPrice;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::Price;
    use crate::relayer::MockChain;
    use crate::storage::sqlite::SqliteStorage;

    fn state() -> Arc<Mutex<NodeState>> {
        Arc::new(Mutex::new(NodeState::new(Box::new(
            SqliteStorage::open(":memory:").unwrap(),
        ))))
    }

    #[test]
    fn propose_lookup_is_constant_time_with_many_agents() {
        // Regression: propose used to scan ALL registered agents
        // (O(n) with an allocation per entry). The DID index must keep
        // propose fast regardless of the agent count, and correct
        // (unknown providers still rejected).
        let arc = Arc::new(Mutex::new(NodeState::with_rate_limits(
            Box::new(SqliteStorage::open(":memory:").unwrap()),
            None,
            100_000_000,
            100_000_000,
        )));
        let (_, client_tok) = arc.lock().unwrap().create_identity();
        let (provider_did, _provider_tok) = arc.lock().unwrap().create_identity();
        // Flood the node with 5_000 extra identities (as the public
        // identity endpoint allows).
        for _ in 0..5_000 {
            let _ = arc.lock().unwrap().create_identity();
        }
        assert_eq!(arc.lock().unwrap().agents.len(), 5_002);
        // Propose must still succeed and stay fast.
        let now = crate::message::now_unix();
        let body = json!({
            "provider": provider_did, "capability_id": "cap:x",
            "terms": {
                "input": {}, "deliverable": {}, "acceptance_criteria": ["ok"],
                "deadline": now + 3600,
                "price": { "amount": 0.05, "currency": "EUR", "model": "fixed", "cap": 100.0 },
                "autonomy": "propose", "confidentiality": null
            },
            "escrow": false
        });
        let bytes = body.to_string().into_bytes();
        let auth = format!("Bearer {client_tok}");
        let t0 = std::time::Instant::now();
        for _ in 0..200 {
            let (s, _) = route(&arc, "POST", "/v1/contract/propose", &bytes, Some(&auth));
            assert_eq!(s, 200, "propose must succeed with many agents");
        }
        let per_req = t0.elapsed().as_secs_f64() / 200.0;
        assert!(
            per_req < 5e-3,
            "propose degraded with many agents: {:.1} ms/req (O(n) scan regression?)",
            per_req * 1e3
        );
        // Unknown provider still rejected.
        let bad = json!({ "provider": "did:gap:unknown", "capability_id": "cap:x", "terms": body["terms"], "escrow": false });
        let (s, _) = route(
            &arc,
            "POST",
            "/v1/contract/propose",
            &bad.to_string().into_bytes(),
            Some(&auth),
        );
        assert_eq!(s, 400);
    }

    fn register(arc: &Arc<Mutex<NodeState>>) -> String {
        let (_, token) = arc.lock().unwrap().create_identity();
        token
    }

    fn announce_lead_gen(arc: &Arc<Mutex<NodeState>>, token: &str) {
        let caps = vec![Capability {
            id: "cap:a:lead-gen".into(),
            name: "lead-generation".into(),
            description: "qualify leads".into(),
            input: json!({}),
            output: json!({}),
            price: Some(DiscoveryPrice {
                amount: 0.05,
                currency: "EUR".into(),
                model: "per_unit".into(),
            }),
            autonomy: vec!["propose".into(), "execute-notify".into()],
        }];
        let body = json!({ "capabilities": caps, "ttl_seconds": 3600 });
        let (status, _) = route(
            arc,
            "POST",
            "/v1/announce",
            &body.to_string().into_bytes(),
            Some(&format!("Bearer {token}")),
        );
        assert_eq!(status, 200);
    }

    #[test]
    fn health_and_card() {
        let arc = state();
        let (s, v) = route(&arc, "GET", "/health", &[], None);
        assert_eq!(s, 200);
        assert_eq!(v["status"], "ok");
        let (s2, v2) = route(&arc, "GET", "/.well-known/gap-agent.json", &[], None);
        assert_eq!(s2, 200);
        assert_eq!(v2["agent"]["name"], "GAP Node");
    }

    #[test]
    fn identity_announce_discover_flow() {
        let arc = state();
        let token = register(&arc);
        announce_lead_gen(&arc, &token);

        // Discover with a name filter.
        let (s, v) = route(&arc, "GET", "/v1/discover?name=lead-generation", &[], None);
        assert_eq!(s, 200);
        assert_eq!(v["results"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn discover_query_string_filters_results() {
        let arc = state();
        let token_a = register(&arc);
        let token_b = register(&arc);

        let cap_a = Capability {
            id: "cap:a:lead".into(),
            name: "lead-generation".into(),
            description: "leads".into(),
            input: json!({}),
            output: json!({}),
            price: Some(DiscoveryPrice {
                amount: 0.05,
                currency: "EUR".into(),
                model: "fixed".into(),
            }),
            autonomy: vec!["propose".into()],
        };
        let cap_b = Capability {
            id: "cap:b:analysis".into(),
            name: "analysis".into(),
            description: "analysis".into(),
            input: json!({}),
            output: json!({}),
            price: Some(DiscoveryPrice {
                amount: 1.0,
                currency: "EUR".into(),
                model: "fixed".into(),
            }),
            autonomy: vec!["propose".into()],
        };
        let body_a = json!({ "capabilities": [cap_a], "languages": ["fr"], "regions": ["EU"] });
        let body_b = json!({ "capabilities": [cap_b], "languages": ["en"], "regions": ["US"] });
        assert_eq!(
            route(
                &arc,
                "POST",
                "/v1/announce",
                &body_a.to_string().into_bytes(),
                Some(&format!("Bearer {token_a}"))
            )
            .0,
            200
        );
        assert_eq!(
            route(
                &arc,
                "POST",
                "/v1/announce",
                &body_b.to_string().into_bytes(),
                Some(&format!("Bearer {token_b}"))
            )
            .0,
            200
        );

        let (s, v) = route(
            &arc,
            "GET",
            "/v1/discover?name=analysis&languages=en&regions=US&max_results=1",
            &[],
            None,
        );
        assert_eq!(s, 200);
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["capabilities"][0]["name"], "analysis");
    }

    #[test]
    fn announce_requires_auth_and_caps() {
        let arc = state();
        let (s, _) = route(&arc, "POST", "/v1/announce", b"{}".as_ref(), None);
        assert_eq!(
            s, 401,
            "missing token is an auth failure, not a bad request"
        );
        let token = register(&arc);
        let (s2, v2) = route(
            &arc,
            "POST",
            "/v1/announce",
            b"{}".as_ref(),
            Some(&format!("Bearer {token}")),
        );
        assert_eq!(s2, 400);
        assert_eq!(v2["error"]["code"], "invalid_request");
    }

    #[test]
    fn full_contract_flow_over_http() {
        let arc = state();
        let client_tok = register(&arc);
        let provider_tok = register(&arc);
        let provider_did = arc.lock().unwrap().agents[&provider_tok]
            .identity
            .did()
            .to_string();

        // Provider announces an analysis capability.
        let caps = vec![Capability {
            id: "cap:p:analyze".into(),
            name: "analyze".into(),
            description: "summarize".into(),
            input: json!({}),
            output: json!({}),
            price: Some(DiscoveryPrice {
                amount: 1.0,
                currency: "EUR".into(),
                model: "fixed".into(),
            }),
            autonomy: vec!["propose".into()],
        }];
        let body = json!({ "capabilities": caps });
        let (s, _) = route(
            &arc,
            "POST",
            "/v1/announce",
            &body.to_string().into_bytes(),
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_eq!(s, 200);

        // Client proposes.
        let terms = Terms {
            input: json!({}),
            deliverable: json!({}),
            acceptance_criteria: vec!["ok".into()],
            deadline: now_unix() + 3600,
            price: Price {
                amount: 1.0,
                currency: "EUR".into(),
                model: "fixed".into(),
                cap: Some(5.0),
            },
            autonomy: "propose".into(),
            confidentiality: None,
            human_review_above: None,
        };
        let body = json!({ "provider": provider_did, "capability_id": "cap:p:analyze", "terms": terms, "escrow": true });
        let (s, v) = route(
            &arc,
            "POST",
            "/v1/contract/propose",
            &body.to_string().into_bytes(),
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(s, 200, "propose failed: {v}");
        let contract_id = v["contract_id"].as_str().unwrap().to_string();

        // Provider accepts (contract becomes signed).
        let (s, _) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{contract_id}/accept"),
            &[],
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_eq!(s, 200, "accept failed");

        // Park escrow (client) — after both signatures.
        let body = json!({ "contract_id": contract_id, "amount": 1.0 });
        let (s, _) = route(
            &arc,
            "POST",
            "/v1/escrow/park",
            &body.to_string().into_bytes(),
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(s, 200, "park failed");

        // Provider delivers.
        let body = json!({ "deliverable_hash": "sha256:abc" });
        let (s, _) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{contract_id}/deliver"),
            &body.to_string().into_bytes(),
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_eq!(s, 200, "deliver failed");

        // Client accepts delivery -> escrow releases.
        let (s, v) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{contract_id}/accept-delivery"),
            &[],
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(s, 200, "accept-delivery failed: {v}");
        assert_eq!(v["state"], "accepted");

        // Audit spine has events (authenticated endpoint).
        let (s, v) = route(
            &arc,
            "GET",
            "/v1/audit",
            &[],
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(s, 200);
        assert!(!v["events"].as_array().unwrap().is_empty());

        // Settlement credited the provider's reputation and fed the
        // registry: a reputation-filtered discover now finds it.
        let (s, v) = route(&arc, "GET", "/v1/discover?min_reputation=0.6", &[], None);
        assert_eq!(s, 200);
        assert_eq!(
            v["results"].as_array().unwrap().len(),
            1,
            "settled provider should pass the reputation filter: {v}"
        );
        // An impossible bar still filters it out.
        let (s, v) = route(&arc, "GET", "/v1/discover?min_reputation=0.99", &[], None);
        assert_eq!(s, 200);
        assert!(v["results"].as_array().unwrap().is_empty());
    }

    #[test]
    fn vault_seals_custodied_seeds_and_restores() {
        let path = format!("/tmp/gap-node-vault-{}.db", crate::new_id("db"));
        let vault = || Some(crate::vault::Vault::new(&[42u8; 32]));
        let (did, token) = {
            let mut state = NodeState::with_vault(
                Box::new(SqliteStorage::open(&path).unwrap()),
                None,
                100_000,
                100_000,
                vault(),
            );
            state.create_identity()
        };

        // The stored row is sealed, not plaintext.
        let store = SqliteStorage::open(&path).unwrap();
        let recs = store.list_identities().unwrap();
        assert_eq!(recs.len(), 1);
        assert!(crate::vault::Vault::is_sealed(&recs[0].seed_hex));

        // Restart with the right master key: the agent is usable again.
        let state = NodeState::with_vault(
            Box::new(SqliteStorage::open(&path).unwrap()),
            None,
            100_000,
            100_000,
            vault(),
        );
        assert_eq!(
            state
                .agent_by_token(&token)
                .unwrap()
                .identity
                .did()
                .to_string(),
            did
        );

        // Restart without the key: the sealed identity is skipped (fails
        // closed), never silently treated as plaintext.
        let state = NodeState::with_vault(
            Box::new(SqliteStorage::open(&path).unwrap()),
            None,
            100_000,
            100_000,
            None,
        );
        assert!(state.agent_by_token(&token).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn node_restores_identity_contract_and_escrow_from_sqlite() {
        let path = format!("/tmp/gap-node-restore-{}.db", crate::new_id("db"));
        let terms = Terms {
            input: json!({}),
            deliverable: json!({}),
            acceptance_criteria: vec!["ok".into()],
            deadline: now_unix() + 3600,
            price: Price {
                amount: 1.0,
                currency: "EUR".into(),
                model: "fixed".into(),
                cap: Some(5.0),
            },
            autonomy: "propose".into(),
            confidentiality: None,
            human_review_above: None,
        };
        let (client_tok, provider_tok, contract_id) = {
            let arc = Arc::new(Mutex::new(NodeState::with_rate_limits(
                Box::new(SqliteStorage::open(&path).unwrap()),
                None,
                100_000,
                100_000,
            )));
            let client_tok = register(&arc);
            let provider_tok = register(&arc);
            let provider_did = arc.lock().unwrap().agents[&provider_tok]
                .identity
                .did()
                .to_string();
            let body = json!({ "provider": provider_did, "capability_id": "cap:p", "terms": terms, "escrow": true });
            let (s, v) = route(
                &arc,
                "POST",
                "/v1/contract/propose",
                &body.to_string().into_bytes(),
                Some(&format!("Bearer {client_tok}")),
            );
            assert_eq!(s, 200, "propose failed: {v}");
            let contract_id = v["contract_id"].as_str().unwrap().to_string();
            assert_eq!(
                route(
                    &arc,
                    "POST",
                    &format!("/v1/contract/{contract_id}/accept"),
                    &[],
                    Some(&format!("Bearer {provider_tok}"))
                )
                .0,
                200
            );
            let body = json!({ "contract_id": contract_id, "amount": "1.00" });
            assert_eq!(
                route(
                    &arc,
                    "POST",
                    "/v1/escrow/park",
                    &body.to_string().into_bytes(),
                    Some(&format!("Bearer {client_tok}"))
                )
                .0,
                200
            );
            (client_tok, provider_tok, contract_id)
        };

        let arc = Arc::new(Mutex::new(NodeState::with_rate_limits(
            Box::new(SqliteStorage::open(&path).unwrap()),
            None,
            100_000,
            100_000,
        )));
        let body = json!({ "deliverable_hash": "sha256:restored" });
        assert_eq!(
            route(
                &arc,
                "POST",
                &format!("/v1/contract/{contract_id}/deliver"),
                &body.to_string().into_bytes(),
                Some(&format!("Bearer {provider_tok}"))
            )
            .0,
            200
        );
        let (s, v) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{contract_id}/accept-delivery"),
            &[],
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(s, 200, "accept after restore failed: {v}");
        assert_eq!(v["settlement"]["amount"], "1.000000");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path}-wal"));
        let _ = std::fs::remove_file(format!("{path}-shm"));
    }

    #[test]
    fn workflow_routes_create_and_read_status() {
        let arc = state();
        let token = register(&arc);
        let body = json!({
            "name": "pipeline",
            "inputs": { "topic": "gap" },
            "steps": [
                { "step_id": "scrape", "capability": "cap:scrape", "inputs": { "query": "${workflow.topic}" }, "outputs": { "raw": "steps.scrape.deliverable" } },
                { "step_id": "analyze", "capability": "cap:analyze", "needs": ["scrape"], "inputs": { "data": "${steps.scrape.raw}" } }
            ],
            "budget": { "max_total": 5.0, "currency": "EUR" },
            "on_failure": "abort"
        });
        let (s, v) = route(
            &arc,
            "POST",
            "/v1/workflows",
            &body.to_string().into_bytes(),
            Some(&format!("Bearer {token}")),
        );
        assert_eq!(s, 200, "workflow create failed: {v}");
        let workflow_id = v["workflow_id"].as_str().unwrap();
        let (s, v) = route(
            &arc,
            "GET",
            &format!("/v1/workflows/{workflow_id}"),
            &[],
            Some(&format!("Bearer {token}")),
        );
        assert_eq!(s, 200, "workflow status failed: {v}");
        assert_eq!(v["steps"].as_array().unwrap().len(), 2);
        assert_eq!(v["steps"][0]["state"], "pending");
    }

    #[test]
    fn unauthorized_operations_rejected() {
        let arc = state();
        let (s, v) = route(&arc, "POST", "/v1/contract/propose", b"{}".as_ref(), None);
        assert_eq!(s, 401);
        assert_eq!(v["error"]["code"], "unauthorized");

        let (s2, _) = route(&arc, "GET", "/v1/contract/urn:gap:ctr:nope", &[], None);
        assert_eq!(s2, 404, "unknown contract is not-found, not bad-request");
    }

    #[test]
    fn rate_limit_returns_429_after_cap() {
        let arc = state();
        let token = register(&arc);
        // Health is not rate-limited per-token when no token is passed,
        // but with a token we hit the 120/min cap.
        let auth = Some(format!("Bearer {token}"));
        let mut saw_429 = false;
        for _ in 0..130 {
            let (s, _) = route_with_ip(
                &arc,
                "GET",
                "/v1/audit",
                &[],
                auth.as_deref(),
                Some("10.0.0.1"),
            );
            if s == 429 {
                saw_429 = true;
                break;
            }
        }
        assert!(
            saw_429,
            "expected a 429 after exceeding the per-token rate cap"
        );
    }

    #[test]
    fn unknown_route_returns_error() {
        let arc = state();
        let (s, v) = route(&arc, "DELETE", "/v1/whatever", &[], None);
        assert_eq!(s, 400);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown route"));
    }

    #[test]
    fn onchain_escrow_flow_via_relayer() {
        // Node configured with a mock chain -> escrow goes on-chain.
        let arc = Arc::new(Mutex::new(NodeState::new(Box::new(
            SqliteStorage::open(":memory:").unwrap(),
        ))));
        {
            let mut g = arc.lock().unwrap();
            let chain = MockChain::new();
            g.set_relayer(Box::new(chain), "0xGapEscrow");
        }

        let client_tok = register(&arc);
        let provider_tok = register(&arc);
        let provider_did = arc.lock().unwrap().agents[&provider_tok]
            .identity
            .did()
            .to_string();

        // Provider announces.
        let caps = vec![Capability {
            id: "cap:p:analyze".into(),
            name: "analyze".into(),
            description: "summarize".into(),
            input: json!({}),
            output: json!({}),
            price: Some(DiscoveryPrice {
                amount: 1.0,
                currency: "EUR".into(),
                model: "fixed".into(),
            }),
            autonomy: vec!["propose".into()],
        }];
        let body = json!({ "capabilities": caps });
        let (s, _) = route(
            &arc,
            "POST",
            "/v1/announce",
            &body.to_string().into_bytes(),
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_eq!(s, 200);

        // Propose + accept + park (on-chain) + deliver + accept-delivery (on-chain release).
        let terms = Terms {
            input: json!({}),
            deliverable: json!({}),
            acceptance_criteria: vec!["ok".into()],
            deadline: now_unix() + 3600,
            price: Price {
                amount: 1.0,
                currency: "EUR".into(),
                model: "fixed".into(),
                cap: Some(5.0),
            },
            autonomy: "propose".into(),
            confidentiality: None,
            human_review_above: None,
        };
        let body = json!({ "provider": provider_did, "capability_id": "cap:p:analyze", "terms": terms, "escrow": true });
        let (s, v) = route(
            &arc,
            "POST",
            "/v1/contract/propose",
            &body.to_string().into_bytes(),
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(s, 200, "propose failed: {v}");
        let contract_id = v["contract_id"].as_str().unwrap().to_string();

        let (s, _) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{contract_id}/accept"),
            &[],
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_eq!(s, 200, "accept failed");

        let body = json!({ "contract_id": contract_id, "amount": 1.0 });
        let (s, _) = route(
            &arc,
            "POST",
            "/v1/escrow/park",
            &body.to_string().into_bytes(),
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(s, 200, "on-chain park failed");

        let body = json!({ "deliverable_hash": "sha256:abc" });
        let (s, _) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{contract_id}/deliver"),
            &body.to_string().into_bytes(),
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_eq!(s, 200, "deliver failed");

        let (s, v) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{contract_id}/accept-delivery"),
            &[],
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(s, 200, "accept-delivery failed: {v}");
        assert_eq!(v["state"], "accepted");
        assert_eq!(v["settlement"]["chain"], "onchain");

        // Audit records the on-chain events (authenticated endpoint).
        let (s, v) = route(
            &arc,
            "GET",
            "/v1/audit",
            &[],
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(s, 200);
        let events = v["events"].as_array().unwrap();
        assert!(events.iter().any(|e| e["kind"] == "pay.parked.onchain"));
        assert!(events.iter().any(|e| e["kind"] == "pay.released.onchain"));
    }
}
