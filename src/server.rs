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
use crate::storage::Storage;
use crate::sybil::RateCounters;
use serde_json::{Value, json};
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
        let identity = match seed {
            Some(seed_bytes) => AgentIdentity::from_seed(&seed_bytes),
            None => AgentIdentity::generate(),
        };
        Self {
            node: NodeIdentity { identity },
            agents: HashMap::new(),
            agents_by_did: HashMap::new(),
            registry: Registry::new(),
            contracts: HashMap::new(),
            escrows: HashMap::new(),
            relayer: None,
            storage,
            rate_limits: std::collections::HashMap::new(),
            ip_limits: std::collections::HashMap::new(),
            token_cap,
            ip_cap,
        }
    }

    /// Enforce per-token and per-IP rate limits before processing a
    /// request (audit fix H-03). Returns an error when over limit.
    pub fn check_rate_limit(&mut self, token: Option<&str>, ip: Option<&str>) -> Result<()> {
        let now = crate::message::now_unix();
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
        self.agents.insert(
            token.clone(),
            RegisteredAgent {
                identity,
                announcement: None,
            },
        );
        self.agents_by_did.insert(did.clone(), token.clone());
        (did, token)
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
        let agent = self
            .agents
            .get_mut(token)
            .ok_or_else(|| Error::Unauthorized("invalid bearer token".into()))?;
        let ann = Announcement::signed(
            &agent.identity,
            capabilities,
            vec![Reachability {
                transport: "https".into(),
                endpoint: format!("https://agent/{}/gap", agent.identity.did()),
            }],
            ttl_seconds,
        );
        ann.verify()?;
        self.registry.announce(ann.clone())?;
        let id = format!("urn:gap:ann:{}", &ann.agent_did.to_string()[..16]);
        agent.announcement = Some(ann);
        self.record("cap.announced", json!({ "agent": token }));
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
        let provider = crate::identity::Did::parse(provider_did)?;
        // O(1) provider lookup via the DID index (previously a scan of
        // every registered agent — quadratic once the node serves
        // many identities).
        if !self.agents_by_did.contains_key(provider_did) {
            return Err(Error::UnknownContract(format!(
                "provider {provider_did} not registered on this node"
            )));
        }
        let contract = Contract::propose(
            &client.identity,
            provider,
            capability_id,
            terms,
            use_escrow,
        );
        let id = contract.contract_id.clone();
        self.contracts.insert(id.clone(), contract);
        self.record("ctr.proposed", json!({ "contract_id": id }));
        Ok(id)
    }

    /// The provider accepts a proposed contract.
    pub fn accept_contract(&mut self, provider_token: &str, contract_id: &str) -> Result<()> {
        let provider = self.agent_by_token(provider_token)?;
        let contract = self
            .contracts
            .get(contract_id)
            .cloned()
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        let signed = contract.accept_by_provider(&provider.identity)?;
        self.contracts.insert(contract_id.into(), signed);
        self.record("ctr.signed", json!({ "contract_id": contract_id }));
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
        let contract = self
            .contracts
            .get_mut(contract_id)
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.provider != provider_did {
            return Err(Error::Unauthorized("only the provider may deliver".into()));
        }
        contract.transition(ContractState::Executing)?;
        contract.transition(ContractState::Delivered)?;
        let _ = deliverable_hash;
        self.record("exe.delivered", json!({ "contract_id": contract_id }));
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
            self.record("pay.released.onchain", json!({ "contract_id": contract_id }));
            let contract = self
                .contracts
                .get_mut(contract_id)
                .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
            contract.transition(ContractState::Accepted)?;
            self.record("exe.accepted", json!({ "contract_id": contract_id }));
            return Ok(json!({
                "state": "accepted",
                "settlement": { "amount": 0.0, "currency": "USDC", "chain": "onchain" }
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

        self.record("exe.accepted", json!({ "contract_id": contract_id }));
        Ok(json!({
            "state": "accepted",
            "settlement": { "amount": amount, "currency": currency }
        }))
    }

    /// Client parks funds into escrow for a contract.
    pub fn escrow_park(&mut self, client_token: &str, contract_id: &str, amount: &crate::amount::Amount) -> Result<()> {
        let client = self.agent_by_token(client_token)?;
        let contract = self
            .contracts
            .get(contract_id)
            .cloned()
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.client != *client.identity.did() {
            return Err(Error::Unauthorized("only the client may park".into()));
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
            self.record("pay.parked.onchain", json!({ "contract_id": contract_id, "amount": amount }));
            return Ok(());
        }

        // Off-chain path (reference escrow).
        let amount_f64 = amount.minor_units() as f64 / crate::amount::SCALE as f64;
        let instruction = Envelope::new(
            client.identity.did().clone(),
            self.node_did(),
            Kind::PayPark,
            json!({ "amount": amount_f64 }),
        )
        .for_contract(contract_id)
        .sign(&client.identity);
        let mut escrow = Escrow::new(AgentIdentity::generate());
        escrow.register(contract)?;
        escrow.park(&instruction)?;
        self.escrows.insert(contract_id.into(), escrow);
        self.record(
            "pay.parked",
            json!({ "contract_id": contract_id, "amount": amount.to_string_decimal() }),
        );
        Ok(())
    }

    /// Record an event on the audit spine.
    pub fn record(&mut self, kind: &str, payload: Value) {
        let _ = self.storage.append_event(kind, payload);
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

/// Route with client IP for rate limiting (audit H-03).
pub fn route_with_ip(
    state: &Arc<Mutex<NodeState>>,
    method: &str,
    path: &str,
    body: &[u8],
    auth: Option<&str>,
    client_ip: Option<&str>,
) -> (u16, Value) {
    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(_) => return (500, json!({ "error": { "code": "internal", "message": "state lock poisoned" } })),
    };

    // Rate limiting first (audit H-03): 429 when over limit.
    let token = auth.and_then(|h| h.strip_prefix("Bearer "));
    if guard.check_rate_limit(token, client_ip).is_err() {
        return (
            429,
            json!({ "error": { "code": "rate_limited", "message": "too many requests" } }),
        );
    }

    let path = path.split('?').next().unwrap_or(path);
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

    let response = match (method, path) {
        // ---- health & card ----
        ("GET", "/health") => Ok(json!({ "status": "ok", "node": guard.node_did() })),
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
            let ttl = body.get("ttl_seconds").and_then(|v| v.as_u64()).unwrap_or(86400);
            match (token, caps.is_empty()) {
                (Some(t), false) => guard
                    .announce(t, caps, languages, regions, ttl)
                    .map(|id| json!({ "announcement_id": id }))
                    ,
                (None, _) => Err(Error::Unauthorized("missing bearer token".into())),
                (_, true) => Err(Error::Other("capabilities required".into())),
            }
        }
        ("GET", "/v1/discover") => {
            let q = parse_query(&body, path);
            let results = guard.discover(&q);
            Ok(json!({ "results": results }))
        }

        // ---- contracts ----
        ("POST", "/v1/contract/propose") => {
            let provider = body.get("provider").and_then(|v| v.as_str()).unwrap_or("");
            let capability_id = body.get("capability_id").and_then(|v| v.as_str()).unwrap_or("");
            let terms_parsed: Option<Terms> = body
                .get("terms")
                .and_then(|t| serde_json::from_value(t.clone()).ok());
            let escrow = body.get("escrow").and_then(|v| v.as_bool()).unwrap_or(true);
            match (token, terms_parsed) {
                (Some(t), Some(terms)) => guard
                    .propose_contract(t, provider, capability_id, terms, escrow)
                    .map(|id| json!({ "contract_id": id, "state": "draft" }))
                    ,
                (None, _) => Err(Error::Unauthorized("missing bearer token".into())),
                (_, None) => Err(Error::Other("terms required".into())),
            }
        }
        (m, p) if m == "POST" && p.starts_with("/v1/contract/") && p.ends_with("/accept") => {
            let id = p.trim_start_matches("/v1/contract/").trim_end_matches("/accept");
            match token {
                Some(t) => guard.accept_contract(t, id).map(|_| json!({ "state": "signed" })),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        (m, p) if m == "POST" && p.starts_with("/v1/contract/") && p.ends_with("/deliver") => {
            let id = p.trim_start_matches("/v1/contract/").trim_end_matches("/deliver");
            let hash = body.get("deliverable_hash").and_then(|v| v.as_str()).unwrap_or("");
            match token {
                Some(t) => guard.deliver(t, id, hash).map(|_| json!({ "state": "delivered" })),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        (m, p) if m == "POST" && p.starts_with("/v1/contract/") && p.ends_with("/accept-delivery") => {
            let id = p.trim_start_matches("/v1/contract/").trim_end_matches("/accept-delivery");
            match token {
                Some(t) => guard.accept_delivery(t, id),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        ("GET", p) if p.starts_with("/v1/contract/") => {
            let id = p.trim_start_matches("/v1/contract/");
            match guard.contracts.get(id) {
                Some(c) => Ok(json!({ "contract_id": c.contract_id, "state": format!("{:?}", c.state) })),
                None => Err(Error::UnknownContract(id.into())),
            }
        }

        // ---- escrow ----
        ("POST", "/v1/escrow/park") => {
            let id = body.get("contract_id").and_then(|v| v.as_str()).unwrap_or("");
            let amount = match body.get("amount") {
                Some(v) if v.is_string() => {
                    crate::amount::Amount::parse(v.as_str().unwrap_or(""))
                }
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
            let events = guard
                .storage
                .events_after(0, 100)
                .unwrap_or_default();
            Ok(json!({ "events": events }))
        }

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
            (400, json!({ "error": { "code": code, "message": e.to_string() } }))
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
    // Also parse from the query string (curl-style).
    if let Some(qs) = path.split('?').nth(1) {
        for pair in qs.split('&') {
            let mut it = pair.split('=');
            let (k, v) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
            match k {
                "name" => q.name = Some(v.into()),
                "max_price" => q.max_price = v.parse().ok(),
                "min_reputation" => q.min_reputation = v.parse().ok(),
                "required_autonomy" => q.required_autonomy = Some(v.into()),
                _ => {}
            }
        }
    }
    q
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
        let (s, _) = route(&arc, "POST", "/v1/contract/propose", &bad.to_string().into_bytes(), Some(&auth));
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
        let (status, _) = route(arc, "POST", "/v1/announce", &body.to_string().into_bytes(), Some(&format!("Bearer {token}")));
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
    fn announce_requires_auth_and_caps() {
        let arc = state();
        let (s, _) = route(&arc, "POST", "/v1/announce", b"{}".as_ref(), None);
        assert_eq!(s, 400);
        let token = register(&arc);
        let (s2, v2) = route(&arc, "POST", "/v1/announce", b"{}".as_ref(), Some(&format!("Bearer {token}")));
        assert_eq!(s2, 400);
        assert_eq!(v2["error"]["code"], "invalid_request");
    }

    #[test]
    fn full_contract_flow_over_http() {
        let arc = state();
        let client_tok = register(&arc);
        let provider_tok = register(&arc);
        let provider_did = arc.lock().unwrap().agents[&provider_tok].identity.did().to_string();

        // Provider announces an analysis capability.
        let caps = vec![Capability {
            id: "cap:p:analyze".into(),
            name: "analyze".into(),
            description: "summarize".into(),
            input: json!({}),
            output: json!({}),
            price: Some(DiscoveryPrice { amount: 1.0, currency: "EUR".into(), model: "fixed".into() }),
            autonomy: vec!["propose".into()],
        }];
        let body = json!({ "capabilities": caps });
        let (s, _) = route(&arc, "POST", "/v1/announce", &body.to_string().into_bytes(), Some(&format!("Bearer {provider_tok}")));
        assert_eq!(s, 200);

        // Client proposes.
        let terms = Terms {
            input: json!({}),
            deliverable: json!({}),
            acceptance_criteria: vec!["ok".into()],
            deadline: now_unix() + 3600,
            price: Price { amount: 1.0, currency: "EUR".into(), model: "fixed".into(), cap: Some(5.0) },
            autonomy: "propose".into(),
            confidentiality: None,
        };
        let body = json!({ "provider": provider_did, "capability_id": "cap:p:analyze", "terms": terms, "escrow": true });
        let (s, v) = route(&arc, "POST", "/v1/contract/propose", &body.to_string().into_bytes(), Some(&format!("Bearer {client_tok}")));
        assert_eq!(s, 200, "propose failed: {v}");
        let contract_id = v["contract_id"].as_str().unwrap().to_string();

        // Provider accepts (contract becomes signed).
        let (s, _) = route(&arc, "POST", &format!("/v1/contract/{contract_id}/accept"), &[], Some(&format!("Bearer {provider_tok}")));
        assert_eq!(s, 200, "accept failed");

        // Park escrow (client) — after both signatures.
        let body = json!({ "contract_id": contract_id, "amount": 1.0 });
        let (s, _) = route(&arc, "POST", "/v1/escrow/park", &body.to_string().into_bytes(), Some(&format!("Bearer {client_tok}")));
        assert_eq!(s, 200, "park failed");

        // Provider delivers.
        let body = json!({ "deliverable_hash": "sha256:abc" });
        let (s, _) = route(&arc, "POST", &format!("/v1/contract/{contract_id}/deliver"), &body.to_string().into_bytes(), Some(&format!("Bearer {provider_tok}")));
        assert_eq!(s, 200, "deliver failed");

        // Client accepts delivery -> escrow releases.
        let (s, v) = route(&arc, "POST", &format!("/v1/contract/{contract_id}/accept-delivery"), &[], Some(&format!("Bearer {client_tok}")));
        assert_eq!(s, 200, "accept-delivery failed: {v}");
        assert_eq!(v["state"], "accepted");

        // Audit spine has events.
        let (s, v) = route(&arc, "GET", "/v1/audit", &[], None);
        assert_eq!(s, 200);
        assert!(!v["events"].as_array().unwrap().is_empty());
    }

    #[test]
    fn unauthorized_operations_rejected() {
        let arc = state();
        let (s, v) = route(&arc, "POST", "/v1/contract/propose", b"{}".as_ref(), None);
        assert_eq!(s, 400);
        assert_eq!(v["error"]["code"], "unauthorized");

        let (s2, _) = route(&arc, "GET", "/v1/contract/urn:gap:ctr:nope", &[], None);
        assert_eq!(s2, 400);
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
            let (s, _) = route_with_ip(&arc, "GET", "/v1/audit", &[], auth.as_deref(), Some("10.0.0.1"));
            if s == 429 {
                saw_429 = true;
                break;
            }
        }
        assert!(saw_429, "expected a 429 after exceeding the per-token rate cap");
    }

    #[test]
    fn unknown_route_returns_error() {
        let arc = state();
        let (s, v) = route(&arc, "DELETE", "/v1/whatever", &[], None);
        assert_eq!(s, 400);
        assert!(v["error"]["message"].as_str().unwrap().contains("unknown route"));
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
        let provider_did = arc.lock().unwrap().agents[&provider_tok].identity.did().to_string();

        // Provider announces.
        let caps = vec![Capability {
            id: "cap:p:analyze".into(),
            name: "analyze".into(),
            description: "summarize".into(),
            input: json!({}),
            output: json!({}),
            price: Some(DiscoveryPrice { amount: 1.0, currency: "EUR".into(), model: "fixed".into() }),
            autonomy: vec!["propose".into()],
        }];
        let body = json!({ "capabilities": caps });
        let (s, _) = route(&arc, "POST", "/v1/announce", &body.to_string().into_bytes(), Some(&format!("Bearer {provider_tok}")));
        assert_eq!(s, 200);

        // Propose + accept + park (on-chain) + deliver + accept-delivery (on-chain release).
        let terms = Terms {
            input: json!({}),
            deliverable: json!({}),
            acceptance_criteria: vec!["ok".into()],
            deadline: now_unix() + 3600,
            price: Price { amount: 1.0, currency: "EUR".into(), model: "fixed".into(), cap: Some(5.0) },
            autonomy: "propose".into(),
            confidentiality: None,
        };
        let body = json!({ "provider": provider_did, "capability_id": "cap:p:analyze", "terms": terms, "escrow": true });
        let (s, v) = route(&arc, "POST", "/v1/contract/propose", &body.to_string().into_bytes(), Some(&format!("Bearer {client_tok}")));
        assert_eq!(s, 200, "propose failed: {v}");
        let contract_id = v["contract_id"].as_str().unwrap().to_string();

        let (s, _) = route(&arc, "POST", &format!("/v1/contract/{contract_id}/accept"), &[], Some(&format!("Bearer {provider_tok}")));
        assert_eq!(s, 200, "accept failed");

        let body = json!({ "contract_id": contract_id, "amount": 1.0 });
        let (s, _) = route(&arc, "POST", "/v1/escrow/park", &body.to_string().into_bytes(), Some(&format!("Bearer {client_tok}")));
        assert_eq!(s, 200, "on-chain park failed");

        let body = json!({ "deliverable_hash": "sha256:abc" });
        let (s, _) = route(&arc, "POST", &format!("/v1/contract/{contract_id}/deliver"), &body.to_string().into_bytes(), Some(&format!("Bearer {provider_tok}")));
        assert_eq!(s, 200, "deliver failed");

        let (s, v) = route(&arc, "POST", &format!("/v1/contract/{contract_id}/accept-delivery"), &[], Some(&format!("Bearer {client_tok}")));
        assert_eq!(s, 200, "accept-delivery failed: {v}");
        assert_eq!(v["state"], "accepted");
        assert_eq!(v["settlement"]["chain"], "onchain");

        // Audit records the on-chain events.
        let (s, v) = route(&arc, "GET", "/v1/audit", &[], None);
        assert_eq!(s, 200);
        let events = v["events"].as_array().unwrap();
        assert!(events.iter().any(|e| e["kind"] == "pay.parked.onchain"));
        assert!(events.iter().any(|e| e["kind"] == "pay.released.onchain"));
    }
}
