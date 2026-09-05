//! GAP node — the HTTP server agents point at.
//!
//! Implements the API documented in `docs/node-api.md`:
//! identity, announce, discover, contracts, escrow, workflows, audit.
//!
//! The node holds agent identities (key custody), the registry, the
//! escrows, and the audit spine. Agents speak HTTPS to it; they never
//! implement GAP themselves.

use crate::amount::Amount;
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
use std::cmp::Ordering;
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    /// What the job was worth, as a decimal string, and in what.
    ///
    /// Denormalised on purpose. Volume used to be summed by looking up
    /// every job's contract on every page load - fine at four thousand
    /// jobs, and 621,462 contract lookups under the global lock once the
    /// full history was restored, most of them reaching ClickHouse.
    /// A settled job's price never changes, so carrying it here costs a
    /// few bytes and removes the reason to ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
}

/// Running totals over every job, so that publishing them costs nothing.
///
/// Kept up to date on the write path and computed once at hydrate. The
/// alternative - recomputing from the jobs map per request - is what
/// made /activity take 2.75 seconds.
#[derive(Default, Clone)]
struct JobStats {
    total: u64,
    judged: u64,
    conforming: u64,
    on_time: u64,
    remedied: u64,
    /// currency -> minor units settled
    volume: std::collections::BTreeMap<String, u128>,
    /// Jobs with no price recorded. Counted rather than treated as
    /// zero: understating volume is still misreporting it.
    unpriced: u64,
    /// capability id -> how many contracts it has actually settled.
    ///
    /// The directory is otherwise a list of claims: an agent announces
    /// what it can do and a price, and nothing distinguishes a
    /// capability that has delivered three hundred times from one
    /// announced this morning and never exercised. This node is the
    /// only party that can tell them apart, because it holds the
    /// history - so it should say. Bounded by the number of distinct
    /// capabilities, not by traffic.
    by_capability: std::collections::BTreeMap<String, u64>,
}

impl JobStats {
    fn add(&mut self, r: &JobRecord) {
        self.total += 1;
        if r.verdict.is_some() {
            self.judged += 1;
        }
        if r.verdict.as_deref() == Some("conforms") {
            self.conforming += 1;
        }
        if r.on_time {
            self.on_time += 1;
        }
        if r.remedied {
            self.remedied += 1;
        }
        *self
            .by_capability
            .entry(r.capability_id.clone())
            .or_insert(0) += 1;
        match (&r.amount, &r.currency) {
            (Some(a), Some(c)) => {
                let minor = crate::amount::Amount::parse(a)
                    .map(|v| v.minor_units())
                    .unwrap_or(0);
                *self.volume.entry(c.clone()).or_insert(0) += minor;
            }
            _ => self.unpriced += 1,
        }
    }
}

/// An agent's dispute record (RFC-0015 §3.3).
///
/// Counting raw disputes would punish the honest agent that bad
/// counterparties keep challenging, and it would hand anyone a griefing
/// weapon: dispute a competitor's every contract to tarnish it. The
/// signal of abuse is **disputing and being wrong**, so the published
/// figure is a win rate, and disputes merely received are tracked
/// separately.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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

/// The deliverable as a judge should read it, or `None` when it is not
/// something a judge can read at all.
///
/// The question is whether the artifact is READABLE, which the media
/// type answers - not how it travelled, which the encoding answers.
/// Keying this on the encoding alone sent markdown and JSON to the
/// judges as a sentence describing their own size, and the judges said
/// so, repeatedly: "the metadata identifies the artifact as
/// text/markdown, but does not provide its contents", "without the
/// artifact contents it cannot be determined". The only honest answer
/// with nothing to read is `inconclusive`, which releases no money - so
/// a provider that base64'd a perfectly good document could not be paid
/// for it. Six of the eight inconclusive verdicts on the public node
/// were this, and none of them were about the work.
///
/// Binary still gets described rather than pasted, and an image is
/// attached separately as an image.
fn judge_readable_text(d: &crate::storage::DeliverableRecord) -> Option<String> {
    if d.encoding != "base64" {
        return None; // already text; the caller passes it through as-is
    }
    let base = d.media_type.split(';').next().unwrap_or("").trim();
    let readable = matches!(
        base,
        "" | "application/json"
            | "application/xml"
            | "application/yaml"
            | "application/x-yaml"
            | "application/javascript"
            | "application/sql"
    ) || base.starts_with("text/")
        || base.ends_with("+json")
        || base.ends_with("+xml");
    if !readable {
        return None;
    }
    crate::artifact::decode_base64(&d.content)
        .and_then(|b| String::from_utf8(b).ok())
        .filter(|s| !s.trim().is_empty())
}

/// Storage key for one agent's record of one job.
///
/// `<agent did>/<job ref>`. A DID never contains a slash and a job ref
/// is hex, so the split is unambiguous in both directions - which
/// matters because hydrate has to recognise the legacy shape (key = the
/// DID alone, value = the whole list) and migrate it.
fn job_key(agent: &str, job_ref: &str) -> String {
    format!("{agent}/{job_ref}")
}

fn pseudonym(input: &str) -> String {
    crate::sha256_hex(input.as_bytes())[..16].to_string()
}

/// The node state — shared behind a mutex, one process, one order.
/// A remembered answer to "does this chain hang together".
///
/// `/v1/audit/verify` walks the entire spine and recomputes every hash.
/// That is the point - a tamper-evidence claim nobody can check is not
/// evidence - but it is O(chain), it is public, and it is
/// unauthenticated. Measured on the live node: ~3 microseconds an
/// event, so 25 ms at eight thousand events and about three SECONDS at
/// a million. Left uncached, any stranger could pin a core to it in a
/// loop.
struct SpineCheck {
    /// Spine height the answer was computed at.
    head: u64,
    at: std::time::Instant,
    took: std::time::Duration,
    value: Value,
}

/// How many finished contracts stay in memory behind the live ones.
///
/// Read traffic is overwhelmingly recent - the activity feed, a job page
/// opened seconds after it settled - so a small window absorbs nearly
/// every lookup and anything older is one ClickHouse read away.
pub const TERMINAL_WINDOW: usize = 5_000;

/// How many recent jobs stay ready for the public feed.
///
/// Comfortably above the fifty a page asks for, so that a request never
/// has to reach past it.
pub const RECENT_JOBS: usize = 200;

/// The materialised contract set, bounded.
///
/// Keeping every contract ever signed in one `HashMap` costs about a
/// kilobyte a deal, for ever: on 2026-08-20 this node held 357,099 of
/// them, 2.5 GiB resident with the whole machine's swap consumed, and it
/// was still climbing by a gigabyte a day. Bounding the spine in August
/// fixed the same disease in a different organ and left this one alone.
///
/// A contract still in flight has to be resident - it is read and
/// written at every step - but that set is bounded by real activity, not
/// by history. A finished one is a record, and records live in storage,
/// which is what storage is for.
#[derive(Default)]
struct Contracts {
    /// Every contract that can still move. Always resident.
    live: HashMap<String, Contract>,
    /// The most recently finished ones, as a read cache.
    recent: HashMap<String, Contract>,
    /// Eviction order for `recent`.
    order: std::collections::VecDeque<String>,
    /// Contracts ever seen. `live.len() + recent.len()` is a cache
    /// size, not a count, and reporting it as one would silently shrink
    /// the node's own history the day eviction starts.
    total: u64,
}

impl Contracts {
    fn get(&self, id: &str) -> Option<&Contract> {
        self.live.get(id).or_else(|| self.recent.get(id))
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Contract> {
        if self.live.contains_key(id) {
            return self.live.get_mut(id);
        }
        self.recent.get_mut(id)
    }

    fn contains_key(&self, id: &str) -> bool {
        self.live.contains_key(id) || self.recent.contains_key(id)
    }

    /// Contracts that can still change, which is the only set the
    /// expiry sweep has ever cared about - it skips every other state.
    fn live(&self) -> impl Iterator<Item = (&String, &Contract)> {
        self.live.iter()
    }

    fn total(&self) -> u64 {
        self.total
    }

    /// Number of contracts held in memory, for the operator view.
    fn resident(&self) -> usize {
        self.live.len() + self.recent.len()
    }

    /// Insert or replace, returning the ids pushed out of the window.
    ///
    /// The caller needs to know: an escrow outlives nothing useful once
    /// its contract is gone, but dropping escrows on their own state
    /// would break three predicates that read "absent from the map" as
    /// "no escrow was ever created" - including the one that decides
    /// whether it is safe to start work. Tying the two evictions
    /// together keeps that equivalence true for every contract that can
    /// still move, which is the only case those predicates are asked
    /// about, and so costs no lookup on the hot path.
    fn insert(&mut self, contract: Contract) -> Vec<String> {
        let mut evicted = Vec::new();
        let id = contract.contract_id.clone();
        if !self.contains_key(&id) {
            self.total += 1;
        }
        if contract.state.is_terminal() {
            // Crossing into a terminal state is what moves a contract
            // out of the working set; doing it here means no call site
            // has to remember to.
            self.live.remove(&id);
            if self.recent.insert(id.clone(), contract).is_none() {
                self.order.push_back(id);
            }
            while self.order.len() > TERMINAL_WINDOW {
                if let Some(old) = self.order.pop_front() {
                    self.recent.remove(&old);
                    evicted.push(old);
                }
            }
        } else {
            self.recent.remove(&id);
            self.live.insert(id, contract);
        }
        evicted
    }
}

pub struct NodeState {
    /// The last spine verification, and what it cost.
    spine_check: Option<SpineCheck>,
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
    /// contract_id -> contract (materialized state), bounded.
    contracts: Contracts,
    /// contract_id -> escrow instance.
    escrows: HashMap<String, Escrow>,
    /// workflow_id -> workflow manifest and engine state.
    workflows: HashMap<String, (Workflow, WorkflowEngine)>,
    /// Optional on-chain relayer (when configured, escrow goes on-chain).
    relayer: Option<Relayer>,
    /// The audit spine.
    pub storage: Box<dyn Storage>,
    /// Policies, by id (RFC-0004). Rebuilt into an `Engine` per
    /// evaluation so that adding one takes effect immediately.
    policies: HashMap<String, crate::policy::Policy>,
    /// Settlements consented to but still inside their cooling-off
    /// window (RFC-0009), contract id -> pending record.
    cooling_off: HashMap<String, Value>,
    /// Registered delegation chains, agent DID -> chain (RFC-0001).
    ///
    /// The node has to know which tree an agent belongs to before it
    /// can aggregate anything per tree, and RFC-0007 is entirely about
    /// aggregating per tree. Without this the sybil defences had
    /// nothing to key on, which is why they were never called.
    delegations: HashMap<String, crate::delegation::TokenChain>,
    /// Per-tree rate counters (RFC-0007). Keyed by tree root, so
    /// spawning sub-agents multiplies nothing.
    tree_limits: crate::sybil::TreeBucket,
    /// Payout requests, by id. Persisted under the "withdrawals" scope
    /// so a restart cannot lose money somebody is owed.
    withdrawals: HashMap<String, Value>,
    /// Projection writes that failed. The spine itself is checked at
    /// the call site; these secondary tables were not, and a dropped
    /// `Result` here is how a contract can be served from memory for
    /// weeks and then vanish on the next restart, with nothing anywhere
    /// saying when it was lost. Counted and surfaced rather than
    /// swallowed.
    persist_failures: u64,
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
    /// Registered pass-through routes, by slug (see `crate::gateway`).
    gateways: HashMap<String, crate::gateway::GatewayRoute>,
    /// Running totals over `jobs`, so that publishing them is a read.
    job_stats: JobStats,
    /// The most recent jobs, newest last, bounded.
    ///
    /// The public feed wants the last fifty. It used to get them by
    /// collecting every job on the node into a Vec and sorting it -
    /// 621,462 pointers and a sort, per request, under the global lock.
    recent_jobs: std::collections::VecDeque<JobRecord>,
    /// Per-agent job history, the raw material of reputation (RFC-0014 §5).
    ///
    /// Deliberately NOT bounded, unlike contracts, escrows and verdicts.
    /// Measured at 357,099 contracts: a `JobRecord` is about 250 bytes,
    /// so this map was roughly 90 MB of a 4.3 GB process - two percent.
    /// Capping it would buy that two percent and cost the job pages:
    /// `public_job` finds a record by scanning for its `job_ref`, and
    /// the persisted form is keyed per agent, so there is no cheap way
    /// to read one back by reference. Worth revisiting the day the
    /// stored shape is per job rather than per agent - which is also
    /// what would fix the separate bug where an agent's whole job list
    /// travels in the URL and stops persisting once it outgrows it.
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
    /// What this node declares about who holds funds (RFC-0016).
    custody: crate::custody::CustodyPolicy,
    /// agent DID -> prefunded balance. Settling a five-cent contract on
    /// chain costs about what the contract is worth, so below the
    /// declared threshold settlement is a ledger entry instead.
    balances: HashMap<String, crate::custody::Balance>,
    /// Transaction hashes already credited. Replaying one transfer is
    /// the cheapest attack available; the only defence is to remember.
    credited_deposits: std::collections::HashSet<String>,
    /// Read-only chain access used to verify deposits.
    deposit_chain: Option<Box<dyn crate::relayer::Chain>>,
    /// contract id -> amount held on the ledger rail. Recorded rather
    /// than inferred: "has no Escrow object" also describes the
    /// on-chain path, and re-deriving it from the policy would break if
    /// the policy changed between park and release.
    balance_holds: HashMap<String, Amount>,
    /// GAP Runtime's global project projection. The durable copy uses
    /// the existing ClickHouse-backed generic state table.
    cloud_projects: HashMap<String, crate::cloud::ProjectRecord>,
    /// Tenant SQLite files live below this directory, one directory per project.
    cloud_root: std::path::PathBuf,
    /// Internal-only function runner. Empty configuration fails closed.
    function_sandbox_url: Option<String>,
    function_sandbox_token: Option<String>,
    realtime_secret: Option<String>,
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

        // Ordered oldest-first so that the newest finished contracts are
        // the ones that survive the window: inserting in storage order
        // would keep whichever 5,000 the backend happened to return
        // last, which is not the same thing and is invisible when it is
        // wrong.
        let mut records = storage.list_contracts().unwrap_or_default();
        records.sort_by_key(|r| r.updated_at);
        let mut contracts = Contracts::default();
        // Every contract id ever seen, resident or not. The job index
        // below is keyed on a pseudonym of this and has to cover the
        // whole history: a link that resolves only while the contract
        // happens to be cached is a page that 404s on a schedule.
        let mut all_ids: Vec<String> = Vec::with_capacity(records.len());
        for rec in records {
            if let Ok(mut contract) = serde_json::from_str::<Contract>(&rec.contract_json) {
                if let Ok(state) = ContractState::parse(&rec.state) {
                    contract.state = state;
                }
                all_ids.push(contract.contract_id.clone());
                contracts.insert(contract);
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

        // Reload the projections that used to live only in RAM.
        //
        // Each is best-effort per entry: a row that no longer
        // deserializes (an older shape, a truncated write) is skipped
        // with a warning rather than taking the node down. Losing one
        // record is bad; refusing to boot because of it is worse.
        fn load<T: serde::de::DeserializeOwned>(
            storage: &dyn Storage,
            scope: &str,
        ) -> HashMap<String, T> {
            let mut out = HashMap::new();
            for rec in storage.list_state(scope).unwrap_or_default() {
                match serde_json::from_str::<T>(&rec.value) {
                    Ok(v) => {
                        out.insert(rec.key, v);
                    }
                    Err(e) => eprintln!("gap-node: skipping {scope}/{}: {e}", rec.key),
                }
            }
            out
        }

        let gateways: HashMap<String, crate::gateway::GatewayRoute> = load(&*storage, "gateways");
        let verdicts: HashMap<String, crate::verifier::Verdict> = load(&*storage, "verdicts");
        // Jobs are read in both shapes: one row per job (current), and
        // one row per agent holding the whole list (legacy). Reading
        // only the new shape would silently drop every job recorded
        // before this change, including the 21,290 rebuilt from
        // contracts on 2026-08-20 - so the old rows are loaded, then
        // rewritten one per job and tombstoned, once.
        let mut jobs: HashMap<String, Vec<JobRecord>> = HashMap::new();
        let mut legacy_agents: Vec<String> = Vec::new();
        for rec in storage.list_state("jobs").unwrap_or_default() {
            match rec.key.split_once('/') {
                Some((agent, _job_ref)) => match serde_json::from_str::<JobRecord>(&rec.value) {
                    Ok(v) => jobs.entry(agent.to_string()).or_default().push(v),
                    Err(e) => eprintln!("gap-node: skipping jobs/{}: {e}", rec.key),
                },
                None => match serde_json::from_str::<Vec<JobRecord>>(&rec.value) {
                    Ok(list) => {
                        jobs.entry(rec.key.clone()).or_default().extend(list);
                        legacy_agents.push(rec.key.clone());
                    }
                    Err(e) => eprintln!("gap-node: skipping jobs/{}: {e}", rec.key),
                },
            }
        }
        // A job can arrive from both shapes during the migration boot;
        // the job ref is unique per agent, so dedupe on it and keep the
        // history in the order it happened.
        for list in jobs.values_mut() {
            list.sort_by_key(|r| r.at);
            let mut seen = std::collections::HashSet::new();
            list.retain(|r| seen.insert(r.job_ref.clone()));
        }
        // One pass, at boot, over records that carry their own price -
        // no contract lookups, which is the whole point.
        let mut job_stats = JobStats::default();
        for r in jobs.values().flatten() {
            job_stats.add(r);
        }
        // The feed window, chosen once here rather than rebuilt per
        // request. Sorting by sequence is what the feed pages on.
        // References first, clone only the window. Cloning every record
        // to keep two hundred of them would allocate 621,462 times at
        // boot for a two-hundred-element result - the same "collect
        // everything, throw away nearly all of it" shape this change
        // exists to remove from the page path.
        let mut newest: Vec<&JobRecord> = jobs.values().flatten().collect();
        newest.sort_by_key(|r| r.seq);
        let window = newest.len().saturating_sub(RECENT_JOBS);
        let recent_jobs: std::collections::VecDeque<JobRecord> =
            newest[window..].iter().map(|r| (*r).clone()).collect();
        let disputes: HashMap<String, DisputeStats> = load(&*storage, "disputes");
        let bindings: HashMap<String, crate::principal::PrincipalBinding> =
            load(&*storage, "bindings");
        let vetoes: HashMap<String, Vec<crate::principal::Veto>> = load(&*storage, "vetoes");
        let budgets: HashMap<String, crate::principal::BudgetGrant> = load(&*storage, "budgets");
        let escalations: HashMap<String, crate::verifier::Escalation> =
            load(&*storage, "escalations");
        let subscriptions: HashMap<String, crate::delivery::Subscription> =
            load(&*storage, "subscriptions");
        // Balances are other people's money: losing them on a restart
        // would be the most expensive amnesia of all.
        let balances: HashMap<String, crate::custody::Balance> = load(&*storage, "balances");
        let balance_holds: HashMap<String, Amount> = load(&*storage, "holds");
        let credited_deposits: std::collections::HashSet<String> = storage
            .list_state("credited")
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.key)
            .collect();
        let cloud_projects: HashMap<String, crate::cloud::ProjectRecord> =
            load(&*storage, "cloud_projects");

        // job_ref -> contract is derivable from the job history, so it
        // is rebuilt rather than stored twice and allowed to disagree.
        //
        // Derive it from the CONTRACTS, not from the verdicts. A job_ref
        // is the pseudonym of a contract id and exists for every settled
        // contract; a verdict is optional and, before the deterministic
        // tier started running on acceptance, most settlements had none.
        // Rebuilding from verdicts therefore dropped exactly those jobs:
        // they still appeared in an agent's public history, because that
        // is read from `jobs`, but their links resolved to nothing. The
        // page rendered a row and the link under it 404'd.
        //
        // Verdicts stay in as a second source so that a job whose
        // contract has gone missing from storage still resolves.
        let mut by_pseudonym: HashMap<String, String> = all_ids
            .iter()
            .map(|cid| (pseudonym(cid), cid.clone()))
            .collect();
        for cid in verdicts.keys() {
            by_pseudonym
                .entry(pseudonym(cid))
                .or_insert_with(|| cid.clone());
        }
        // Reputation is earned, and it was being forgotten.
        //
        // `Reputation` lives on the in-memory agent identity, and the
        // identity table stores only a seed - so every restart rebuilt
        // each agent from its seed with a fresh, empty counter. Score
        // returned to the 0.50 prior and `n` to zero, no matter how many
        // contracts the agent had actually settled. The public page said
        // "0.50 over 0 verified job(s)" beside a job history that listed
        // the jobs, because the history is persisted and the counter
        // was not.
        //
        // Replayed from that history rather than stored beside it. The
        // job records are the evidence the score claims to summarise, so
        // deriving one from the other is the only version that cannot
        // drift; a second persisted counter would eventually disagree
        // with the list underneath it, and there would be no way to say
        // which was right.
        for (did_str, records) in jobs.iter() {
            let Some(agent) = agents_by_did
                .get(did_str)
                .and_then(|t: &String| agents.get_mut(t))
            else {
                continue;
            };
            for r in records {
                // Same predicate the live path uses: what counts is
                // whether the work was accepted, not how it got there.
                agent
                    .identity
                    .reputation_mut()
                    .record(r.outcome == "accepted", r.on_time);
            }
            // The discovery registry keeps its own copy so that
            // `min_reputation` can filter without touching identities.
            // Left unset, a restart silently reopened every
            // reputation-filtered query to agents that had not earned
            // the score.
            if let Ok(did) = crate::identity::Did::parse(did_str) {
                registry.set_reputation(did, agent.identity.reputation().success_rate());
            }
        }

        let withdrawals: HashMap<String, Value> = load(&*storage, "withdrawals");
        let delegations: HashMap<String, crate::delegation::TokenChain> =
            load(&*storage, "delegations");
        let cooling_off: HashMap<String, Value> = load(&*storage, "cooling_off");
        let policies: HashMap<String, crate::policy::Policy> = load(&*storage, "policies");

        let mut jobs_by_ref = HashMap::new();
        for records in jobs.values() {
            for r in records {
                if let Some(cid) = by_pseudonym.get(&r.job_ref) {
                    jobs_by_ref.insert(r.job_ref.clone(), cid.clone());
                }
            }
        }

        // The daily spend counter: keyed "did|day", and only today's
        // rows matter. Without it a restart hands out a fresh allowance.
        let today = now_unix() / 86_400;
        let mut spend_today = HashMap::new();
        for rec in storage.list_state("spend").unwrap_or_default() {
            let (did, day) = match rec.key.rsplit_once('|') {
                Some((d, day)) => (d.to_string(), day.parse::<u64>().unwrap_or(0)),
                None => continue,
            };
            if day != today {
                continue;
            }
            if let Ok(text) = serde_json::from_str::<String>(&rec.value) {
                if let Ok(amount) = crate::amount::Amount::parse(&text) {
                    spend_today.insert((did, day), amount);
                }
            }
        }

        let mut state = Self {
            spine_check: None,
            node: NodeIdentity { identity },
            agents,
            agents_by_did,
            registry,
            contracts,
            escrows,
            workflows: HashMap::new(),
            relayer: None,
            storage,
            withdrawals,
            delegations,
            cooling_off,
            policies,
            tree_limits: crate::sybil::TreeBucket::new(),
            persist_failures: 0,
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
            verdicts,
            gateways,
            job_stats,
            recent_jobs,
            jobs,
            disputes,
            jobs_by_ref,
            bindings,
            vetoes,
            budgets,
            spend_today,
            escalations,
            subscriptions,
            outbox: Vec::new(),
            custody: crate::custody::CustodyPolicy::from_env(),
            balances,
            balance_holds,
            credited_deposits,
            deposit_chain: None,
            cloud_projects,
            cloud_root: std::env::var("GAP_CLOUD_ROOT")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("/data/runtime-projects")),
            function_sandbox_url: std::env::var("GAP_FUNCTION_SANDBOX_URL")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            function_sandbox_token: std::env::var("GAP_FUNCTION_SANDBOX_TOKEN")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            realtime_secret: std::env::var("GAP_REALTIME_SECRET")
                .ok()
                .filter(|v| !v.trim().is_empty()),
        };

        // One-time migration of the legacy jobs rows, done here rather
        // than lazily: a node that keeps reading the old shape keeps
        // paying for it, and a migration that never runs to completion
        // is a second shape to support for ever. Failures are reported
        // and the legacy row is left in place, so a partial run is
        // simply retried on the next boot.
        if !legacy_agents.is_empty() {
            let now = now_unix();
            let mut rows: Vec<crate::storage::StateRecord> = Vec::new();
            for agent in &legacy_agents {
                for r in state.jobs.get(agent).cloned().unwrap_or_default() {
                    rows.push(crate::storage::StateRecord {
                        scope: "jobs".into(),
                        key: job_key(agent, &r.job_ref),
                        value: serde_json::to_string(&r).unwrap_or_default(),
                        updated_at: now,
                    });
                }
            }
            match state.storage.upsert_state_many(&rows) {
                Ok(()) => {
                    // Only once every record is out do the lists go: the
                    // tombstone is what makes this irreversible, so it
                    // waits for a write that actually succeeded.
                    for agent in &legacy_agents {
                        if let Err(e) = state.storage.delete_state("jobs", agent) {
                            eprintln!("gap-node: jobs migration: cannot drop legacy {agent}: {e}");
                        }
                    }
                    eprintln!(
                        "[gap-node] jobs migration: {} agent list(s) split into {} row(s)",
                        legacy_agents.len(),
                        rows.len()
                    );
                }
                // Left exactly as it was, to be retried on the next boot.
                Err(e) => eprintln!("gap-node: jobs migration failed, legacy rows kept: {e}"),
            }
        }

        state
    }

    /// Configure the node-arbitration admin token.
    pub fn set_admin_token(&mut self, token: impl Into<String>) {
        self.admin_token = Some(token.into());
    }

    pub fn set_cloud_root(&mut self, root: impl Into<std::path::PathBuf>) {
        self.cloud_root = root.into();
    }

    pub fn set_function_sandbox(&mut self, url: &str, token: &str) {
        self.function_sandbox_url = Some(url.trim_end_matches('/').to_string());
        self.function_sandbox_token = Some(token.to_string());
    }

    pub fn set_realtime_secret(&mut self, secret: &str) {
        self.realtime_secret = Some(secret.to_string());
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
        let r = self.storage.upsert_contract(&record);
        self.note_persist(r, "contract", &record.contract_id.clone());
    }

    /// Record a projection write outcome. Never fails the request: a
    /// deliverable that is already committed to the spine must not be
    /// rejected because a secondary table hiccuped. But it is no longer
    /// invisible.
    fn note_persist<T>(&mut self, r: Result<T>, kind: &str, key: &str) {
        if let Err(e) = r {
            self.persist_failures += 1;
            eprintln!("gap-node: cannot persist {kind} {key}: {e}");
        }
    }

    /// How many projection writes have failed since this process
    /// started. Exposed so "the node looks fine" and "the node is
    /// silently losing records" stop looking identical.
    pub fn persist_failures(&self) -> u64 {
        self.persist_failures
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
        let r = self.storage.upsert_escrow(&record);
        self.note_persist(r, "escrow", &record.contract_id.clone());
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
        // PER-TREE cap (RFC-0007). The per-token cap above is what a
        // Sybil walks straight through: spawn ten sub-agents, get ten
        // tokens, get ten times the budget. Aggregating by delegation
        // root is the entire point of the RFC, and it was written and
        // never called.
        //
        // Applied only to actors that registered a chain, because an
        // agent with no chain IS its own tree and the per-token cap
        // already is its per-tree cap. Same ceiling either way, so
        // declaring a chain can only tighten, never loosen.
        if let Some(t) = token {
            if let Some(chain) = self
                .agents
                .get(t)
                .map(|a| a.identity.did().to_string())
                .and_then(|did| self.delegations.get(&did))
            {
                self.tree_limits
                    .record_invocation(chain, now, self.token_cap)?;
            }
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
    /// Give the node read access to a chain, so it can verify deposits.
    ///
    /// Separate from the relayer: reading receipts needs no keys and no
    /// escrow contract, and a node can perfectly well verify incoming
    /// deposits while settling everything else off chain.
    pub fn set_deposit_chain(&mut self, chain: Box<dyn Chain>) {
        self.deposit_chain = Some(chain);
    }

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
        let r = self.storage.upsert_identity(&IdentityRecord {
            token: token.clone(),
            did: did.clone(),
            seed_hex,
            created_at: now_unix(),
        });
        // An identity that does not land is an agent that loses its key
        // and its history at the next restart. Worth a line in the log.
        self.note_persist(r, "identity", &did.clone());
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
        languages: Vec<String>,
        regions: Vec<String>,
        ttl_seconds: u64,
    ) -> Result<String> {
        self.announce_request(
            token,
            crate::discovery::AnnounceRequest {
                capabilities,
                languages,
                regions,
                ttl_seconds,
                ..Default::default()
            },
        )
    }

    /// Announce with the agent's declared reachability (spec 02 §2.2)
    /// and its self-declared profile.
    ///
    /// The node used to overwrite whatever the agent declared with a
    /// placeholder (`https://agent/<did>/gap`, not a routable host),
    /// which discarded the very data event delivery needs - spec §2.4.4
    /// requires a registry to support a transport *listed by the agent*
    /// (RFC-0013 §2.2). Declared entries are now stored verbatim; the
    /// node-mediated entry is appended, clearly marked.
    ///
    /// Re-announcing is also how an agent renames itself: the registry
    /// is an upsert keyed on the DID, so the newest announcement wins
    /// and there is no separate update call to forget to implement.
    pub fn announce_request(
        &mut self,
        token: &str,
        req: crate::discovery::AnnounceRequest,
    ) -> Result<String> {
        let agent = self
            .agents
            .get_mut(token)
            .ok_or_else(|| Error::Unauthorized("invalid bearer token".into()))?;
        let mut reachability = req.reachability;
        reachability.push(Reachability {
            transport: "gap-node".into(),
            endpoint: format!("/v1/contract/?agent={}", agent.identity.did()),
        });
        let mut ann = Announcement::signed(
            &agent.identity,
            req.capabilities,
            reachability,
            req.ttl_seconds,
        );
        ann.languages = req.languages;
        ann.regions = req.regions;
        ann.name = req.profile.name;
        ann.description = req.profile.description;
        ann.resign(&agent.identity);
        ann.verify()?;
        self.registry.announce(ann.clone())?;
        let id = format!("urn:gap:ann:{}", &ann.agent_did.to_string()[..16]);
        agent.announcement = Some(ann);
        if let Some(saved) = &agent.announcement {
            let did = saved.agent_did.to_string();
            let r = self.storage.upsert_announcement(&AnnouncementRecord {
                agent_did: did.clone(),
                announcement_json: serde_json::to_string(saved).unwrap_or_else(|_| "{}".into()),
                expires_at: now_unix().saturating_add(saved.ttl_seconds),
            });
            self.note_persist(r, "announcement", &did);
        }
        self.record("cap.announce", json!({ "agent_did": self.agent_by_token(token).map(|a| a.identity.did().to_string()).unwrap_or_default() }));
        Ok(id)
    }

    /// Query the registry.
    /// Answer a query and sign the answer as the registry.
    pub fn discover_signed(&self, query: &Query) -> crate::discovery::SignedQueryResult {
        crate::discovery::SignedQueryResult::signed(
            &self.node.identity,
            query,
            self.registry.query(query),
        )
    }

    /// The node's own X25519 public key, so agents can seal payloads to
    /// it (and so the AgentCard can publish it).
    pub fn node_encryption_key(&self) -> String {
        crate::sealed::EncryptionKey::of(&self.node.identity).public_hex()
    }

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
        let provider = crate::identity::Did::parse(provider_did)?;
        // O(1) provider lookup via the DID index (previously a scan of
        // every registered agent — quadratic once the node serves
        // many identities).
        if !self.agents_by_did.contains_key(provider_did) {
            return Err(Error::UnknownContract(format!(
                "provider {provider_did} not registered on this node"
            )));
        }
        // RFC-0004: the layered policy engine, evaluated before a
        // contract exists rather than after. A deny that arrives once
        // both parties have signed is an argument, not a guardrail.
        if let Some(decision) = self.evaluate_policy(&client_did, &provider, capability_id, &terms)
        {
            let denied = decision.outcome == "deny";
            self.record(
                "pol.decision",
                json!({
                    "outcome": decision.outcome,
                    "capability_id": capability_id,
                    "applied_rules": decision.applied_rules,
                    "decision_id": decision.decision_id,
                    "explanation": decision.explanation,
                }),
            );
            if denied {
                return Err(Error::AutonomyViolation(format!(
                    "policy denied this contract: {}",
                    decision.explanation
                )));
            }
        }
        let client = self.agent_by_token(client_token)?;
        let contract =
            Contract::propose(&client.identity, provider, capability_id, terms, use_escrow);
        let id = contract.contract_id.clone();
        self.persist_contract(&contract);
        self.set_contract(contract);
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
            self.set_contract(c);
            if let Some(saved) = self.contracts.get(contract_id).cloned() {
                self.persist_contract(&saved);
            }
            self.record("ctr.accept", json!({ "contract_id": contract_id }));
            return Ok(());
        }
        let signed = contract.accept_by_provider(&provider.identity)?;
        self.set_contract(signed);
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
        deliverable_uri: Option<&str>,
        artifact: Option<crate::artifact::Artifact>,
    ) -> Result<()> {
        let provider_did = self.agent_by_token(provider_token)?.identity.did().clone();
        self.check_veto(&provider_did, contract_id)?;
        let funded = self.escrow_is_funded(contract_id);
        let contract = self
            .contracts
            .get_mut(contract_id)
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.provider != provider_did {
            return Err(Error::Unauthorized("only the provider may deliver".into()));
        }
        // The backstop. `exe.start` now refuses first, so reaching this
        // means the provider skipped the optional start call - it should
        // still not hand over work nobody has paid for.
        if contract.escrow && !funded {
            return Err(Error::EscrowViolation(
                "escrow is not parked: the client has not secured payment for this contract. \
Call POST /v1/contract/{id}/start before working - it refuses while unfunded"
                    .into(),
            ));
        }
        if deliverable_hash.trim().is_empty() {
            return Err(Error::Other("deliverable_hash required".into()));
        }
        // Settle the digest's FORM here, at delivery, instead of ruling
        // on it after the fact.
        //
        // Everything downstream already treats `<hex>` and
        // `sha256:<hex>` as the same commitment - `digests_match` says
        // so explicitly. Everything except the deterministic check,
        // which demands the prefix and rules the delivery nonconforming
        // without it. So a provider that sent a bare digest got a 200,
        // and learned otherwise as a permanent nonconforming verdict on
        // its public record. It happened on this node, to work nobody
        // ever read: no judge was consulted, because the delivery never
        // got past the check. The whole mistake was a missing prefix.
        //
        // Normalised, never refused. Refusing anything else here would
        // change what this endpoint accepts for every caller at once,
        // and the failure being fixed is narrow: a real digest missing
        // its prefix. Whatever does not look like a bare sha256 is
        // passed through untouched, exactly as before.
        let normalised;
        let deliverable_hash = {
            let h = deliverable_hash.trim();
            if h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()) {
                normalised = format!("sha256:{h}");
                normalised.as_str()
            } else {
                h
            }
        };
        // Keep the commitment: verification later checks what the client
        // actually received against this exact digest.
        contract.deliverable_hash = Some(deliverable_hash.to_string());
        // The artifact itself, when the provider handed it over.
        // Checked against the commitment HERE rather than at
        // verification: a digest that does not match the bytes is a
        // delivery that was never valid, and saying so immediately is
        // worth far more to the provider than a verdict an hour later.
        let stored = Self::accept_artifact(artifact, deliverable_hash)?;
        // An optional retrieval location for artifacts too large to pass
        // inline. It is a convenience, never an authority - the digest
        // is what the verifier checks, so pointing at bytes that do not
        // hash to it fails verification exactly as it should.
        if let Some(uri) = deliverable_uri.map(str::trim).filter(|u| !u.is_empty()) {
            contract.deliverable_uri = Some(uri.to_string());
        }
        // `exe.start` is optional (spec 04 §4.2): a provider that
        // announced its plan is already Executing, one that went
        // straight to delivery still needs the intermediate step.
        if contract.state == ContractState::Signed {
            contract.transition(ContractState::Executing)?;
        }
        contract.transition(ContractState::Delivered)?;
        let saved = contract.clone();
        self.persist_contract(&saved);
        self.persist_deliverable(contract_id, deliverable_hash, stored, deliverable_uri);
        self.record("exe.deliver", json!({ "contract_id": contract_id }));
        Ok(())
    }

    /// Validate an inline artifact against the digest the provider
    /// committed to. Returns it unchanged when it checks out.
    fn accept_artifact(
        artifact: Option<crate::artifact::Artifact>,
        declared_hash: &str,
    ) -> Result<Option<crate::artifact::Artifact>> {
        let art = match artifact.filter(|a| !a.is_empty()) {
            Some(a) => a,
            None => return Ok(None),
        };
        if art.byte_len() > crate::artifact::MAX_INLINE_BYTES {
            return Err(Error::Other(format!(
                "deliverable is {} bytes, over the {} byte inline limit: host it and send \
deliverable_uri with the digest instead",
                art.byte_len(),
                crate::artifact::MAX_INLINE_BYTES
            )));
        }
        let computed = art.digest()?;
        if !crate::artifact::digests_match(&computed, declared_hash) {
            return Err(Error::Other(format!(
                "deliverable_hash does not match the content supplied: committed to \
{declared_hash}, the content hashes to {computed}"
            )));
        }
        Ok(Some(art))
    }

    /// Record what the node now holds for a contract.
    fn persist_deliverable(
        &mut self,
        contract_id: &str,
        digest: &str,
        artifact: Option<crate::artifact::Artifact>,
        uri: Option<&str>,
    ) {
        let uri = uri.map(str::trim).unwrap_or("");
        if artifact.is_none() && uri.is_empty() {
            return;
        }
        let art = artifact.unwrap_or(crate::artifact::Artifact {
            content: String::new(),
            encoding: "utf8".into(),
            media_type: String::new(),
        });
        let _ = self
            .storage
            .upsert_deliverable(&crate::storage::DeliverableRecord {
                contract_id: contract_id.to_string(),
                digest: digest.to_string(),
                encoding: art.encoding,
                media_type: art.media_type,
                content: art.content,
                uri: uri.to_string(),
                delivered_at: now_unix(),
            });
    }

    /// The artifact the node holds for a contract, if any.
    pub fn deliverable_of(&self, contract_id: &str) -> Option<crate::storage::DeliverableRecord> {
        self.storage.get_deliverable(contract_id).ok().flatten()
    }

    /// `GET /v1/contract/{id}/deliverable` — hand the artifact to a
    /// party to the contract.
    ///
    /// Restricted to the client and the provider. A public directory of
    /// who trades what is one thing; handing any caller the work
    /// somebody paid for is another, and no amount of pseudonymity in
    /// the reputation projection would undo that.
    pub fn fetch_deliverable(&self, token: &str, contract_id: &str) -> Result<Value> {
        let caller = self.agent_by_token(token)?.identity.did().clone();
        let contract = self
            .contracts
            .get(contract_id)
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.client != caller && contract.provider != caller {
            return Err(Error::Unauthorized(
                "only the contract's parties may fetch the deliverable".into(),
            ));
        }
        match self.deliverable_of(contract_id) {
            Some(d) => Ok(json!({
                "contract_id": contract_id,
                "digest": d.digest,
                "encoding": d.encoding,
                "media_type": d.media_type,
                "content": d.content,
                "uri": d.uri,
                "delivered_at": d.delivered_at,
                // Restating the guarantee where it is consumed: these
                // bytes hashed to the committed digest at delivery, so
                // the caller does not have to take the node's word for
                // it - it can hash them again.
                "integrity": "content matched the provider's committed digest at delivery",
            })),
            None => Err(Error::Other(
                "no artifact is held for this contract: the provider committed to a digest \
only. Fetch it from deliverable_uri on the contract, or ask the provider to re-deliver with \
the content inline"
                    .into(),
            )),
        }
    }

    /// Client accepts delivery; escrow releases automatically.
    pub fn accept_delivery(&mut self, client_token: &str, contract_id: &str) -> Result<Value> {
        let client = self.agent_by_token(client_token)?.identity.clone();
        let contract = self
            .contracts
            .get(contract_id)
            .cloned()
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.client != *client.did() {
            return Err(Error::Unauthorized("only the client may accept".into()));
        }
        if contract.state != ContractState::Delivered {
            return Err(Error::InvalidTransition {
                from: contract.state.wire_name().into(),
                to: "accepted".into(),
            });
        }
        // RFC-0009: hold the money for a while before it moves.
        //
        // The window defers the ACCEPTANCE, not the release. Every line
        // of the settlement below - three payment paths, signed
        // envelopes, receipts, job records, reputation - runs later
        // unchanged, by re-entering this same function once the window
        // has elapsed. Extracting that block to call it from two places
        // would have been the larger and far riskier change, and a
        // mistake in it does not fail loudly: it pays the wrong party,
        // or pays twice.
        if let Some(window) = self.cooling_off_for(&contract) {
            let now = now_unix();
            match self.cooling_off.get(contract_id).cloned() {
                None => {
                    let settles_at = now.saturating_add(window);
                    let (_guard, receipt) = crate::irreversibility::IrreversibilityGuard::begin(
                        crate::irreversibility::IrreversibilityClass::Financial,
                        contract_id,
                        now,
                        Some(window),
                        &self.node.identity,
                    );
                    let pending = json!({
                        "contract_id": contract_id,
                        "consented_at": now,
                        "settles_at": settles_at,
                        "window_seconds": window,
                        "receipt": receipt,
                    });
                    self.save_state("cooling_off", contract_id, &pending);
                    self.cooling_off.insert(contract_id.into(), pending);
                    self.record(
                        "pay.cooling_off.pending",
                        json!({ "contract_id": contract_id, "settles_at": settles_at }),
                    );
                    return Ok(json!({
                        "contract_id": contract_id,
                        "state": "cooling_off",
                        "settles_at": settles_at,
                        "window_seconds": window,
                        "receipt": receipt,
                        "note": "Consent is recorded and the escrow stays parked. It settles \
                    when the window elapses - call accept-delivery again then, or POST \
                    /v1/contract/{id}/withdraw-consent before it to cancel.",
                    }));
                }
                Some(p) if p["settles_at"].as_u64().unwrap_or(u64::MAX) > now => {
                    // Idempotent: asking again inside the window is not
                    // an error, it is a status check.
                    return Ok(json!({
                        "contract_id": contract_id,
                        "state": "cooling_off",
                        "settles_at": p["settles_at"],
                        "seconds_remaining": p["settles_at"].as_u64().unwrap_or(0) - now,
                    }));
                }
                Some(_) => {
                    // Elapsed. Clear it and fall through to settle.
                    self.cooling_off.remove(contract_id);
                    self.forget_state("cooling_off", contract_id);
                    self.record(
                        "pay.cooling_off.elapsed",
                        json!({ "contract_id": contract_id }),
                    );
                }
            }
        }

        // RFC-0014: a recorded non-conforming verdict blocks release.
        // The client may accept an *inconclusive* delivery (that is its
        // prerogative — it is its money), but it cannot release funds
        // against signed evidence that the work does not conform; that
        // path is the dispute, where an arbitrator splits.
        // The buyer is the authority on its own money.
        //
        // The judges are advisers, and they are consulted only when the
        // buyer asks - by calling /verify instead of accepting. So a
        // subjective ruling, however adverse, cannot overrule a buyer
        // who has looked at the work and says it is fine. Letting it do
        // so produced the failure that motivated this: a buyer asked for
        // a review, the panel escalated on `judge_disagreement`, and the
        // contract stranded in `delivered` - not `nonconforming`, so no
        // remedy for the provider, and not acceptable either, so the
        // escrow sat parked with nobody able to move it. Neither party
        // had done anything wrong.
        //
        // One escalation still blocks, and it is not the judges
        // speaking. `value_threshold` is the *principal's* own rule
        // (RFC-0015 `human_review_above`): the human who owns the buying
        // agent said that above this amount a person looks first. That
        // is not a judge overruling a buyer, it is a buyer's owner
        // overruling the buyer, which is the whole point of setting it.
        if let Some(v) = self.verdicts.get(contract_id) {
            if v.escalation == Some(crate::verifier::Escalation::ValueThreshold) {
                return Err(Error::EscrowViolation(
                    "this contract is above the principal's human-review threshold (RFC-0015); a person closes it before escrow moves"
                        .into(),
                ));
            }
        }

        // Accepting against an adverse ruling is allowed, but it is not
        // invisible: it goes on the record. A marketplace where buyers
        // quietly wave through work the judges called non-conforming is
        // one where the conformance rate stops meaning anything.
        let overridden = self
            .verdicts
            .get(contract_id)
            .filter(|v| {
                v.ruling == crate::verifier::Ruling::Nonconforming || v.escalation.is_some()
            })
            .map(|v| {
                (
                    v.ruling.as_str().to_string(),
                    v.escalation.map(|e| e.as_str().to_string()),
                )
            });

        // A client may settle without ever asking for verification, and
        // in practice they do: the observed sequence is deliver then
        // accept, with no verify in between. That left every settled job
        // in the public record with no ruling at all, and the node's own
        // conformance rate undefined while jobs piled up underneath it.
        //
        // So when nothing has judged this delivery yet, run the
        // deterministic tier here. It costs nothing - digest and
        // deadline, no model call - and it is the tier the protocol
        // calls authoritative anyway, so there is no reason for it to be
        // optional. The subjective tier stays opt-in through /verify.
        //
        // It records, it does not block: the client's explicit
        // acceptance still governs settlement. A digest mismatch is
        // captured in the job record for the provider's reputation
        // rather than used to overrule a buyer who has seen the work and
        // says it is fine.
        if !self.verdicts.contains_key(contract_id) {
            let held = self.deliverable_of(contract_id);
            let evidence = crate::verifier::Evidence {
                contract_id: contract_id.to_string(),
                capability_id: contract.capability_id.clone(),
                // Deterministic tier only: no criteria means no judge is
                // consulted even if one is configured.
                acceptance_criteria: vec![],
                brief: None,
                deadline: contract.terms.deadline,
                delivered_at: now_unix(),
                declared_hash: contract.deliverable_hash.clone().unwrap_or_default(),
                computed_hash: held.as_ref().map(|d| d.digest.clone()),
                deliverable_excerpt: None,
                image_base64: None,
                image_media_type: None,
                confidential: contract.terms.confidentiality.is_some(),
            };
            let verdict = crate::verifier::verify_panel(&self.node.identity, &evidence, &[], None);
            self.save_state("verdicts", contract_id, &verdict);
            self.verdicts.insert(contract_id.to_string(), verdict);
        }

        // Build the signed acceptance + release envelopes.
        let acceptance = Envelope::new(
            client.did().clone(),
            contract.provider.clone(),
            Kind::ExeAccept,
            match &overridden {
                None => json!({ "verdict": "accepted" }),
                // Signed by the client, so the record shows the buyer
                // knew what it was waving through.
                Some((ruling, escalation)) => json!({
                    "verdict": "accepted",
                    "overrode_verdict": ruling,
                    "overrode_escalation": escalation,
                }),
            },
        )
        .for_contract(contract_id)
        .sign(&client);
        let release = Envelope::new(
            client.did().clone(),
            self.node_did(),
            Kind::PayRelease,
            json!({}),
        )
        .for_contract(contract_id)
        .sign(&client);

        // Custodial path: the hold moves from the client's ledger to
        // the provider's. No transaction, so no gas, which is the whole
        // reason the balance exists.
        if let Some(amount) = self.balance_holds.get(contract_id).copied() {
            let client_did = contract.client.to_string();
            let provider_did = contract.provider.to_string();
            let currency = self.custody.currency.clone();

            let payer = self
                .balances
                .get_mut(&client_did)
                .ok_or_else(|| Error::EscrowViolation("no balance for the client".into()))?;
            payer.settle_held(amount)?;
            self.save_balance(&client_did);

            let payee = self.balances.entry(provider_did.clone()).or_default();
            if payee.currency.is_empty() {
                payee.currency = currency.clone();
            }
            payee.credit(amount);
            self.save_balance(&provider_did);

            let c = self
                .contracts
                .get_mut(contract_id)
                .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
            c.transition(ContractState::Accepted)?;
            let saved = c.clone();
            self.persist_contract(&saved);
            self.persist_escrow(
                contract_id,
                crate::payment::EscrowState::Released,
                Amount::ZERO,
                &currency,
            );
            self.record(
                "pay.released",
                json!({
                    "contract_id": contract_id,
                    "amount": amount.to_string_decimal(),
                    "rail": "balance",
                }),
            );
            self.record(
                "exe.accept",
                match &overridden {
                    None => json!({ "contract_id": contract_id }),
                    Some((ruling, escalation)) => json!({
                        "contract_id": contract_id,
                        "overrode_verdict": ruling,
                        "overrode_escalation": escalation,
                    }),
                },
            );
            let on_time = now_unix() <= contract.terms.deadline;
            self.record_job(
                &contract.provider,
                &contract.client,
                contract_id,
                &contract.capability_id,
                "accepted",
                on_time,
            );
            self.credit_reputation(&contract.provider, true, on_time);
            self.balance_holds.remove(contract_id);
            self.forget_state("holds", contract_id);
            return Ok(json!({
                "state": "accepted",
                "settlement": {
                    "amount": amount.to_string_decimal(),
                    "currency": currency,
                    "rail": "balance"
                }
            }));
        }

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
            self.record(
                "exe.accept",
                match &overridden {
                    None => json!({ "contract_id": contract_id }),
                    Some((ruling, escalation)) => json!({
                        "contract_id": contract_id,
                        "overrode_verdict": ruling,
                        "overrode_escalation": escalation,
                    }),
                },
            );
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

        // The payout, on the spine. The two paths above record their
        // own release — custodial as `pay.released`, relayer as
        // `pay.released.onchain` — and this one, which is the path any
        // contract without a custodial balance takes, recorded only
        // `exe.accept`. So the default settlement moved money, returned
        // an amount to the caller, and left the audit chain with no
        // record that anyone was ever paid: acceptance was provable,
        // payment was not.
        self.record(
            "pay.released",
            json!({
                "contract_id": contract_id,
                // Same shape as the custodial path's receipt: a decimal
                // string, so a subscriber parses one form, not two.
                "amount": amount.to_string_decimal(),
                "rail": "escrow",
            }),
        );
        self.record(
            "exe.accept",
            match &overridden {
                None => json!({ "contract_id": contract_id }),
                Some((ruling, escalation)) => json!({
                    "contract_id": contract_id,
                    "overrode_verdict": ruling,
                    "overrode_escalation": escalation,
                }),
            },
        );
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

        // Custodial path (RFC-0016): below the declared threshold a
        // park is a ledger hold, not a transaction. Settling a
        // five-cent contract on chain costs about what the contract is
        // worth, so the gas is paid twice per agent lifetime instead.
        if self.custody.settles_from_balance(*amount) {
            let did = client_did.to_string();
            let currency = self.custody.currency.clone();
            let entry = self.balances.entry(did.clone()).or_default();
            if entry.currency.is_empty() {
                entry.currency = currency;
            }
            // Refuses rather than going negative: a custodian that lets
            // a balance go below zero has extended credit, which is a
            // different regulated activity from holding deposits.
            entry.hold(*amount)?;
            self.save_balance(&did);
            self.balance_holds.insert(contract_id.to_string(), *amount);
            self.save_state("holds", contract_id, amount);
            self.persist_escrow(
                contract_id,
                crate::payment::EscrowState::Parked,
                *amount,
                &self.custody.currency.clone(),
            );
            self.record(
                "pay.parked",
                json!({
                    "contract_id": contract_id,
                    "amount": amount.to_string_decimal(),
                    "rail": "balance",
                }),
            );
            return Ok(());
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
        self.set_contract(contract.clone());
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
        self.set_contract(contract.clone());
        self.persist_contract(&contract);
        self.persist_escrow(contract_id, escrow_state, escrow_held, &receipt.currency);
        self.save_dispute(&contract.client.to_string());
        self.save_dispute(&contract.provider.to_string());
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
            self.save_dispute(&c.client.to_string());
            self.save_dispute(&c.provider.to_string());
            // A human has now ruled: the case is closed.
            self.escalations.remove(contract_id);
            self.forget_state("escalations", contract_id);
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
        // Read from storage, not from memory: finished contracts are
        // evicted from the in-memory set, and an export that scanned it
        // would hand the agent a silently truncated copy of its own
        // history - the worst possible failure for a call whose entire
        // purpose is portability.
        let contracts: Vec<Contract> = self
            .storage
            .contracts_for_agent(&did)
            .unwrap_or_default()
            .iter()
            .filter_map(|r| {
                serde_json::from_str::<Contract>(&r.contract_json)
                    .ok()
                    .map(|mut c| {
                        if let Ok(state) = ContractState::parse(&r.state) {
                            c.state = state;
                        }
                        c
                    })
            })
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
        match self.storage.append_event(kind, payload.clone()) {
            Ok(seq) => self.enqueue_event(seq, kind, payload),
            // SAY SO. This used to be `if let Ok(..)`, and a failed
            // append was indistinguishable from a successful one: the
            // event still sat in the in-memory mirror, every page kept
            // answering, and the audit chain silently stopped growing on
            // disk. It went unnoticed for an hour because nothing said
            // anything. An event that did not persist is the one failure
            // this node must never be quiet about.
            Err(e) => eprintln!(
                "gap-node: SPINE WRITE FAILED for {kind}: {e} — the event is in memory but                  NOT persisted; it will be missing after a restart"
            ),
        }
    }

    // ---- GAP Runtime -------------------------------------------------

    pub fn cloud_create_project(&mut self, token: &str) -> Result<crate::cloud::ProjectRecord> {
        let owner_did = self.agent_by_token(token)?.identity.did().to_string();
        use rand::RngCore;
        let mut random = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut random);
        let project_id = format!("prj_{}", hex::encode(random));
        crate::cloud::ProjectStore::open(&self.cloud_root, &project_id)?;
        let now = now_unix();
        let project = crate::cloud::ProjectRecord {
            project_id: project_id.clone(),
            owner_did,
            status: "active".into(),
            plan: "free".into(),
            created_at: now,
            updated_at: now,
        };
        self.cloud_projects
            .insert(project_id.clone(), project.clone());
        self.save_state("cloud_projects", &project_id, &project);
        self.record(
            "cloud.project.created",
            json!({ "project_id": project_id, "owner_did": project.owner_did }),
        );
        Ok(project)
    }

    pub fn cloud_list_projects(&self, token: &str) -> Result<Vec<crate::cloud::ProjectRecord>> {
        let owner = self.agent_by_token(token)?.identity.did().to_string();
        let mut projects: Vec<_> = self
            .cloud_projects
            .values()
            .filter(|p| p.owner_did == owner)
            .cloned()
            .collect();
        projects.sort_by_key(|p| p.created_at);
        Ok(projects)
    }

    fn cloud_owned_project(
        &self,
        token: &str,
        project_id: &str,
    ) -> Result<crate::cloud::ProjectRecord> {
        let owner = self.agent_by_token(token)?.identity.did().to_string();
        let project = self
            .cloud_projects
            .get(project_id)
            .cloned()
            .ok_or_else(|| Error::Other("unknown cloud project".into()))?;
        if project.owner_did != owner {
            return Err(Error::Unauthorized(
                "cloud project belongs to another agent".into(),
            ));
        }
        if project.status != "active" {
            return Err(Error::Other("cloud project is not active".into()));
        }
        Ok(project)
    }

    pub fn cloud_put_kv(
        &mut self,
        token: &str,
        project_id: &str,
        key: &str,
        value: &[u8],
        expires_at: Option<u64>,
    ) -> Result<()> {
        self.cloud_owned_project(token, project_id)?;
        crate::cloud::ProjectStore::open(&self.cloud_root, project_id)?.put_kv(
            key,
            value,
            expires_at,
            now_unix(),
        )?;
        self.record(
            "cloud.kv.put",
            json!({ "project_id": project_id, "key_digest": crate::sha256_hex(key.as_bytes()), "size_bytes": value.len() }),
        );
        Ok(())
    }

    pub fn cloud_get_kv(
        &self,
        token: &str,
        project_id: &str,
        key: &str,
    ) -> Result<Option<Vec<u8>>> {
        self.cloud_owned_project(token, project_id)?;
        crate::cloud::ProjectStore::open(&self.cloud_root, project_id)?.get_kv(key, now_unix())
    }

    pub fn cloud_put_object(
        &mut self,
        token: &str,
        project_id: &str,
        key: &str,
        content: &[u8],
        media_type: &str,
    ) -> Result<String> {
        self.cloud_owned_project(token, project_id)?;
        let digest = crate::cloud::ProjectStore::open(&self.cloud_root, project_id)?.put_object(
            key,
            content,
            media_type,
            now_unix(),
        )?;
        self.record(
            "cloud.object.put",
            json!({ "project_id": project_id, "object_digest": digest, "size_bytes": content.len() }),
        );
        Ok(digest)
    }

    pub fn cloud_get_object(
        &self,
        token: &str,
        project_id: &str,
        key: &str,
    ) -> Result<Option<crate::cloud::StoredObject>> {
        self.cloud_owned_project(token, project_id)?;
        crate::cloud::ProjectStore::open(&self.cloud_root, project_id)?.get_object(key)
    }

    fn cloud_prepare_database(&self, token: &str, project_id: &str) -> Result<std::path::PathBuf> {
        self.cloud_owned_project(token, project_id)?;
        Ok(self.cloud_root.clone())
    }

    pub fn cloud_issue_realtime_token(
        &mut self,
        token: &str,
        project_id: &str,
        channels: &[String],
        permissions: &[String],
        subject: Option<&str>,
    ) -> Result<Value> {
        self.cloud_owned_project(token, project_id)?;
        if channels.len() > 25
            || channels.iter().any(|channel| {
                channel.is_empty()
                    || channel.len() > 128
                    || !channel.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b':' | b'.')
                    })
            })
        {
            return Err(Error::Other("invalid realtime channel scope".into()));
        }
        if permissions.is_empty()
            || permissions.len() > 2
            || permissions
                .iter()
                .any(|permission| !matches!(permission.as_str(), "subscribe" | "publish"))
        {
            return Err(Error::Other("invalid realtime permissions".into()));
        }
        if subject.is_some_and(|value| {
            value.is_empty()
                || value.len() > 128
                || !value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b':' | b'.' | b'@')
                })
        }) {
            return Err(Error::Other("invalid realtime subject".into()));
        }
        let secret = self
            .realtime_secret
            .as_deref()
            .ok_or_else(|| Error::Other("realtime service is not configured".into()))?;
        use rand::RngCore;
        let mut nonce = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let expires_at = now_unix().saturating_add(3600);
        let claims = json!({
            "project_id": project_id,
            "channels": channels,
            "permissions": permissions,
            "subject": subject,
            "exp": expires_at,
            "jti": hex::encode(nonce)
        });
        use base64::Engine;
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).map_err(|error| Error::Other(error.to_string()))?);
        use hmac::{Hmac, Mac};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
            .map_err(|_| Error::Other("invalid realtime secret".into()))?;
        mac.update(encoded.as_bytes());
        let signed = format!("{encoded}.{}", hex::encode(mac.finalize().into_bytes()));
        self.record(
            "cloud.realtime.token.issued",
            json!({ "project_id": project_id, "expires_at": expires_at, "channel_count": channels.len(), "permissions": permissions }),
        );
        Ok(json!({ "token": signed, "expires_at": expires_at }))
    }

    pub fn cloud_deploy_function(
        &mut self,
        token: &str,
        project_id: &str,
        name: &str,
        runtime: &str,
        source: &[u8],
    ) -> Result<crate::cloud::FunctionVersion> {
        self.cloud_owned_project(token, project_id)?;
        let static_findings = match runtime {
            "javascript" => crate::cloud::scan_javascript_function(source)?,
            "wasm" => vec!["WASM requires manual security review".into()],
            _ => Vec::new(),
        };
        let source_text = std::str::from_utf8(source).unwrap_or("");
        let judges: Vec<&dyn crate::verifier::Verifier> =
            [self.verifier.as_deref(), self.verifier_b.as_deref()]
                .into_iter()
                .flatten()
                .collect();
        let mut opinions = Vec::new();
        let mut security_reasons = Vec::new();
        for judge in &judges {
            let judge_name = judge.name();
            match judge.judge_function_security(name, source_text) {
                Ok((ruling, reasons)) => {
                    security_reasons.extend(
                        reasons
                            .into_iter()
                            .map(|reason| format!("[{judge_name}] {reason}")),
                    );
                    opinions.push(ruling);
                }
                Err(error) => {
                    security_reasons.push(format!("[{judge_name}] unavailable: {error}"));
                    opinions.push(crate::verifier::Ruling::Inconclusive);
                }
            }
        }
        let release_ruling =
            if opinions.is_empty() || opinions.contains(&crate::verifier::Ruling::Inconclusive) {
                if opinions.is_empty() {
                    security_reasons.push("security judge is not configured".into());
                }
                crate::cloud::ReleaseRuling::NeedsReview
            } else if opinions
                .iter()
                .all(|ruling| *ruling == crate::verifier::Ruling::Conforms)
            {
                crate::cloud::ReleaseRuling::ApprovedWithConstraints
            } else if opinions
                .iter()
                .all(|ruling| *ruling == crate::verifier::Ruling::Nonconforming)
            {
                crate::cloud::ReleaseRuling::Rejected
            } else {
                security_reasons.push("security judges disagreed".into());
                crate::cloud::ReleaseRuling::NeedsReview
            };
        let security_judge = if judges.is_empty() {
            "none".into()
        } else {
            judges
                .iter()
                .map(|judge| judge.name())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut store = crate::cloud::ProjectStore::open(&self.cloud_root, project_id)?;
        let mut version = store.deploy_function(name, runtime, source, now_unix())?;
        store.set_function_ruling(name, version.version, release_ruling.clone())?;
        version.ruling = release_ruling;
        version.security_review = Some(crate::cloud::FunctionSecurityReview {
            judge: security_judge.clone(),
            static_findings: static_findings.clone(),
            reasons: security_reasons.clone(),
        });
        self.record(
            "cloud.function.deployed",
            json!({
                "project_id": project_id,
                "name": name,
                "version": version.version,
                "digest": version.digest,
                "ruling": version.ruling,
                "security_judge": security_judge,
                "static_findings": static_findings,
                "security_reasons": security_reasons
            }),
        );
        Ok(version)
    }

    pub fn cloud_activate_function(
        &mut self,
        token: &str,
        project_id: &str,
        name: &str,
        version: u64,
    ) -> Result<()> {
        self.cloud_owned_project(token, project_id)?;
        crate::cloud::ProjectStore::open(&self.cloud_root, project_id)?
            .activate_function(name, version)?;
        self.record(
            "cloud.function.activated",
            json!({ "project_id": project_id, "name": name, "version": version }),
        );
        Ok(())
    }

    pub fn cloud_delete_function(
        &mut self,
        token: &str,
        project_id: &str,
        name: &str,
    ) -> Result<bool> {
        self.cloud_owned_project(token, project_id)?;
        let deleted = crate::cloud::ProjectStore::open(&self.cloud_root, project_id)?
            .delete_function(name)?;
        if deleted {
            self.record(
                "cloud.function.deleted",
                json!({ "project_id": project_id, "name": name }),
            );
        }
        Ok(deleted)
    }

    pub fn cloud_delete_function_version(
        &mut self,
        token: &str,
        project_id: &str,
        name: &str,
        version: u64,
    ) -> Result<bool> {
        self.cloud_owned_project(token, project_id)?;
        let deleted = crate::cloud::ProjectStore::open(&self.cloud_root, project_id)?
            .delete_function_version(name, version)?;
        if deleted {
            self.record(
                "cloud.function.version.deleted",
                json!({ "project_id": project_id, "name": name, "version": version }),
            );
        }
        Ok(deleted)
    }

    fn cloud_prepare_invocation(
        &self,
        token: &str,
        project_id: &str,
        name: &str,
        request: Value,
    ) -> Result<(String, String, Value, u64, String)> {
        self.cloud_owned_project(token, project_id)?;
        let function = crate::cloud::ProjectStore::open(&self.cloud_root, project_id)?
            .active_function(name)?
            .ok_or_else(|| Error::Other("function has no active version".into()))?;
        if function.runtime != "javascript" {
            return Err(Error::Other("WASM execution is not enabled yet".into()));
        }
        let source = String::from_utf8(function.source)
            .map_err(|_| Error::Other("JavaScript function source is not UTF-8".into()))?;
        let url = self
            .function_sandbox_url
            .clone()
            .ok_or_else(|| Error::Other("function sandbox is not configured".into()))?;
        let sandbox_token = self
            .function_sandbox_token
            .clone()
            .ok_or_else(|| Error::Other("function sandbox token is not configured".into()))?;
        Ok((
            url,
            sandbox_token,
            json!({ "source": source, "request": request, "project_id": project_id }),
            function.version,
            function.digest,
        ))
    }

    fn cloud_prepare_public_invocation(
        &self,
        project_id: &str,
        name: &str,
        request: Value,
    ) -> Result<(String, String, Value, u64, String)> {
        let function = crate::cloud::ProjectStore::open(&self.cloud_root, project_id)?
            .active_function(name)?
            .ok_or_else(|| Error::Other("function has no active version".into()))?;
        if function.runtime != "javascript" {
            return Err(Error::Other("WASM execution is not enabled yet".into()));
        }
        let source = String::from_utf8(function.source)
            .map_err(|_| Error::Other("JavaScript function source is not UTF-8".into()))?;
        let url = self
            .function_sandbox_url
            .clone()
            .ok_or_else(|| Error::Other("function sandbox is not configured".into()))?;
        let sandbox_token = self
            .function_sandbox_token
            .clone()
            .ok_or_else(|| Error::Other("function sandbox token is not configured".into()))?;
        Ok((
            url,
            sandbox_token,
            json!({ "source": source, "request": request, "project_id": project_id }),
            function.version,
            function.digest,
        ))
    }

    fn cloud_issue_function_token(
        &self,
        owner_token: &str,
        project_id: &str,
        name: &str,
    ) -> Result<Value> {
        self.cloud_owned_project(owner_token, project_id)?;
        let store = crate::cloud::ProjectStore::open(&self.cloud_root, project_id)?;
        if store.function_http_policy(name)?.auth != "token" {
            return Err(Error::Other("function HTTP auth mode is not token".into()));
        }
        let secret = self
            .realtime_secret
            .as_deref()
            .ok_or_else(|| Error::Other("function token signing is not configured".into()))?;
        let expires_at = now_unix().saturating_add(3600);
        let claims = json!({ "project_id": project_id, "function": name, "exp": expires_at });
        use base64::Engine;
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).map_err(|e| Error::Other(e.to_string()))?);
        use hmac::{Hmac, Mac};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
            .map_err(|_| Error::Other("invalid signing secret".into()))?;
        mac.update(encoded.as_bytes());
        Ok(
            json!({ "token": format!("{encoded}.{}", hex::encode(mac.finalize().into_bytes())), "expires_at": expires_at }),
        )
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
            if let Some(c) = self.contracts.get(cid) {
                return c.client.to_string() == did || c.provider.to_string() == did;
            }
            // Not resident is not the same as unknown. Failing closed
            // here is right for a contract nobody has ever heard of and
            // wrong for one that merely settled last week: the agent
            // would quietly stop seeing its own older events. The
            // parties are columns, so this costs a point read and no
            // JSON parsing.
            return match self.storage.get_contract(cid) {
                Ok(Some(rec)) => rec.client == did || rec.provider == did,
                _ => false,
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

    /// The second judge, when one is configured (RFC-0015).
    pub fn verifier_b_name(&self) -> Option<String> {
        self.verifier_b.as_ref().map(|v| v.name())
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

        // What the judge will actually read.
        //
        // The client may pass the bytes it received - that is the
        // strongest evidence, because it proves what *arrived*. When it
        // does not, fall back to the artifact the node holds. Without
        // this fallback a delivery whose artifact went through the node
        // was judged with no content at all, and the only honest answer
        // to "does this meet the criteria" with nothing to read is
        // `inconclusive` - which releases no funds and strands a
        // perfectly good delivery.
        let held = self.deliverable_of(contract_id);
        let (content, content_origin): (Option<String>, &str) = match content {
            Some(c) => (Some(c.to_string()), "client"),
            None => match held.as_ref().filter(|d| !d.content.is_empty()) {
                Some(d) => {
                    // Text goes to the judge as text; binary cannot be
                    // read by one, so it is described rather than
                    // pasted - a judge shown a megabyte of base64 will
                    // hallucinate an opinion about it.
                    let decoded = judge_readable_text(d);
                    if let Some(text) = decoded {
                        (Some(text), "node")
                    } else if d.encoding == "base64" {
                        let looks_like_image = d.media_type.starts_with("image/");
                        (
                            Some(format!(
                                "[the provider delivered the artifact inline to the node: {} \
base64 characters, media type {}, digest {}. The node recomputed the digest from the bytes it \
received and it MATCHES the commitment, so the deliverable was supplied in full{}]",
                                d.content.len(),
                                if d.media_type.is_empty() {
                                    "unspecified"
                                } else {
                                    &d.media_type
                                },
                                d.digest,
                                if looks_like_image {
                                    ". The image itself is attached to this message; judge it \
directly rather than reasoning about its metadata"
                                } else {
                                    ""
                                }
                            )),
                            "node-binary",
                        )
                    } else {
                        (Some(d.content.clone()), "node")
                    }
                }
                None => (None, "none"),
            },
        };
        let content = content.as_deref();

        // A contract that negotiated confidentiality, or carries a
        // compliance context, must never have its content leave the
        // node — enforced here, not left to the judge.
        let confidential = contract.terms.confidentiality.is_some();
        let evidence = crate::verifier::Evidence {
            contract_id: contract_id.to_string(),
            capability_id: contract.capability_id.clone(),
            acceptance_criteria: contract.terms.acceptance_criteria.clone(),
            // What was ordered. A criterion like "matches the source
            // image" is unanswerable without it.
            brief: brief_of(&contract.terms),
            deadline: contract.terms.deadline,
            delivered_at: now_unix(),
            declared_hash: contract.deliverable_hash.clone().unwrap_or_default(),
            // Where the digest comes from matters. Bytes supplied by the
            // client are hashed here, because that is the whole point -
            // it proves what arrived. Content the node holds was already
            // hashed and matched at delivery, so its stored digest is
            // the answer; hashing the human-readable stand-in written
            // for a binary artifact would compute the digest of a
            // sentence and fail integrity on a delivery that is sound.
            computed_hash: match content_origin {
                "client" => content.map(|c| format!("sha256:{}", crate::sha256_hex(c.as_bytes()))),
                "node" | "node-binary" => held.as_ref().map(|d| d.digest.clone()),
                _ => None,
            },
            deliverable_excerpt: if confidential {
                None
            } else {
                content.map(|c| c.to_string())
            },
            // An image goes to the judge as an image. Describing it and
            // asking whether it matches the prompt can only ever produce
            // `inconclusive` - which releases nothing, so an image
            // marketplace where images cannot be judged simply does not
            // settle. Confidentiality still wins: nothing leaves the
            // node for a contract that negotiated it.
            image_base64: if confidential {
                None
            } else {
                held.as_ref()
                    .filter(|d| d.encoding == "base64" && d.media_type.starts_with("image/"))
                    .map(|d| d.content.clone())
            },
            image_media_type: held.as_ref().map(|d| d.media_type.clone()),
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
            self.save_state("escalations", contract_id, &e);
        }
        self.verdicts
            .insert(contract_id.to_string(), verdict.clone());
        self.save_state("verdicts", contract_id, &verdict);
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
        deliverable_uri: Option<&str>,
        artifact: Option<crate::artifact::Artifact>,
    ) -> Result<Value> {
        let provider_did = self.agent_by_token(provider_token)?.identity.did().clone();
        // Same commitment check as a first delivery: a rework that does
        // not hash to what it claims is not a rework.
        let stored = Self::accept_artifact(artifact, deliverable_hash)?;
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
        if let Some(uri) = deliverable_uri.map(str::trim).filter(|u| !u.is_empty()) {
            contract.deliverable_uri = Some(uri.to_string());
        }
        if contract.state == ContractState::Disputed {
            contract.transition(ContractState::Delivered)?;
        }
        let saved = contract.clone();
        let remaining = Self::MAX_REMEDIES - saved.remedies_used;
        self.persist_contract(&saved);
        // Replace what the node holds: the old artifact is the one that
        // was just judged non-conforming.
        self.persist_deliverable(contract_id, deliverable_hash, stored, deliverable_uri);
        // The stale verdict must go: it judged the previous artifact.
        self.verdicts.remove(contract_id);
        self.forget_state("verdicts", contract_id);
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
        // One contract lookup, here, at settlement - not one per job on
        // every page load. Both facts come from the same read.
        let contract = self.contracts.get(contract_id);
        let remedied = contract.map(|c| c.remedies_used > 0).unwrap_or(false);
        let price = contract.map(|c| {
            (
                crate::amount::Amount::from_f64_rounding(c.terms.price.amount).to_string_decimal(),
                c.terms.price.currency.clone(),
            )
        });
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
            // The spine POSITION of this settlement, not how many
            // events happen to exist. `/v1/activity` pages on this
            // number and the browser resumes its stream from it, so a
            // count that no longer matches the sequence hides every
            // settlement made after it: the cursor sits above them
            // forever. That is exactly what happened after the spine was
            // renumbered - the node kept settling and the feed showed
            // nothing new.
            seq: self.storage.head_seq().unwrap_or(0),
            amount: price.as_ref().map(|(a, _)| a.clone()),
            currency: price.as_ref().map(|(_, c)| c.clone()),
        };
        let job_ref = record.job_ref.clone();
        // One row per job, not one row per agent.
        //
        // Storing an agent's whole history under its DID meant rewriting
        // every job it had ever done each time it did another: the value
        // grew past the URL limit and the projection stopped persisting
        // on 2026-08-14, and even once the write moved to the request
        // body it was an 8 MB serialise on the settlement path, inside
        // the global lock. Both are the same mistake - a write whose
        // cost grows with history - and both go away when the key is the
        // job rather than the agent.
        self.save_state("jobs", &job_key(&agent.to_string(), &job_ref), &record);
        self.job_stats.add(&record);
        self.recent_jobs.push_back(record.clone());
        while self.recent_jobs.len() > RECENT_JOBS {
            self.recent_jobs.pop_front();
        }
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

    /// Close the deals that time has already ended.
    ///
    /// A contract carries a deadline, and past it no delivery can be on
    /// time and no acceptance is coming. Nothing in the protocol said
    /// so: a contract nobody funded or nobody answered stayed `draft` or
    /// `signed` for ever, any parked escrow stayed parked, and the
    /// public feed showed as in-flight a deal that had been abandoned
    /// days earlier.
    ///
    /// This CLOSES them, it does not erase them: `ctr.cancel` on the
    /// spine, escrow refunded, exactly what a party calling cancel would
    /// produce. Deleting a contract from an audit chain would remove the
    /// one property this node sells, and the chain would no longer
    /// verify. An abandoned deal is a fact; the honest way to stop
    /// showing it as live is to record that it ended, not to pretend it
    /// never happened.
    ///
    /// Two rules, and a contract needs only one of them:
    ///   * its own deadline passed more than `grace` ago — the terms
    ///     both parties signed say the deal is over;
    ///   * nothing has happened to it for longer than `max_idle` — for
    ///     a deal whose deadline is years out, or is not worth the name,
    ///     silence is the only signal there is.
    ///
    /// "Nothing has happened" is read off the SPINE, not off
    /// `created_at`. Age-since-creation would expire a contract raised
    /// six hours ago and signed a minute ago, which is a live deal being
    /// killed mid-negotiation. The spine knows when each contract last
    /// moved, and that is the question being asked.
    ///
    /// A `Delivered` contract is never cancelled. The work exists, so
    /// the timeout resolves the other way: with `auto_accept_delivered`
    /// the node accepts on the silent buyer's behalf and the provider is
    /// paid. Cancelling instead would hand every buyer the same trick -
    /// take delivery, wait out the clock, keep the work and the money.
    pub fn expire_stale_contracts(
        &mut self,
        admin: &str,
        now: u64,
        grace: u64,
        max_idle: u64,
        auto_accept_delivered: bool,
        dry_run: bool,
    ) -> Result<Value> {
        match self.admin_token.as_deref() {
            Some(t) if t == admin => {}
            _ => return Err(Error::Unauthorized("operator token required".into())),
        }
        Ok(self.sweep_expired(now, grace, max_idle, auto_accept_delivered, dry_run))
    }

    /// The sweep itself, without the token check, so the node's own
    /// reaper thread in `main` can call it.
    ///
    /// Public because that thread lives in the binary, not the library.
    /// It is NOT reachable from the network: the only route into it is
    /// `expire_stale_contracts` above, which checks the operator token
    /// first. Anything else calling this is already inside the process
    /// holding the state lock, and has no boundary left to cross.
    pub fn sweep_expired(
        &mut self,
        now: u64,
        grace: u64,
        max_idle: u64,
        auto_accept_delivered: bool,
        dry_run: bool,
    ) -> Value {
        // When each contract last moved, off the TAIL of the audit chain.
        //
        // Bounded on purpose, and the bound is not a shortcut: a contract
        // with no event inside the window has not moved recently, which
        // is exactly what the idle rule is asking. It falls back to
        // `created_at`, and an old creation date expires it, which is the
        // right answer. Reading the whole spine here was fine at a few
        // hundred events and is a full scan under the state lock at a
        // hundred thousand a day - this runs every fifteen minutes.
        // Aligned with the in-memory spine window, so this reads from
        // the mirror instead of pulling fifty thousand rows out of
        // ClickHouse every fifteen minutes while holding the lock. The
        // bound stays correct for the same reason as before: a contract
        // with no event in the window has not moved recently, falls
        // back to `created_at`, and an old creation date expires it.
        const LAST_MOVE_WINDOW: u64 = crate::storage::clickhouse::SPINE_WINDOW as u64;
        let head = self.storage.head_seq().unwrap_or(0);
        let from = head.saturating_sub(LAST_MOVE_WINDOW);
        let mut last_move: HashMap<String, u64> = HashMap::new();
        for e in self
            .storage
            .events_after(from, LAST_MOVE_WINDOW)
            .unwrap_or_default()
        {
            if let Some(cid) = e.payload["contract_id"].as_str() {
                let slot = last_move.entry(cid.to_string()).or_insert(0);
                *slot = (*slot).max(e.at);
            }
        }

        // Two outcomes, and which one a contract gets turns on whether
        // the work was ever handed over.
        let mut to_cancel: Vec<(String, &'static str)> = Vec::new();
        let mut to_pay: Vec<String> = Vec::new();
        let mut stranded = 0usize;
        let mut refused: Vec<Value> = Vec::new();
        for (id, c) in self.contracts.live() {
            let delivered = match c.state {
                ContractState::Draft | ContractState::Signed | ContractState::Executing => false,
                ContractState::Delivered => true,
                _ => continue,
            };
            let past_deadline =
                c.terms.deadline > 0 && now > c.terms.deadline.saturating_add(grace);
            // Fall back to creation only when the spine has nothing for
            // this contract; a contract with neither is left alone
            // rather than expired on no evidence at all.
            let seen = last_move
                .get(id)
                .copied()
                .filter(|t| *t > 0)
                .unwrap_or(c.created_at);
            let idle = seen > 0 && now > seen.saturating_add(max_idle);
            if !past_deadline && !idle {
                continue;
            }
            if delivered {
                if auto_accept_delivered {
                    to_pay.push(id.clone());
                } else {
                    stranded += 1;
                }
            } else if past_deadline {
                to_cancel.push((id.clone(), "deadline passed"));
            } else {
                to_cancel.push((id.clone(), "abandoned before completion"));
            }
        }

        let mut closed = Vec::new();
        for (id, why) in to_cancel {
            let state = self.contracts.get(&id).map(|c| c.state.wire_name());
            if dry_run {
                closed.push(json!({
                    "deal_ref": pseudonym(&id), "was": state,
                    "outcome": "cancelled", "reason": why,
                }));
                continue;
            }
            match self.expire_one(&id, why) {
                Ok(refunded) => closed.push(json!({
                    "deal_ref": pseudonym(&id), "was": state,
                    "outcome": "cancelled", "reason": why,
                    "escrow_refunded": refunded,
                })),
                // A contract that refuses the transition is left alone
                // and named, not retried into a different state.
                Err(e) => refused.push(json!({
                    "deal_ref": pseudonym(&id), "was": state,
                    "error": e.to_string(),
                })),
            }
        }

        let mut paid = Vec::new();
        for id in to_pay {
            if dry_run {
                paid.push(json!({
                    "deal_ref": pseudonym(&id), "was": "delivered",
                    "outcome": "auto-accepted", "reason": "buyer silent past the window",
                }));
                continue;
            }
            match self.auto_accept_one(&id) {
                Ok(settlement) => paid.push(json!({
                    "deal_ref": pseudonym(&id), "was": "delivered",
                    "outcome": "auto-accepted", "reason": "buyer silent past the window",
                    "settlement": settlement,
                })),
                // A delivery whose escrow record is gone cannot be
                // settled in either direction - there is no money left
                // to move to anyone. Those are closed rather than
                // reported for ever; every other failure is named and
                // left untouched.
                //
                // Say WHY, always. A sweep that reports "8 left alone"
                // and swallows the reason is how a broken payout path
                // hides in plain sight: the count reads as a policy
                // choice instead of eight failures.
                Err(Error::EscrowViolation(_)) if !self.escrows.contains_key(&id) => {
                    match self.close_unsettleable(&id) {
                        Ok(()) => closed.push(json!({
                            "deal_ref": pseudonym(&id), "was": "delivered",
                            "outcome": "cancelled",
                            "reason": "no escrow record survives; nothing to settle either way",
                        })),
                        Err(e) => refused.push(json!({
                            "deal_ref": pseudonym(&id), "was": "delivered",
                            "error": e.to_string(),
                        })),
                    }
                }
                Err(e) => refused.push(json!({
                    "deal_ref": pseudonym(&id), "was": "delivered",
                    "error": e.to_string(),
                })),
            }
        }

        json!({
            "dry_run": dry_run,
            "cancelled": closed.len(),
            "auto_accepted": paid.len(),
            "left_alone": stranded + refused.len(),
            "contracts": closed,
            "settled": paid,
            "refused": refused,
        })
    }

    /// Close a delivered contract whose escrow record no longer exists.
    ///
    /// The ONLY case where a delivery is cancelled, and it is not a
    /// judgement about who deserved the money: there is no money. These
    /// are records from before escrow state was persisted, so the hold
    /// they refer to did not survive a restart. Accepting them fails on
    /// "no escrow for contract" and refunding has nothing to refund, so
    /// leaving them open reports the same five failures for ever.
    ///
    /// It sets the state directly instead of going through
    /// `transition()`, which refuses Delivered -> Cancelled — correctly,
    /// because a party doing this would be cancelling work it received.
    /// The caller has already established that no escrow exists; that
    /// condition is what makes this safe, and it is checked there rather
    /// than trusted here.
    fn close_unsettleable(&mut self, contract_id: &str) -> Result<()> {
        if self.escrows.contains_key(contract_id) {
            return Err(Error::EscrowViolation(
                "this contract still has an escrow: settle it, do not close it".into(),
            ));
        }
        let contract = self
            .contracts
            .get_mut(contract_id)
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.state != ContractState::Delivered {
            return Err(Error::InvalidTransition {
                from: contract.state.wire_name().into(),
                to: "cancelled".into(),
            });
        }
        contract.state = ContractState::Cancelled;
        let saved = contract.clone();
        self.persist_contract(&saved);
        self.record(
            "ctr.cancel",
            json!({
                "contract_id": contract_id,
                "by": self.node_did().to_string(),
                "reason": "delivered, but no escrow record survives to settle",
                "expired_by_node": true,
                "after_delivery": true,
                "refunded": false,
            }),
        );
        Ok(())
    }

    /// Accept a delivery the buyer never answered, on the node's own
    /// authority, and pay the provider.
    ///
    /// The alternative — cancelling and refunding — hands any buyer a
    /// free exploit: take delivery, say nothing for a day, get the money
    /// back and keep the work. Silence has to favour the side that
    /// actually did something.
    ///
    /// It re-enters `accept_delivery` with the buyer's own custodied
    /// token rather than reimplementing settlement. That function owns
    /// three payment paths, signed envelopes, receipts, job records and
    /// reputation; a second copy of it would not fail loudly when it
    /// drifted — it would pay the wrong party, or pay twice. A buyer
    /// this node does not custody has no token to borrow, so the
    /// contract is reported and left alone rather than force-settled.
    ///
    /// `exe.accept.auto` is written only once the acceptance has
    /// actually landed. It used to be written first, on the argument
    /// that the annotation should precede the acceptance it explains —
    /// and then five deliveries whose escrow had not survived failed
    /// inside `accept_delivery`, leaving the chain asserting an
    /// auto-acceptance that never happened, immediately followed by the
    /// cancellation. An audit chain may carry an out-of-order
    /// annotation; it may not carry a false one.
    fn auto_accept_one(&mut self, contract_id: &str) -> Result<Value> {
        let client_did = self
            .contracts
            .get(contract_id)
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?
            .client
            .to_string();
        let token = self
            .agents_by_did
            .get(&client_did)
            .cloned()
            .ok_or_else(|| Error::Other("buyer is not custodied on this node".into()))?;
        let settlement = self.accept_delivery(&token, contract_id)?;
        // Accepted, or merely started down the cooling-off path? Only
        // the first is an acceptance, and only it gets recorded — the
        // window is re-entered by the next sweep, which would otherwise
        // stamp a second annotation on every pass.
        let landed = self
            .contracts
            .get(contract_id)
            .map(|c| c.state == ContractState::Accepted)
            .unwrap_or(false);
        if landed {
            self.record(
                "exe.accept.auto",
                json!({
                    "contract_id": contract_id,
                    "by": self.node_did().to_string(),
                    "reason": "delivered and unanswered past the acceptance window",
                }),
            );
        }
        Ok(settlement)
    }

    /// Cancel one contract on the node's own authority, refunding any
    /// parked escrow. Split out so the sweep above holds no borrow
    /// across the mutation.
    fn expire_one(&mut self, contract_id: &str, reason: &str) -> Result<bool> {
        let contract = self
            .contracts
            .get_mut(contract_id)
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        contract.transition(ContractState::Cancelled)?;
        let saved = contract.clone();
        self.persist_contract(&saved);

        // Parked funds go home. A deal the node closed must strand
        // money even less than one a party closed.
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
                        json!({ "reason": "contract expired" }),
                    )
                    .for_contract(contract_id)
                    .sign(&identity);
                    escrow.refund(&instruction).is_ok()
                }
                _ => false,
            }
        };
        if refunded {
            self.persist_escrow(
                contract_id,
                crate::payment::EscrowState::Refunded,
                Amount::ZERO,
                &saved.terms.price.currency,
            );
        }
        // `by` is the node, not a party: whoever reads this back must be
        // able to tell an operator sweep from a buyer changing its mind.
        self.record(
            "ctr.cancel",
            json!({
                "contract_id": contract_id,
                "by": self.node_did().to_string(),
                "reason": reason,
                "expired_by_node": true,
                "refunded": refunded,
            }),
        );
        Ok(refunded)
    }

    // ---- durable projections -------------------------------------
    //
    // Contracts, escrows, identities and announcements each had a typed
    // table; everything else lived only in RAM and vanished on restart.
    // These write through to one generic state table so that a redeploy
    // stops being a quiet amnesia event.

    /// Persist one entry of a projection. Failures are logged, never
    /// fatal: losing durability is bad, refusing to serve is worse.
    fn save_state<T: serde::Serialize>(&mut self, scope: &str, key: &str, value: &T) {
        let json = match serde_json::to_string(value) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("gap-node: cannot serialize {scope}/{key}: {e}");
                return;
            }
        };
        if let Err(e) = self.storage.upsert_state(&crate::storage::StateRecord {
            scope: scope.to_string(),
            key: key.to_string(),
            value: json,
            updated_at: now_unix(),
        }) {
            eprintln!("gap-node: cannot persist {scope}/{key}: {e}");
        }
    }

    /// Persist one agent's dispute counters after they change.
    fn save_dispute(&mut self, agent: &str) {
        if let Some(d) = self.disputes.get(agent).cloned() {
            self.save_state("disputes", agent, &d);
        }
    }

    /// Forget one entry, so state that outlived its purpose does not
    /// come back at the next restart.
    fn forget_state(&mut self, scope: &str, key: &str) {
        if let Err(e) = self.storage.delete_state(scope, key) {
            eprintln!("gap-node: cannot delete {scope}/{key}: {e}");
        }
    }

    // ---- custody & balances (RFC-0016) ---------------------------

    /// What this node declares about custody.
    pub fn custody(&self) -> &crate::custody::CustodyPolicy {
        &self.custody
    }

    /// Set the custody policy explicitly.
    ///
    /// Configuration normally comes from the environment, but that is
    /// process-global: two tests setting it concurrently corrupt each
    /// other, which is a flake that looks exactly like a real bug.
    pub fn set_custody(&mut self, policy: crate::custody::CustodyPolicy) {
        self.custody = policy;
    }

    /// An agent's balance, or a zeroed one if it has never deposited.
    pub fn balance_of(&self, did: &str) -> crate::custody::Balance {
        self.balances
            .get(did)
            .cloned()
            .unwrap_or_else(|| crate::custody::Balance {
                currency: self.custody.currency.clone(),
                ..Default::default()
            })
    }

    fn save_balance(&mut self, did: &str) {
        if let Some(b) = self.balances.get(did).cloned() {
            self.save_state("balances", did, &b);
        }
    }

    /// Credit a deposit.
    ///
    /// `reference` is the on-chain transaction that funded it, when
    /// there is one. It is recorded on the spine so a disputed credit
    /// can be traced back to something outside this node's word
    /// (RFC-0016 §8).
    /// Credit a verified deposit.
    ///
    /// The amount is **never** taken from the caller. The depositor is
    /// the party that benefits from overstating it, so the node reads
    /// the chain and decides for itself: the transaction must have
    /// succeeded, moved the settlement token, landed on this node's
    /// deposit address, and be deep enough to survive a reorg.
    ///
    /// Crediting is idempotent on the transaction hash. Replaying one
    /// transfer is the cheapest attack available and the only defence
    /// is to remember.
    pub fn deposit_from_chain(&mut self, token: &str, tx: &str) -> Result<Value> {
        if !self.custody.mode.holds_funds() {
            return Err(Error::EscrowViolation(
                "this node is non-custodial and does not hold balances; \
settle on chain instead"
                    .into(),
            ));
        }
        let policy = crate::deposit::DepositPolicy::from_env();
        if !policy.is_configured() && policy.contract().trim().is_empty() {
            return Err(Error::EscrowViolation(
                "this node cannot verify deposits: no settlement token or deposit address is \
configured, and crediting one on the depositor's word is not an option"
                    .into(),
            ));
        }
        let tx = tx.trim();
        if tx.is_empty() {
            return Err(Error::Other("tx required".into()));
        }
        // Remember first: a hash already credited must never be
        // credited again, whatever the chain says now.
        if self.credited_deposits.contains(tx) {
            return Err(Error::EscrowViolation(format!(
                "transaction {tx} has already been credited"
            )));
        }
        let chain = self
            .deposit_chain
            .as_ref()
            .ok_or_else(|| Error::EscrowViolation("no chain connection configured".into()))?;
        let receipt = chain.transaction_receipt(tx)?;
        let head = chain.block_number()?;
        let did = self.agent_by_token(token)?.identity.did().to_string();

        // The deposit contract first: it carries the agent identifier,
        // so it answers "whose money is this?" without anyone having to
        // be believed. A plain transfer only answers "how much", and is
        // accepted solely when the node runs per-agent addresses, where
        // the destination is the attribution.
        let (raw, from, confirmations) = match crate::deposit::deposit_from_receipt(&receipt, head)
        {
            Some(d) => {
                let amount = policy.accept_contract_deposit(&d, &did)?;
                (amount, d.from, d.confirmations)
            }
            None => {
                let observed =
                    crate::deposit::transfer_from_receipt(&receipt, head).ok_or_else(|| {
                        Error::EscrowViolation(
                            "that transaction carries no deposit this node can credit".into(),
                        )
                    })?;
                let amount = policy.accept(&observed)?;
                (amount, observed.from, observed.confirmations)
            }
        };
        let amount = crate::deposit::units_to_amount(raw.minor_units(), policy.decimals);
        let currency = self.custody.currency.clone();
        let entry = self.balances.entry(did.clone()).or_default();
        if entry.currency.is_empty() {
            entry.currency = currency;
        }
        entry.credit(amount);
        let balance = entry.clone();
        self.save_balance(&did);
        self.credited_deposits.insert(tx.to_string());
        self.save_state("credited", tx, &did);
        self.record(
            "pay.deposit",
            json!({
                "agent_did": did,
                "amount": amount.to_string_decimal(),
                "currency": balance.currency,
                "tx": tx,
                "from": from,
                "confirmations": confirmations,
            }),
        );
        Ok(json!({
            "credited": amount.to_string_decimal(),
            "available": balance.available.to_string_decimal(),
            "held": balance.held.to_string_decimal(),
            "withdrawing": balance.withdrawing.to_string_decimal(),
            "currency": balance.currency,
            "tx": tx,
        }))
    }

    /// Credit a balance on the operator's authority, for rails the node
    /// cannot read itself (a bank transfer, a card payment).
    ///
    /// Admin-gated, and recorded with its external reference: an
    /// operator crediting a balance out of nothing is exactly what
    /// proof of reserves is meant to expose, so it had better be
    /// traceable to something outside this node.
    pub fn credit_off_chain(
        &mut self,
        admin: &str,
        agent_did: &str,
        amount: &Amount,
        reference: &str,
    ) -> Result<Value> {
        match self.admin_token.as_deref() {
            Some(t) if t == admin => {}
            _ => return Err(Error::Unauthorized("operator token required".into())),
        }
        if reference.trim().is_empty() {
            return Err(Error::Other(
                "reference required: an off-chain credit must point at something outside this node"
                    .into(),
            ));
        }
        let currency = self.custody.currency.clone();
        let entry = self.balances.entry(agent_did.to_string()).or_default();
        if entry.currency.is_empty() {
            entry.currency = currency;
        }
        entry.credit(*amount);
        let balance = entry.clone();
        self.save_balance(agent_did);
        self.record(
            "pay.deposit",
            json!({
                "agent_did": agent_did,
                "amount": amount.to_string_decimal(),
                "currency": balance.currency,
                "rail": "operator",
                "reference": reference,
            }),
        );
        Ok(json!({
            "credited": amount.to_string_decimal(),
            "available": balance.available.to_string_decimal(),
            "currency": balance.currency,
        }))
    }

    /// Take funds out, within the declared SLA.
    /// Request a payout. Earmarks the funds; it does not send them.
    ///
    /// The previous version debited the balance, emitted `pay.withdraw`
    /// and returned a receipt quoting a settlement SLA - and then
    /// nothing sent anything. There was no consumer of that event
    /// anywhere in the tree, no relayer call and no payout. The agent's
    /// balance fell and the money stayed, which is the one failure mode
    /// a custodian must never have. It also had no custody gate, unlike
    /// both of its neighbours, so it was inert on this node only by
    /// accident: `balances` is empty in non-custodial mode, so it
    /// happened to fail with "no balance" rather than by design.
    ///
    /// Money out is now shaped like money in. `credit_off_chain`
    /// requires an operator and an external reference because the node
    /// cannot see a bank transfer; a payout is the same fact in the
    /// other direction, so it takes an operator and a reference too.
    /// Between the two steps the funds sit in `withdrawing`, counted in
    /// liabilities, visible to the agent and to proof of reserves.
    pub fn request_withdrawal(
        &mut self,
        token: &str,
        amount: &Amount,
        destination: &str,
    ) -> Result<Value> {
        if !self.custody.mode.holds_funds() {
            return Err(Error::EscrowViolation(
                "this node is non-custodial and holds no balances to withdraw from".into(),
            ));
        }
        // An unvalidated destination was accepted before, including the
        // empty string: a payout instruction pointing nowhere is how
        // money gets sent nowhere.
        let destination = destination.trim();
        if destination.is_empty() {
            return Err(Error::Other(
                "destination required: a payout must say where it is going".into(),
            ));
        }
        if !is_payout_destination(destination) {
            return Err(Error::Other(format!(
                "destination is not a payable address: expected 0x followed by 40 hex \
characters, got {destination:?}"
            )));
        }
        if amount.minor_units() == 0 {
            return Err(Error::Other("amount must be positive".into()));
        }

        let did = self.agent_by_token(token)?.identity.did().to_string();
        let entry = self
            .balances
            .get_mut(&did)
            .ok_or_else(|| Error::EscrowViolation("no balance on this node".into()))?;
        // Only `available` may be earmarked: funds held against an open
        // contract are already committed to somebody else.
        entry.start_withdrawal(*amount)?;
        let balance = entry.clone();
        self.save_balance(&did);

        let request_id = format!(
            "wd_{}",
            &crate::sha256_hex(
                format!(
                    "{did}|{destination}|{}|{}",
                    amount.to_string_decimal(),
                    now_unix()
                )
                .as_bytes()
            )[..16]
        );
        let record = json!({
            "request_id": request_id,
            "agent_did": did,
            "amount": amount.to_string_decimal(),
            "currency": balance.currency,
            "destination": destination,
            "requested_at": now_unix(),
            "state": "pending",
        });
        self.save_state("withdrawals", &request_id, &record);
        self.withdrawals.insert(request_id.clone(), record.clone());
        self.record("pay.withdraw.requested", record.clone());

        Ok(json!({
            "request_id": request_id,
            // Not "withdrawn". Nothing has moved yet, and a receipt that
            // says otherwise is the bug this replaces.
            "state": "pending",
            "amount": amount.to_string_decimal(),
            "available": balance.available.to_string_decimal(),
            "held": balance.held.to_string_decimal(),
            "withdrawing": balance.withdrawing.to_string_decimal(),
            "currency": balance.currency,
            "destination": destination,
            "settles_within_seconds": self.custody.withdrawal_sla_seconds,
            "note": "The funds have left your spendable balance and have not left the node. \
        They stay in this node's liabilities until an operator records the payout.",
        }))
    }

    /// Record that a payout actually went out.
    ///
    /// Operator-only and reference-required, exactly like the credit
    /// rail: the node cannot watch a bank wire or a manual transfer
    /// leave, so the only honest thing it can do is record who said it
    /// did and what it can be checked against.
    pub fn settle_withdrawal(
        &mut self,
        admin: &str,
        request_id: &str,
        reference: &str,
    ) -> Result<Value> {
        match self.admin_token.as_deref() {
            Some(t) if t == admin => {}
            _ => return Err(Error::Unauthorized("operator token required".into())),
        }
        if reference.trim().is_empty() {
            return Err(Error::Other(
                "reference required: a payout must point at something outside this node \
(a transaction hash, a wire reference)"
                    .into(),
            ));
        }
        let mut record = self
            .withdrawals
            .get(request_id)
            .cloned()
            .ok_or_else(|| Error::Other(format!("unknown withdrawal request: {request_id}")))?;
        if record["state"] != json!("pending") {
            return Err(Error::EscrowViolation(format!(
                "withdrawal {request_id} is already {}",
                record["state"].as_str().unwrap_or("resolved")
            )));
        }
        let did = record["agent_did"].as_str().unwrap_or("").to_string();
        let amount = Amount::parse(record["amount"].as_str().unwrap_or("0"))?;

        let entry = self
            .balances
            .get_mut(&did)
            .ok_or_else(|| Error::EscrowViolation("no balance on this node".into()))?;
        entry.finish_withdrawal(amount)?;
        self.save_balance(&did);

        record["state"] = json!("settled");
        record["reference"] = json!(reference);
        record["settled_at"] = json!(now_unix());
        self.save_state("withdrawals", request_id, &record);
        self.withdrawals.insert(request_id.into(), record.clone());
        self.record("pay.withdraw.settled", record.clone());
        Ok(record)
    }

    /// Give up on a payout and return the funds.
    ///
    /// Without this a bad destination stranded the money in
    /// `withdrawing` forever, which is the same defect as losing it,
    /// only slower.
    pub fn cancel_withdrawal(
        &mut self,
        admin: &str,
        request_id: &str,
        reason: &str,
    ) -> Result<Value> {
        match self.admin_token.as_deref() {
            Some(t) if t == admin => {}
            _ => return Err(Error::Unauthorized("operator token required".into())),
        }
        let mut record = self
            .withdrawals
            .get(request_id)
            .cloned()
            .ok_or_else(|| Error::Other(format!("unknown withdrawal request: {request_id}")))?;
        if record["state"] != json!("pending") {
            return Err(Error::EscrowViolation(format!(
                "withdrawal {request_id} is already {}",
                record["state"].as_str().unwrap_or("resolved")
            )));
        }
        let did = record["agent_did"].as_str().unwrap_or("").to_string();
        let amount = Amount::parse(record["amount"].as_str().unwrap_or("0"))?;

        let entry = self
            .balances
            .get_mut(&did)
            .ok_or_else(|| Error::EscrowViolation("no balance on this node".into()))?;
        entry.cancel_withdrawal(amount)?;
        self.save_balance(&did);

        record["state"] = json!("cancelled");
        record["reason"] = json!(reason);
        record["cancelled_at"] = json!(now_unix());
        self.save_state("withdrawals", request_id, &record);
        self.withdrawals.insert(request_id.into(), record.clone());
        self.record("pay.withdraw.cancelled", record.clone());
        Ok(record)
    }

    /// Payout requests an operator still owes somebody.
    pub fn pending_withdrawals(&self) -> Value {
        let mut pending: Vec<Value> = self
            .withdrawals
            .values()
            .filter(|w| w["state"] == json!("pending"))
            .cloned()
            .collect();
        pending.sort_by_key(|w| w["requested_at"].as_u64().unwrap_or(0));
        json!({ "count": pending.len(), "withdrawals": pending })
    }

    /// Total owed to agents: the number a reserves attestation has to
    /// cover, and the one anyone can recompute from the spine.
    pub fn liabilities(&self) -> Amount {
        Amount::from_minor(
            self.balances
                .values()
                .map(|b| b.total().minor_units())
                .sum(),
        )
    }

    /// A signed statement that holdings cover liabilities (RFC-0016 §6).
    ///
    /// `holdings` comes from configuration, because the node cannot see
    /// the operator's wallet. That is exactly why the attestation is
    /// pinned to a spine sequence: the liability side is verifiable by
    /// replay, so only the holdings side rests on the operator's word.
    pub fn reserves(&self) -> Value {
        let liabilities = self.liabilities();
        let holdings = std::env::var("GAP_RESERVE_HOLDINGS")
            .ok()
            .and_then(|h| Amount::parse(&h).ok())
            .unwrap_or(liabilities);
        let accounts: Vec<String> = std::env::var("GAP_RESERVE_ACCOUNTS")
            .unwrap_or_default()
            .split(',')
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect();
        let mut attestation = crate::custody::ReserveAttestation {
            at: now_unix(),
            // A reserve attestation says "replay the chain up to HERE
            // and recompute this". That has to be a real sequence.
            spine_seq: self.storage.head_seq().unwrap_or(0),
            liabilities: liabilities.to_string_decimal(),
            holdings: holdings.to_string_decimal(),
            currency: self.custody.currency.clone(),
            accounts,
            signature: None,
        };
        attestation.sign(&self.node.identity);
        let solvent = attestation.is_solvent();
        let mut out = serde_json::to_value(&attestation).unwrap_or_default();
        out["solvent"] = json!(solvent);
        out["custody"] = serde_json::to_value(&self.custody).unwrap_or_default();
        out["note"] = json!(
            "Liabilities are recomputable: replay the audit spine to spine_seq and fold the \
balance events. Holdings are the operator's declaration. This is proof of reserves, not proof \
of solvency."
        );
        out
    }

    /// The deposit address derived for one agent (RFC-0016 §5.3).
    ///
    /// Derived from the node seed, never stored: the address holds
    /// other people's money until it is swept, and a lost database must
    /// not mean lost funds.
    pub fn deposit_address_for(&self, agent_did: &str) -> Result<String> {
        let key = crate::deposit::deposit_key_for(&self.node.identity.seed_bytes(), agent_did);
        let evm = crate::relayer::EvmKey::from_bytes(&key)?;
        Ok(format!("0x{}", hex::encode(evm.address())))
    }

    /// Links a buyer with no crypto can use to fund an agent's balance.
    ///
    /// Every configured provider is returned rather than one chosen for
    /// the caller: coverage, fees and payment methods differ by country,
    /// and the buyer knows their country. Returning both is also what
    /// keeps either from becoming load-bearing.
    pub fn onramp_links(
        &self,
        token: &str,
        fiat_currency: &str,
        amount: Option<&str>,
    ) -> Result<Value> {
        if !self.custody.mode.holds_funds() {
            return Err(Error::EscrowViolation(
                "this node is non-custodial and holds no balances to fund".into(),
            ));
        }
        let did = self.agent_by_token(token)?.identity.did().to_string();
        let address = self.deposit_address_for(&did)?;
        let config = crate::onramp::OnrampConfig::from_env();
        let available = config.available();
        if available.is_empty() {
            return Err(Error::Other(
                "no on-ramp is configured on this node: fund the balance on chain, or ask the \
operator"
                    .into(),
            ));
        }
        let req = crate::onramp::OnrampRequest {
            deposit_address: address.clone(),
            fiat_currency: fiat_currency.to_uppercase(),
            fiat_amount: amount.map(|a| a.to_string()),
            reference: did.clone(),
        };
        let mut links = Vec::new();
        for provider in available {
            match crate::onramp::build_url(provider, &config, &req) {
                Ok(url) => links.push(json!({ "provider": provider.as_str(), "url": url })),
                // One misconfigured provider must not deny the other.
                Err(e) => eprintln!("gap-node: {} link unavailable: {e}", provider.as_str()),
            }
        }
        Ok(json!({
            "agent_did": did,
            "deposit_address": address,
            "currency": self.custody.currency,
            "links": links,
            "note": "Pay through one of these and the stablecoin lands on your deposit address; \
        the node credits it once it has enough confirmations. Nothing to send yourself.",
        }))
    }

    /// Is this contract's payment actually secured?
    ///
    /// One predicate, used by both `exe.start` and `exe.deliver`, so the
    /// two can never drift into disagreeing about whether it is safe to
    /// work. An on-chain relayer counts as funded: settlement lives in
    /// the contract rather than in this node's escrow map.
    pub fn escrow_is_funded(&self, contract_id: &str) -> bool {
        self.escrows.contains_key(contract_id)
            || self.balance_holds.contains_key(contract_id)
            || self.relayer.is_some()
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
        let funded = self.escrow_is_funded(contract_id);
        let contract = self
            .contracts
            .get_mut(contract_id)
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.provider != did {
            return Err(Error::Unauthorized("only the provider may start".into()));
        }
        // Refuse to start unfunded work.
        //
        // Escrow used to be checked only at delivery, which meant a
        // provider could accept, spend real compute producing the
        // deliverable, and discover at the last step that the client
        // had never parked the money. The cost had already been paid by
        // then and there was no way to recover it. The guard belongs
        // where the spending starts, not where it ends.
        if contract.escrow && !funded {
            return Err(Error::EscrowViolation(
                "escrow is not parked: do not start work yet. Wait for the escrow.parked \
event (or poll GET /v1/contract/{id} until escrow_funded is true)"
                    .into(),
            ));
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
        // The row too, or the next restart puts it straight back. This
        // was not theoretical: an agent delisted by hand reappeared on
        // the directory page an hour later, when a deploy restarted the
        // node.
        let r = self.storage.delete_announcement(&did.to_string());
        self.note_persist(r, "announcement removal", &did.to_string());
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
        self.bindings.insert(agent.clone(), binding.clone());
        self.save_state("bindings", &agent, &binding);
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
        self.forget_state("vetoes", &agent);
        self.forget_state("bindings", &agent);
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
        if let Some(list) = self.vetoes.get(&agent).cloned() {
            self.save_state("vetoes", &agent, &list);
        }
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
        self.budgets.insert(agent.clone(), grant.clone());
        self.save_state("budgets", &agent, &grant);
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
        self.spend_today.insert((key.clone(), day), after);
        // Without this a restart hands out a fresh daily allowance.
        self.save_state("spend", &format!("{key}|{day}"), &after.to_string_decimal());
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
        self.subscriptions.insert(id.clone(), sub.clone());
        self.save_state("subscriptions", &id, &sub);
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
                self.forget_state("subscriptions", id);
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
                    // Searching for an agent by the name it goes by is
                    // the first thing anyone tries.
                    || a.name.to_lowercase().contains(&needle)
                    || a.description.to_lowercase().contains(&needle)
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
                // Announced is not proven.
                //
                // Every other field here is the agent's own claim about
                // itself. This one is the node's: how many contracts
                // each capability has actually settled, from the job
                // history. A buyer can then tell a capability with three
                // hundred deliveries behind it from one announced this
                // morning - which is the only thing in a directory that
                // a newcomer cannot simply assert.
                let capabilities: Vec<Value> = a
                    .capabilities
                    .iter()
                    .map(|c| {
                        let mut v = serde_json::to_value(c).unwrap_or(json!({}));
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert(
                                "settled".into(),
                                json!(self
                                    .job_stats
                                    .by_capability
                                    .get(&c.id)
                                    .copied()
                                    .unwrap_or(0)),
                            );
                        }
                        v
                    })
                    .collect();
                json!({
                    "did": did,
                    "name": a.name,
                    "description": a.description,
                    "capabilities": capabilities,
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

        // ONE BID PER TREE (RFC-0007 section 3).
        //
        // The point of the whole RFC: a principal that spawns sub-agents
        // must not get more shelf space than one that does not.
        // Announcing from ten delegates of the same root used to produce
        // ten directory entries, which is a Sybil attack costing nothing
        // and needing no crypto.
        //
        // The survivor is the best-scored of the tree, ties broken by
        // DID so the answer is stable across calls - a directory that
        // reshuffles on every request is one nobody can audit.
        let mut best: std::collections::BTreeMap<String, Value> = Default::default();
        for a in agents {
            let did = a["did"].as_str().unwrap_or("").to_string();
            let tree = self.tree_of(&did);
            // The root represents its own tree whenever it is announcing.
            // Anything else picks a delegate essentially at random and
            // shows a buyer a sub-agent while the principal it answers
            // to sits invisible one line away.
            //
            // Otherwise: best score, ties broken by DID, so the listing
            // is stable across calls. A directory that reshuffles on
            // every request is one nobody can audit.
            let rank = |v: &Value, d: &str| {
                (
                    u8::from(d == tree),
                    v["score"].as_f64().unwrap_or(0.0),
                    std::cmp::Reverse(d.to_string()),
                )
            };
            match best.get(&tree) {
                Some(kept) => {
                    if rank(&a, &did) > rank(kept, kept["did"].as_str().unwrap_or("")) {
                        best.insert(tree, a);
                    }
                }
                None => {
                    best.insert(tree, a);
                }
            }
        }
        let agents: Vec<Value> = best
            .into_values()
            .map(|mut a| {
                let did = a["did"].as_str().unwrap_or("").to_string();
                let tree = self.tree_of(&did);
                // Said out loud: a buyer comparing two entries deserves
                // to know when one of them speaks for a whole tree.
                if tree != did {
                    a["tree_root"] = json!(tree);
                }
                a
            })
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

    /// How one spine event kind appears on the public feed: the phase
    /// of the deal it belongs to, and the words a reader sees.
    ///
    /// This is an ALLOWLIST, and deliberately so. Spine payloads carry
    /// fields a stranger wrote — `reason`, `note`, `plan`, `step` — and
    /// a redaction pass that strips the ones we thought of today will
    /// publish the next one somebody adds tomorrow. Nothing reaches the
    /// feed unless it is named here, and no payload field is ever
    /// copied through: `public_lifecycle_after` rebuilds every row from
    /// the contract, whose price and capability are already public once
    /// the deal settles.
    ///
    /// The phase names group events by colour on `/activity`; they are
    /// part of the public API, so they are stable strings rather than
    /// the internal kind.
    fn public_phase(kind: &str) -> Option<(&'static str, &'static str)> {
        Some(match kind {
            // Negotiation.
            "ctr.propose" => ("negotiation", "contract proposed"),
            "ctr.counter" => ("negotiation", "terms countered"),
            "ctr.accept" => ("negotiation", "terms accepted"),
            "ctr.signed" => ("negotiation", "contract signed"),
            "ctr.reject" => ("closed", "proposal rejected"),
            "ctr.cancel" => ("closed", "contract cancelled"),
            // Money in.
            "pay.park" | "pay.parked" => ("escrow", "escrow funded"),
            "pay.parked.onchain" => ("escrow", "escrow funded on-chain"),
            "pay.cooling_off.pending" => ("escrow", "cooling-off started"),
            "pay.cooling_off.elapsed" => ("escrow", "cooling-off elapsed"),
            "pay.cooling_off.withdrawn" => ("closed", "consent withdrawn"),
            // Work.
            "exe.start" => ("execution", "work started"),
            "exe.progress" => ("execution", "progress reported"),
            "exe.deliver" | "exe.delivered" => ("execution", "deliverable handed over"),
            // Judgement.
            "exe.verify" => ("verdict", "deliverable verified"),
            "exe.accept" => ("verdict", "accepted by the client"),
            "exe.accept.auto" => ("verdict", "auto-accepted, buyer silent"),
            "exe.reject" => ("verdict", "rejected by the client"),
            "ctr.remedy" => ("verdict", "rework granted"),
            "ctr.dispute" | "pay.dispute" | "pay.disputed" => ("verdict", "disputed"),
            "ctr.ruling" | "pay.ruled" => ("verdict", "ruling issued"),
            // Money out.
            "pay.released" => ("settled", "payment released"),
            "pay.released.onchain" => ("settled", "payment released on-chain"),
            "pay.refund" | "pay.refunded" => ("settled", "payment refunded"),
            // The market itself.
            "cap.announce" => ("market", "capability listed"),
            "cap.deregister" => ("market", "capability withdrawn"),
            _ => return None,
        })
    }

    /// The deal lifecycle in public, pseudonymous form: every step from
    /// proposal to payout, not just the settlements.
    ///
    /// Same cursor as the rest of the protocol — `after` is a spine
    /// sequence — so the browser and an agent resume identically.
    /// `scanned_to` is the last sequence LOOKED AT, not the last one
    /// published: most spine kinds are internal, so a caller that
    /// advanced its cursor only past published rows would re-read the
    /// same unpublished tail forever.
    pub fn public_lifecycle_after(&self, after: u64, limit: usize) -> Value {
        let events = self
            .storage
            .events_after(after, limit as u64)
            .unwrap_or_default();
        let scanned_to = events.last().map(|e| e.seq).unwrap_or(after);
        let rows: Vec<Value> = events
            .iter()
            .filter_map(|e| {
                let (phase, label) = Self::public_phase(&e.kind)?;
                // The contract id is the private handle that ties both
                // parties together; the pseudonym of it is the public
                // one, and it is the SAME value as `job_ref`, so a row
                // on this feed and the settlement it becomes carry one
                // reference. That is what lets the page group them.
                let contract_id = e.payload["contract_id"].as_str();
                let deal_ref = contract_id.map(pseudonym);
                let contract = contract_id.and_then(|c| self.contracts.get(c));
                // An auto-acceptance that never landed. 28 of these were
                // written before the annotation was moved to after the
                // acceptance it describes, and they cannot be removed —
                // an audit chain does not get edited when it embarrasses
                // its author. What CAN be fixed is the reading: a deal
                // that ended cancelled was never auto-accepted, and
                // saying so is more truthful than repeating the claim.
                let (phase, label) = match (e.kind.as_str(), contract.map(|c| c.state)) {
                    ("exe.accept.auto", Some(ContractState::Cancelled)) => {
                        ("closed", "auto-acceptance failed, nothing to settle")
                    }
                    _ => (phase, label),
                };
                Some(json!({
                    "seq": e.seq,
                    "at": e.at,
                    "kind": e.kind,
                    "phase": phase,
                    "label": label,
                    "deal_ref": deal_ref,
                    // Whether /job/<deal_ref> resolves yet. A link to a
                    // deal still in flight would 404, and a feed whose
                    // rows mostly 404 reads as broken rather than live.
                    "settled": deal_ref
                        .as_deref()
                        .map(|d| self.jobs_by_ref.contains_key(d))
                        .unwrap_or(false),
                    "capability_id": contract.map(|c| c.capability_id.clone()),
                    "amount": contract.map(|c| {
                        crate::amount::Amount::from_f64_rounding(c.terms.price.amount)
                            .to_string_decimal()
                    }),
                    "currency": contract.map(|c| c.terms.price.currency.clone()),
                }))
            })
            .collect();
        json!({ "events": rows, "count": rows.len(), "scanned_to": scanned_to })
    }

    /// The last `limit` lifecycle rows, newest first, for the initial
    /// server render of `/activity`.
    ///
    /// Reads a window off the tail of the spine rather than the whole
    /// chain: this projection only ever shows the recent past, and the
    /// chain only grows. The window is over-sized because most kinds
    /// are not published — asking for exactly `limit` events would come
    /// back nearly empty.
    pub fn public_lifecycle(&self, limit: usize) -> Value {
        let total = self.storage.event_count().unwrap_or(0);
        let window = (limit as u64).saturating_mul(8).max(200);
        let all = self.public_lifecycle_after(total.saturating_sub(window), window as usize);
        let mut rows = all["events"].as_array().cloned().unwrap_or_default();
        rows.reverse();
        rows.truncate(limit);
        json!({ "events": rows, "count": rows.len() })
    }

    /// Recent public activity: settled jobs across all agents, already
    /// pseudonymous (RFC-0014 §5), newest first.
    pub fn public_activity_after(&self, after: u64, limit: usize) -> Value {
        let mut all: Vec<Value> = self
            .jobs
            .iter()
            .flat_map(|(agent, records)| {
                records
                    .iter()
                    .filter(|r| r.seq > after)
                    .map(move |r| (agent.as_str(), r))
            })
            .map(|(agent, r)| self.public_job_row_for(agent, r))
            .collect();
        all.sort_by_key(|v| v["seq"].as_u64().unwrap_or(0));
        all.truncate(limit);
        json!({ "jobs": all, "count": all.len() })
    }

    /// One settled job in the public, pseudonymous shape the feed uses.
    fn public_job_row(&self, r: &JobRecord) -> Value {
        // The owning agent is only needed for its pseudonym, and a job
        // record already carries its counterparty; look the owner up
        // rather than threading it through every caller.
        let agent = self
            .jobs
            .iter()
            .find(|(_, records)| records.iter().any(|x| x.job_ref == r.job_ref))
            .map(|(a, _)| a.as_str())
            .unwrap_or("");
        self.public_job_row_for(agent, r)
    }

    fn public_job_row_for(&self, agent: &str, r: &JobRecord) -> Value {
        let amount = self.job_amount(&r.job_ref);
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
            // A job whose contract is gone cannot be priced or timed. A
            // dash, not a zero: a marketplace that reports zero for work
            // it cannot price is understating its own volume.
            "amount": amount.as_ref().map(|(a, _)| a.clone()),
            "currency": amount.as_ref().map(|(_, c)| c.clone()),
            "duration_seconds": self
                .jobs_by_ref
                .get(&r.job_ref)
                .and_then(|cid| self.contract_for_read(cid))
                .filter(|c| c.created_at > 0 && r.at >= c.created_at)
                .map(|c| r.at - c.created_at),
        })
    }

    /// One settled job in public, pseudonymous form: the full verdict —
    /// checks, reasons and each judge's opinion — with the contract id
    /// and both parties stripped. This is what makes a score auditable
    /// rather than merely asserted.
    /// Store a contract, and drop what its arrival pushed out.
    ///
    /// The single door into the contract set: every write goes through
    /// here so that eviction stays paired with the escrow map. Doing it
    /// at five call sites would work until someone adds a sixth.
    fn set_contract(&mut self, contract: Contract) {
        for gone in self.contracts.insert(contract) {
            // Its contract is out of the window, so nothing left in this
            // process will ask whether it is funded. The row stays in
            // storage; this is a cache, not a ledger.
            self.escrows.remove(&gone);
            // A verdict is read on the job page, which now reads the
            // contract from storage too, so the pair stays consistent:
            // both come back together or neither does. Persisted under
            // the "verdicts" scope, and the job page falls back to it.
            self.verdicts.remove(&gone);
        }
    }

    /// The verdict for a job page, resident or not.
    ///
    /// Same contract as `contract_for_read`, and it has to be: a page
    /// that showed the deal but lost the verdict the day it fell out of
    /// the cache would be worse than one that 404s, because it would
    /// look complete.
    fn verdict_for_read(&self, id: &str) -> Option<std::borrow::Cow<'_, crate::verifier::Verdict>> {
        if let Some(v) = self.verdicts.get(id) {
            return Some(std::borrow::Cow::Borrowed(v));
        }
        let rec = self.storage.get_state("verdicts", id).ok().flatten()?;
        serde_json::from_str(&rec.value)
            .ok()
            .map(std::borrow::Cow::Owned)
    }

    /// One agent's record of one job, resident or not.
    ///
    /// Looks in memory first, then asks storage by key. Scanning every
    /// job to find one by its public reference was affordable while the
    /// node held four thousand of them and is not at 621,462.
    fn job_record_for(&self, contract_id: &str, job_ref: &str) -> Option<JobRecord> {
        if let Some(r) = self
            .recent_jobs
            .iter()
            .find(|r| r.job_ref == job_ref)
            .cloned()
        {
            return Some(r);
        }
        let parties: Vec<String> = match self.contract_for_read(contract_id) {
            Some(c) => vec![c.client.to_string(), c.provider.to_string()],
            None => Vec::new(),
        };
        for did in parties {
            if let Some(r) = self
                .jobs
                .get(&did)
                .and_then(|l| l.iter().find(|r| r.job_ref == job_ref))
                .cloned()
            {
                return Some(r);
            }
            if let Ok(Some(rec)) = self.storage.get_state("jobs", &job_key(&did, job_ref)) {
                if let Ok(r) = serde_json::from_str::<JobRecord>(&rec.value) {
                    return Some(r);
                }
            }
        }
        None
    }

    /// Register (or update) a pass-through route.
    ///
    /// Refuses outright when the node has no master key. The route
    /// carries someone else's API credential, and a node that cannot
    /// seal it has no business holding it - a plaintext secret store
    /// that works is worse than a feature that does not.
    pub fn gateway_register(&mut self, token: &str, body: &Value) -> Result<Value> {
        let owner = self.agent_by_token(token)?.identity.did().to_string();
        let vault = self
            .vault
            .as_ref()
            .ok_or_else(|| Error::Other(
                "gateway needs GAP_MASTER_KEY: this node will not store an upstream credential in the clear".into(),
            ))?;
        let slug = body["slug"].as_str().unwrap_or("").to_string();
        if !crate::gateway::valid_slug(&slug) {
            return Err(Error::Other(
                "slug must be lowercase letters, digits or '-' (max 64)".into(),
            ));
        }
        let upstream = body["upstream"].as_str().unwrap_or("").to_string();
        if !upstream.starts_with("https://") {
            return Err(Error::Other("upstream must be https".into()));
        }
        if !crate::gateway::upstream_is_public(&upstream) {
            return Err(Error::Other(
                "upstream must be a public host: this node will not fetch its own network on an agent's behalf".into(),
            ));
        }
        if let Some(existing) = self.gateways.get(&slug) {
            if existing.owner != owner {
                return Err(Error::Unauthorized("slug belongs to another agent".into()));
            }
        }
        let secret = body["auth_value"].as_str().unwrap_or("");
        let route = crate::gateway::GatewayRoute {
            slug: slug.clone(),
            owner,
            upstream,
            capability_id: body["capability_id"].as_str().unwrap_or("").to_string(),
            amount: body["amount"].as_str().unwrap_or("0.010000").to_string(),
            currency: body["currency"].as_str().unwrap_or("USDC").to_string(),
            auth_header: body["auth_header"]
                .as_str()
                .unwrap_or("Authorization")
                .to_string(),
            auth_value_sealed: if secret.is_empty() {
                String::new()
            } else {
                vault.seal(secret)
            },
            acceptance_criteria: body["acceptance_criteria"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        };
        let public = route.public();
        self.save_state("gateways", &slug, &route);
        self.gateways.insert(slug.clone(), route);
        // The event carries the PUBLIC shape. An audit spine is the last
        // place a credential should end up, because it is the one store
        // designed never to forget.
        self.record("gw.registered", public.clone());
        Ok(public)
    }

    /// Every registered route, in public shape.
    pub fn gateway_list(&self) -> Value {
        let routes: Vec<Value> = self.gateways.values().map(|r| r.public()).collect();
        json!({ "routes": routes, "count": routes.len() })
    }

    /// What the gateway should do with an incoming call.
    ///
    /// Returns either a challenge to answer with, or everything needed
    /// to make the upstream call - deliberately including the secret,
    /// so the caller can DROP THE LOCK before going out to the network.
    /// An upstream service taking two seconds while this node's global
    /// mutex is held is how a slow partner becomes an outage here.
    pub fn gateway_begin(
        &mut self,
        slug: &str,
        tail: &str,
        resource: &str,
        client_token: Option<&str>,
        contract_id: Option<&str>,
    ) -> Result<crate::gateway::GatewayStep> {
        let route = self
            .gateways
            .get(slug)
            .cloned()
            .ok_or_else(|| Error::Other(format!("no gateway route '{slug}'")))?;
        let node_did = self.node_did().to_string();

        // Already paid? Verify it, then hand back the call to make.
        if let (Some(cid), Some(tok)) = (contract_id, client_token) {
            let client_did = self.agent_by_token(tok)?.identity.did().to_string();
            let c = self
                .contracts
                .get(cid)
                .ok_or_else(|| Error::UnknownContract(cid.into()))?;
            if c.client.to_string() != client_did {
                return Err(Error::Unauthorized(
                    "contract belongs to another agent".into(),
                ));
            }
            if c.capability_id != route.capability_id {
                return Err(Error::Other("contract is for another capability".into()));
            }
            if !self.escrow_is_funded(cid) {
                return Err(Error::EscrowViolation(
                    "escrow is not funded: park it before retrying".into(),
                ));
            }
            let secret = match (&self.vault, route.auth_value_sealed.is_empty()) {
                (_, true) => String::new(),
                (Some(v), false) => v.open(&route.auth_value_sealed)?,
                (None, false) => {
                    return Err(Error::Other("cannot open the upstream credential".into()))
                }
            };
            let provider_token = self
                .agents_by_did
                .get(&route.owner)
                .cloned()
                .ok_or_else(|| Error::Other("gateway owner is not node-custodied".into()))?;
            return Ok(crate::gateway::GatewayStep::Forward {
                url: crate::gateway::upstream_url(&route, tail)?,
                auth_header: route.auth_header.clone(),
                auth_value: secret,
                contract_id: cid.to_string(),
                provider_token,
                client_token: tok.to_string(),
            });
        }

        // Not paid. An anonymous caller cannot have a contract drafted
        // for it - there is no party to name - so it is told what it
        // needs first rather than handed a dead contract id.
        let Some(tok) = client_token else {
            return Ok(crate::gateway::GatewayStep::Challenge(
                route.challenge(resource, "", &node_did),
            ));
        };
        let terms = crate::contract::Terms {
            input: json!({ "resource": resource }),
            deliverable: json!({ "media_type": "application/json" }),
            acceptance_criteria: route.acceptance_criteria.clone(),
            deadline: now_unix() + 900,
            price: crate::contract::Price {
                amount: route.amount.parse::<f64>().unwrap_or(0.0),
                currency: route.currency.clone(),
                model: "fixed".into(),
                cap: None,
            },
            autonomy: "execute-notify".into(),
            confidentiality: None,
            human_review_above: None,
            cooling_off_seconds: None,
        };
        let cid = self.propose_contract(tok, &route.owner, &route.capability_id, terms, true)?;
        let provider_token = self
            .agents_by_did
            .get(&route.owner)
            .cloned()
            .ok_or_else(|| Error::Other("gateway owner is not node-custodied".into()))?;
        // The provider side is accepted for it: a gateway sells at a
        // published price and has nothing to negotiate. The buyer
        // signed by proposing, so the contract is now fully signed and
        // there is nothing left for it to accept - calling `accept`
        // would be refused. What the buyer still does explicitly is
        // FUND, and that is its moment of consent to the criteria it
        // can read in this response: nothing is forwarded upstream
        // until the escrow holds the money.
        self.accept_contract(&provider_token, &cid)?;
        Ok(crate::gateway::GatewayStep::Challenge(
            route.challenge(resource, &cid, &node_did),
        ))
    }

    /// Record what the upstream returned, and settle.
    ///
    /// Runs the ordinary contract path - start, deliver, accept - so a
    /// gateway call lands on the same public record as any other deal,
    /// with the same verdict machinery behind it.
    pub fn gateway_complete(
        &mut self,
        contract_id: &str,
        provider_token: &str,
        client_token: &str,
        media_type: &str,
        payload: &[u8],
    ) -> Result<Value> {
        let _ = self.start_execution(provider_token, contract_id, &json!({ "via": "gateway" }));
        // Text upstreams (JSON, almost always) travel as text so the
        // judge can read them. The base64 branch is what made six
        // verdicts "inconclusive" once, and a gateway response is
        // exactly the case that must not repeat it.
        let text = std::str::from_utf8(payload).ok();
        let artifact = match text {
            Some(t) => crate::artifact::Artifact {
                content: t.to_string(),
                encoding: "utf8".into(),
                media_type: media_type.to_string(),
            },
            None => crate::artifact::Artifact {
                content: crate::artifact::encode_base64(payload),
                encoding: "base64".into(),
                media_type: media_type.to_string(),
            },
        };
        let digest = artifact.digest()?;
        self.deliver(provider_token, contract_id, &digest, None, Some(artifact))?;
        self.accept_delivery(client_token, contract_id)
    }

    /// A contract for a read-only path, resident or not.
    ///
    /// The in-memory set holds what is in flight plus a window of what
    /// just finished; everything older is in storage. Public pages must
    /// not care which - this node's whole claim is that a settled deal
    /// stays inspectable for ever, and "for ever" cannot mean "until it
    /// falls out of a cache". Borrows on a hit, reads on a miss.
    fn contract_for_read(&self, id: &str) -> Option<std::borrow::Cow<'_, Contract>> {
        if let Some(c) = self.contracts.get(id) {
            return Some(std::borrow::Cow::Borrowed(c));
        }
        let rec = self.storage.get_contract(id).ok().flatten()?;
        let mut c: Contract = serde_json::from_str(&rec.contract_json).ok()?;
        if let Ok(state) = ContractState::parse(&rec.state) {
            c.state = state;
        }
        Some(std::borrow::Cow::Owned(c))
    }

    pub fn public_job(&self, job_ref: &str) -> Result<Value> {
        let contract_id = self
            .jobs_by_ref
            .get(job_ref)
            .ok_or_else(|| Error::Other("unknown job reference".into()))?;
        // Addressed, not searched. The key is <did>/<job ref>, and the
        // parties come from the contract the reference already resolves
        // to - so this is at most three point reads instead of a walk
        // over every job on the node.
        let record = self
            .job_record_for(contract_id, job_ref)
            .ok_or_else(|| Error::Other("unknown job reference".into()))?;
        let record = &record;
        let contract = self.contract_for_read(contract_id);
        let verdict = self.verdict_for_read(contract_id);
        Ok(json!({
            "job_ref": job_ref,
            "capability_id": record.capability_id,
            "outcome": record.outcome,
            "on_time": record.on_time,
            // When it settled, and how long it took end to end. The
            // page said "on time" without ever saying on time for what,
            // or how long anyone waited.
            "amount": contract
                .as_ref()
                .map(|c| crate::amount::Amount::from_f64_rounding(c.terms.price.amount).to_string_decimal()),
            "currency": contract.as_ref().map(|c| c.terms.price.currency.clone()),
            "duration_seconds": contract
                .as_ref()
                .filter(|c| c.created_at > 0 && record.at >= c.created_at)
                .map(|c| record.at - c.created_at),
            "started_at": contract.as_ref().map(|c| c.created_at),
            "remedied": record.remedied,
            "at": record.at,
            // The criteria are public: they are what the verdict judged.
            "acceptance_criteria": contract.as_ref().map(|c| c.terms.acceptance_criteria.clone()),
            "verdict": verdict.as_ref().map(|v| json!({
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

    /// What a settled job was worth, from the contract it settled.
    ///
    /// Not stored on `JobRecord`, for the same reason its duration is
    /// not: the contract is where the agreed price lives, so deriving
    /// it keeps the figure right for every job settled before this
    /// existed and makes the two impossible to desynchronise.
    ///
    /// Returns `None` when the contract is gone, and the pages render a
    /// dash for that rather than a zero. A marketplace that reports
    /// 0.00 for work it cannot price is understating its own volume.
    fn job_amount(&self, job_ref: &str) -> Option<(String, String)> {
        let cid = self.jobs_by_ref.get(job_ref)?;
        let c = self.contract_for_read(cid)?;
        Some((
            crate::amount::Amount::from_f64_rounding(c.terms.price.amount).to_string_decimal(),
            c.terms.price.currency.clone(),
        ))
    }

    /// Walk the audit spine and check every link.
    ///
    /// A chain nobody can check is decoration, so this is public and
    /// unauthenticated in its summary form: anyone can ask whether this
    /// node's history hangs together, and recompute it themselves from
    /// `/v1/audit` using the rule in `storage::event_hash`.
    ///
    /// It reports where the chain STARTS, and that number is the honest
    /// part. Events written before the chain existed carry no hash and
    /// cannot be given one now: hashing them today would produce a
    /// chain proving only that nobody has touched them since, while
    /// looking identical to one proving they were never touched at all.
    /// Saying "verified from seq N" is a smaller claim than the truth
    /// would allow, and it is the one that is actually true.
    /// Verify the chain, reusing the last answer when it is still true.
    ///
    /// Two rules, and either one is enough to serve from memory:
    ///
    ///   * the spine has not grown since the last check - nothing can
    ///     have changed, so the answer is not stale, it is current;
    ///   * or the last check is recent, where "recent" is derived from
    ///     what it COST rather than fixed. A flat one-second cache is
    ///     fine at eight thousand events and a treadmill at a million:
    ///     the node would spend every second recomputing a three-second
    ///     answer. Backing off to eight times the measured cost keeps
    ///     the work under an eighth of one core no matter how long the
    ///     chain gets.
    ///
    /// A served answer always says how old it is and what height it was
    /// taken at, so nobody has to guess whether they are looking at
    /// this second or the last one.
    pub fn verify_spine(&mut self) -> Value {
        let head = self.storage.head_seq().unwrap_or(0);
        if let Some(prev) = &self.spine_check {
            let backoff = std::cmp::max(
                std::time::Duration::from_secs(1),
                prev.took.saturating_mul(8),
            );
            if prev.head == head || prev.at.elapsed() < backoff {
                let mut v = prev.value.clone();
                if let Some(o) = v.as_object_mut() {
                    o.insert("checked_at_seq".into(), json!(prev.head));
                    o.insert(
                        "checked_ms_ago".into(),
                        json!(prev.at.elapsed().as_millis() as u64),
                    );
                }
                return v;
            }
        }
        let started = std::time::Instant::now();
        let value = self.verify_spine_uncached();
        let took = started.elapsed();
        self.spine_check = Some(SpineCheck {
            head,
            at: std::time::Instant::now(),
            took,
            value: value.clone(),
        });
        let mut v = value;
        if let Some(o) = v.as_object_mut() {
            o.insert("checked_at_seq".into(), json!(head));
            o.insert("checked_ms_ago".into(), json!(0));
        }
        v
    }

    fn verify_spine_uncached(&self) -> Value {
        // Paged, not slurped.
        //
        // `events_after(0, u64::MAX)` asked the store for the entire
        // chain in one answer, which was free while the whole chain sat
        // in memory and is a way to run the node out of it now that
        // only a window does. Walking in pages holds one page at a
        // time, whatever the chain has grown to.
        const PAGE: u64 = 5_000;
        let mut events: Vec<crate::storage::EventRecord> = Vec::new();
        let mut cursor = 0u64;
        loop {
            let page = self.storage.events_after(cursor, PAGE).unwrap_or_default();
            let last = page.last().map(|e| e.seq);
            let n = page.len() as u64;
            events.extend(page);
            match last {
                Some(seq) if n == PAGE && seq > cursor => cursor = seq,
                _ => break,
            }
        }
        let mut segments: Vec<Value> = vec![];
        let mut breaks: Vec<u64> = vec![];
        let mut seg_from: Option<u64> = None;
        let mut seg_links = 0u64;
        let mut seg_last = 0u64;
        let mut expected_prev = String::new();

        for e in &events {
            if e.hash.is_empty() {
                continue; // written before the chain existed
            }
            let recomputed =
                crate::storage::event_hash(e.seq, &e.kind, e.at, &e.payload, &e.prev_hash);
            let links_here = seg_from.is_some() && e.prev_hash == expected_prev;
            if recomputed != e.hash || (seg_from.is_some() && !links_here) {
                // Close the running stretch and start a new one HERE.
                // Stopping dead at the first break lets one historical
                // incident hide every link after it, and the useful
                // question is not "is this perfect" but "which parts of
                // this history can I trust".
                if let Some(from) = seg_from {
                    segments
                        .push(json!({ "from_seq": from, "to_seq": seg_last, "links": seg_links }));
                }
                breaks.push(e.seq);
                seg_from = None;
            }
            if seg_from.is_none() {
                seg_from = Some(e.seq);
                seg_links = 0;
            }
            seg_links += 1;
            seg_last = e.seq;
            expected_prev = e.hash.clone();
        }
        if let Some(from) = seg_from {
            segments.push(json!({ "from_seq": from, "to_seq": seg_last, "links": seg_links }));
        }

        let unchained = events.iter().filter(|e| e.hash.is_empty()).count();
        let verified: u64 = segments.iter().filter_map(|s| s["links"].as_u64()).sum();
        json!({
            "intact": breaks.is_empty(),
            "breaks_at_seq": breaks,
            "segments": segments,
            "links_verified": verified,
            "events_total": events.len(),
            "unchained_prefix": unchained,
            "tip_hash": events.last().map(|e| e.hash.clone()).filter(|h| !h.is_empty()),
            "algorithm": "sha256 over {at,kind,payload,prev_hash,seq} as compact sorted-key JSON, \
        payload normalised through one parse/serialise cycle before it is stored and hashed",
            "note": "Events written before the chain existed carry no hash. They are counted in \
        unchained_prefix and excluded rather than hashed retroactively, which would manufacture evidence \
        this node does not have. A break does not stop verification: each unbroken stretch is reported \
        separately.",
        })
    }

    /// How long this settlement waits before the money moves
    /// (RFC-0009), or `None` when it moves immediately.
    ///
    /// OFF unless asked for, and that is a decision rather than an
    /// omission. RFC-0009 classes a settlement as `financial` with a
    /// one-hour default, but the contracts here are worth five cents
    /// and settle in seconds: a blanket hour would destroy the property
    /// this protocol exists to demonstrate while protecting nobody from
    /// anything. The RFC's own table says "transfer > threshold", so a
    /// threshold is what this reads.
    ///
    /// Note the overlap with RFC-0015 `human_review_above`, which gates
    /// the same moment. They are not redundant: one waits for a PERSON
    /// and the other waits for TIME, and a principal who is asleep is
    /// protected by exactly one of them.
    fn cooling_off_for(&self, contract: &Contract) -> Option<u64> {
        // The parties agreeing on a window beats whatever the operator
        // configured.
        if let Some(secs) = contract.terms.cooling_off_seconds {
            return (secs > 0).then_some(secs);
        }
        let threshold = std::env::var("GAP_COOLING_OFF_ABOVE")
            .ok()
            .and_then(|v| Amount::parse(&v).ok())?;
        let value = Amount::from_f64_rounding(contract.terms.price.amount);
        if value.minor_units() < threshold.minor_units() {
            return None;
        }
        Some(
            std::env::var("GAP_COOLING_OFF_SECONDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| {
                    crate::irreversibility::IrreversibilityClass::Financial.default_cooling_off()
                }),
        )
    }

    /// The buyer changes its mind inside the window (RFC-0009 §3.3).
    pub fn withdraw_consent(&mut self, token: &str, contract_id: &str) -> Result<Value> {
        let did = self.agent_by_token(token)?.identity.did().clone();
        let contract = self
            .contracts
            .get(contract_id)
            .cloned()
            .ok_or_else(|| Error::UnknownContract(contract_id.into()))?;
        if contract.client != did {
            return Err(Error::Unauthorized(
                "only the buyer may withdraw its own consent".into(),
            ));
        }
        let pending = self
            .cooling_off
            .get(contract_id)
            .cloned()
            .ok_or_else(|| Error::Other("nothing is cooling off for this contract".into()))?;
        if pending["settles_at"].as_u64().unwrap_or(0) <= now_unix() {
            return Err(Error::Other(
                "the window has elapsed; accept-delivery will settle it".into(),
            ));
        }
        self.cooling_off.remove(contract_id);
        self.forget_state("cooling_off", contract_id);
        self.record(
            "pay.cooling_off.withdrawn",
            json!({ "contract_id": contract_id }),
        );
        Ok(json!({
            "contract_id": contract_id,
            "state": "consent_withdrawn",
            "note": "Nothing was released. The escrow stays parked; the buyer may accept again \
        or dispute.",
        }))
    }

    /// Install a policy (RFC-0004).
    ///
    /// Operator-gated for every layer, including the personal one. That
    /// is narrower than the RFC allows - it envisages a principal
    /// managing its own rules - but a node that lets any caller add a
    /// policy lets any caller DENY somebody else's contracts, and a
    /// deny is terminal. Widening this needs principal-scoped
    /// ownership, which is real work rather than a looser check.
    pub fn add_policy(&mut self, admin: &str, policy_json: &Value) -> Result<Value> {
        match self.admin_token.as_deref() {
            Some(t) if t == admin => {}
            _ => return Err(Error::Unauthorized("operator token required".into())),
        }
        let policy: crate::policy::Policy = serde_json::from_value(policy_json.clone())
            .map_err(|e| Error::Other(format!("malformed policy: {e}")))?;
        if policy.rules.is_empty() {
            return Err(Error::Other(
                "a policy with no rules decides nothing; refusing to store it".into(),
            ));
        }
        let id = policy.policy_id.clone();
        self.save_state("policies", &id, &policy);
        self.policies.insert(id.clone(), policy);
        self.record("pol.install", json!({ "policy_id": id }));
        Ok(json!({ "policy_id": id, "policies": self.policies.len() }))
    }

    /// Evaluate a proposed contract against every layer (RFC-0004).
    ///
    /// Returns the signed decision record. `deny` is terminal and the
    /// caller refuses the proposal; anything else proceeds, with the
    /// record on the spine either way. A policy engine whose verdicts
    /// are not recorded is one nobody can argue with afterwards, which
    /// is most of what the RFC is for.
    fn evaluate_policy(
        &self,
        client: &crate::identity::Did,
        provider: &crate::identity::Did,
        capability_id: &str,
        terms: &Terms,
    ) -> Option<crate::policy::DecisionRecord> {
        if self.policies.is_empty() {
            return None; // nothing installed: no evaluation, no record
        }
        let mut engine = crate::policy::Engine::new();
        for p in self.policies.values() {
            engine.add_policy(p.clone());
        }
        let mut ctx = crate::policy::ActionContext::new();
        ctx.set("action.kind", json!("ctr.propose"));
        ctx.set("action.capability", json!(capability_id));
        ctx.set("action.amount", json!(terms.price.amount));
        ctx.set("action.currency", json!(terms.price.currency));
        ctx.set("action.autonomy", json!(terms.autonomy));
        ctx.set("actor.did", json!(client.to_string()));
        ctx.set("counterparty.did", json!(provider.to_string()));
        Some(engine.evaluate(&ctx, &self.node.identity, None))
    }

    /// What this node can honestly claim to speak (RFC-0011).
    ///
    /// A SELF-declaration, and it says so. RFC-0011 §3.3 defines an
    /// external Conformance Kit; running one is a stronger claim than
    /// this, and until that exists a node reporting its own level is
    /// telling you what it believes, not what a third party measured.
    ///
    /// The level is DERIVED, never configured. Each area below is
    /// backed by something that either exists in this binary or does
    /// not, and `conformance_areas_match_reality` in the test module
    /// checks the list against the router rather than trusting it. That
    /// matters more than it sounds: a hand-set level is exactly the
    /// kind of claim that stays true in the config long after it stopped
    /// being true in the code.
    ///
    /// Three areas are deliberately false today. `policy`, `delegation`
    /// and `compliance` have modules with tests and no way to reach
    /// them, so the node caps at L2 rather than claiming the L3 its
    /// feature list might suggest.
    pub fn conformance(&self) -> Value {
        use crate::conformance::{AreaResult, ConformanceReport, Level};

        // (area, served, why)
        let areas: &[(&str, bool, &str)] = &[
            (
                "identity",
                true,
                "DIDs, Ed25519 signing, key rotation, POST /v1/identity",
            ),
            (
                "message",
                true,
                "signed envelopes, replay window, message_id dedup",
            ),
            (
                "discovery",
                true,
                "POST /v1/announce, GET /v1/discover, TTL, deregister",
            ),
            ("agentcard", true, "GET /.well-known/gap-agent.json"),
            (
                "contract",
                true,
                "propose, accept, counter, reject, cancel, remedy",
            ),
            (
                "execution",
                true,
                "start, progress, deliver, verify, accept-delivery",
            ),
            (
                "payment",
                true,
                "escrow park, release, refund, arbitrated split",
            ),
            (
                "governance",
                true,
                "autonomy levels, principal binding, veto, budgets",
            ),
            (
                "receipt_chain",
                true,
                "hash-chained spine, GET /v1/audit/verify",
            ),
            (
                "policy",
                true,
                "layered engine evaluated on ctr.propose, POST /v1/policy, signed decision records",
            ),
            (
                // Partially served, and partial is not served. Chains
                // are registered, verified and drive per-tree
                // aggregation (RFC-0007), but mandate budgets are not
                // enforced against contracts and there is no
                // revocation, so claiming the area would be claiming
                // the half that is missing.
                "delegation",
                false,
                "chains registered and verified (POST /v1/delegation), but mandate budgets \
unenforced and no revocation",
            ),
            (
                "compliance",
                false,
                "src/compliance.rs exists and nothing calls it",
            ),
            (
                "tokenomics",
                false,
                "spec part 07 is informative; no implementation",
            ),
        ];

        let served: std::collections::HashSet<&str> = areas
            .iter()
            .filter(|(_, ok, _)| *ok)
            .map(|(a, _, _)| *a)
            .collect();
        let level = [Level::L4, Level::L3, Level::L2, Level::L1, Level::L0]
            .into_iter()
            .find(|l| l.required_areas().iter().all(|a| served.contains(a)))
            .unwrap_or(Level::L0);

        let report = ConformanceReport {
            report_id: format!(
                "urn:gap:conf:{}",
                &crate::sha256_hex(format!("{}|{}", self.node_did(), crate::VERSION).as_bytes())
                    [..16]
            ),
            level,
            suite_version: "self-declared".into(),
            per_area: areas
                .iter()
                .map(|(area, ok, _)| AreaResult {
                    area: (*area).to_string(),
                    tests_run: 1,
                    tests_passed: usize::from(*ok),
                })
                .collect(),
            implementation_version: crate::VERSION.to_string(),
            generated_at: now_unix(),
            signed_by: self.node_did(),
            sig: None,
        }
        .sign(&self.node.identity);

        let mut out = serde_json::to_value(&report).unwrap_or_else(|_| json!({}));
        out["missing_for_next_level"] = json!(match level {
            Level::L4 => vec![],
            other => {
                let next = match other {
                    Level::L0 => Level::L1,
                    Level::L1 => Level::L2,
                    Level::L2 => Level::L3,
                    _ => Level::L4,
                };
                next.required_areas()
                    .iter()
                    .filter(|a| !served.contains(*a))
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
            }
        });
        out["why"] = json!(areas
            .iter()
            .map(|(a, ok, why)| json!({ "area": a, "served": ok, "detail": why }))
            .collect::<Vec<_>>());
        out["note"] = json!(
            "Self-declared, not measured by an external Conformance Kit \
(RFC-0011 section 3.3). The level is derived from the areas below, never configured, and a test \
checks that list against the router."
        );
        out
    }

    /// Register a delegation chain, so this agent is known to belong to
    /// a tree (RFC-0001, RFC-0007).
    ///
    /// Everything RFC-0007 promises - "one bid per tree", per-tree rate
    /// limits, restricted actions a sub-agent may not perform - needs to
    /// know which tree an actor is in. Nothing could answer that, which
    /// is the whole reason those defences were dead code.
    ///
    /// The chain must end at the CALLER. Accepting a chain that
    /// delegates to somebody else would let an agent file itself under
    /// a stranger's tree, which is either impersonation or a way to
    /// exhaust another tree's quota.
    pub fn register_delegation(&mut self, token: &str, chain_json: &Value) -> Result<Value> {
        let did = self.agent_by_token(token)?.identity.did().clone();
        let chain: crate::delegation::TokenChain = serde_json::from_value(chain_json.clone())
            .map_err(|e| Error::Other(format!("malformed delegation chain: {e}")))?;
        // Signatures, parent linkage, depth, expiry, no escalation.
        chain.verify(now_unix())?;
        match chain.delegate() {
            Some(d) if *d == did => {}
            Some(other) => {
                return Err(Error::Unauthorized(format!(
                    "this chain delegates to {other}, not to you"
                )))
            }
            None => return Err(Error::Other("empty delegation chain".into())),
        }

        let root = crate::sybil::tree_root(&chain);
        let sub = crate::sybil::is_sub_agent(&chain);
        let budget = chain.effective_budget();
        let depth = chain.tokens.len();
        self.save_state("delegations", &did.to_string(), &chain);
        self.delegations.insert(did.to_string(), chain);
        self.record(
            "dlg.register",
            json!({ "agent_did": did.to_string(), "tree_root": root.to_string(), "depth": depth }),
        );
        Ok(json!({
            "agent_did": did.to_string(),
            "tree_root": root.to_string(),
            "depth": depth,
            "is_sub_agent": sub,
            "effective_budget": budget,
            "note": "Rate limits, contract caps and directory listings now aggregate across \
        this whole tree rather than per agent.",
        }))
    }

    /// The tree an agent belongs to; itself when it has no chain.
    ///
    /// An unregistered agent is its own tree, which keeps the aggregate
    /// rules total: there is no actor the per-tree limits do not apply
    /// to, so registering a chain can only ever tighten what an agent
    /// may do, never loosen it. A defence you can escape by declining
    /// to declare anything is not a defence.
    pub fn tree_of(&self, did: &str) -> String {
        self.delegations
            .get(did)
            .map(|c| crate::sybil::tree_root(c).to_string())
            .unwrap_or_else(|| did.to_string())
    }

    /// Refuse an action a sub-agent may never perform (RFC-0007 §3).
    pub fn check_restricted(
        &self,
        did: &str,
        action: crate::sybil::RestrictedAction,
    ) -> Result<()> {
        match self.delegations.get(did) {
            Some(chain) => crate::sybil::enforce_restricted(chain, action),
            // No chain means no delegation, which means not a sub-agent.
            None => Ok(()),
        }
    }

    /// Headline numbers for the public home page.
    ///
    /// Every field is derived from state this node actually holds — no
    /// figure here is a placeholder. A node that has settled nothing
    /// reports zeros and the page says so, because a marketplace that
    /// inflates its own volume is exactly the thing this protocol
    /// exists to make unnecessary.
    pub fn public_stats(&self) -> Value {
        let anns = self.registry.query(&Query::default());
        let capabilities: usize = anns.iter().map(|a| a.capabilities.len()).sum();
        // The lowest advertised price, which is the point: GAP is built
        // so that a job worth 0.05 is still worth contracting for.
        let cheapest = anns
            .iter()
            .flat_map(|a| a.capabilities.iter())
            .filter_map(|c| c.price.as_ref())
            .min_by(|a, b| a.amount.partial_cmp(&b.amount).unwrap_or(Ordering::Equal));

        // Read, not recomputed. These used to be five passes over every
        // job the node had ever recorded, per request.
        let st = &self.job_stats;
        let (total, judged, conforming, on_time, remedied) =
            (st.total, st.judged, st.conforming, st.on_time, st.remedied);

        let judges: Vec<String> = [
            self.verifier.as_ref().map(|v| v.name()),
            self.verifier_b.as_ref().map(|v| v.name()),
        ]
        .into_iter()
        .flatten()
        .collect();

        // Volume settled, BROKEN DOWN BY CURRENCY and never summed
        // across them. This node already carries both EUR and USDC, so
        // a single headline figure would be adding two different things
        // and calling the result money.
        //
        // Jobs whose contract is no longer in storage are counted in
        // `unpriced` rather than as zero: understating volume is still
        // misreporting it, and saying how many could not be priced is
        // the only honest way to publish the rest.
        // Summed as jobs settle. This was a contract lookup per job per
        // page load: 621,462 of them once the full history was restored,
        // most reaching ClickHouse, all under the global lock.
        let by_currency = self.job_stats.volume.clone();
        let unpriced = self.job_stats.unpriced;
        let volume = json!({
            "by_currency": by_currency
                .into_iter()
                .map(|(c, minor)| {
                    (c, crate::amount::Amount::from_minor(minor).to_string_decimal())
                })
                .collect::<std::collections::BTreeMap<_, _>>(),
            "unpriced_jobs": unpriced,
        });

        json!({
            "node": self.node_did().to_string(),
            "version": crate::VERSION,
            "agents": anns.len(),
            "capabilities": capabilities,
            "cheapest": cheapest.map(|p| json!({ "amount": p.amount, "currency": p.currency })),
            "contracts": self.contracts.total(),
            // Cache occupancy, not history. Published so that the day
            // eviction starts biting there is a number to look at
            // instead of a guess: if this sits at its ceiling while
            // reads are slow, the window is too small.
            "contracts_resident": self.contracts.resident(),
            "jobs": total,
            "judged": judged,
            "conforming": conforming,
            // Rates are None rather than 1.0 when nothing has settled:
            // a fresh node claiming a 100% success rate would be a lie
            // told by a division.
            "conform_rate": (judged > 0).then(|| conforming as f64 / judged as f64),
            "on_time_rate": (total > 0).then(|| on_time as f64 / total as f64),
            "remedied": remedied,
            "volume": volume,
            "escalated": self.escalations.len(),
            "judges": judges,
            "events": self.storage.event_count().unwrap_or(0),
            // RFC-0016: who holds the money here. A visitor should not
            // have to read an AgentCard to find out.
            "custody": self.custody,
            "custody_gaps": self.custody.declaration_gaps(),
            "liabilities": self.custody.mode.holds_funds()
                .then(|| self.liabilities().to_string_decimal()),
        })
    }

    /// One capability, in public: what it is, who offers it, and every
    /// settled job that used it (RFC-0014 §5).
    ///
    /// The node already let a reader audit an *agent* and a *job*. This
    /// is the third axis, and the one a buyer actually shops on: not
    /// "is this agent good" but "has this particular service been
    /// delivered before, and how did it go".
    ///
    /// Jobs stay pseudonymous exactly as elsewhere: the capability is
    /// public, who bought it is not.
    pub fn public_capability(&self, capability_id: &str) -> Result<Value> {
        let anns = self.registry.query(&Query::default());
        // The offer, and who makes it. An id is namespaced per agent, so
        // there is normally one - but the registry does not enforce that
        // and the page should not assume it.
        let mut offers = Vec::new();
        let mut name = String::new();
        let mut description = String::new();
        for a in &anns {
            for c in &a.capabilities {
                if c.id != capability_id {
                    continue;
                }
                if name.is_empty() {
                    name = c.name.clone();
                    description = c.description.clone();
                }
                let did = a.agent_did.to_string();
                let rep = self
                    .agents_by_did
                    .get(&did)
                    .and_then(|t| self.agents.get(t))
                    .map(|ag| ag.identity.reputation().clone())
                    .unwrap_or_default();
                offers.push(json!({
                    "did": did,
                    "name": a.name,
                    "price": c.price,
                    "score": rep.success_rate(),
                    "n": rep.executions,
                    "languages": a.languages,
                    "regions": a.regions,
                }));
            }
        }

        // Every settled job that used it. Records outlive an
        // announcement, so a capability withdrawn from the directory
        // still has a readable history - which is the point of keeping
        // one.
        let mut jobs: Vec<Value> = self
            .jobs
            .values()
            .flatten()
            .filter(|r| r.capability_id == capability_id)
            .map(|r| {
                json!({
                    "seq": r.seq,
                    "job_ref": r.job_ref,
                    "outcome": r.outcome,
                    "verdict": r.verdict,
                    "judged_by": r.judged_by,
                    "remedied": r.remedied,
                    "on_time": r.on_time,
                    "at": r.at,
                })
            })
            .collect();
        jobs.sort_by_key(|j| std::cmp::Reverse(j["seq"].as_u64().unwrap_or(0)));

        if offers.is_empty() && jobs.is_empty() {
            return Err(Error::Other("unknown capability".into()));
        }

        let total = jobs.len() as u64;
        let judged = jobs.iter().filter(|j| j["verdict"].is_string()).count() as u64;
        let conforming = jobs
            .iter()
            .filter(|j| j["verdict"] == json!("conforms"))
            .count() as u64;
        let on_time = jobs.iter().filter(|j| j["on_time"] == json!(true)).count() as u64;
        let remedied = jobs.iter().filter(|j| j["remedied"] == json!(true)).count() as u64;

        Ok(json!({
            "capability_id": capability_id,
            "name": name,
            "description": description,
            "offers": offers,
            "jobs": jobs,
            "settled": total,
            "judged": judged,
            "conforming": conforming,
            // Rates are absent rather than 1.0 on no evidence: a
            // capability nobody has bought must not read as flawless.
            "conform_rate": (judged > 0).then(|| conforming as f64 / judged as f64),
            "on_time_rate": (total > 0).then(|| on_time as f64 / total as f64),
            "remedied": remedied,
            "node": self.node_did().to_string(),
        }))
    }

    /// Every capability id this node knows about, announced or merely
    /// remembered. Used for the sitemap, so a withdrawn capability with
    /// a history stays indexable.
    pub fn known_capabilities(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .registry
            .query(&Query::default())
            .iter()
            .flat_map(|a| a.capabilities.iter().map(|c| c.id.clone()))
            .chain(
                self.jobs
                    .values()
                    .flatten()
                    .map(|r| r.capability_id.clone()),
            )
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// The most recent settlements, newest first. Same projection as
    /// the cursor form — one shape, so the page and the live stream can
    /// never disagree about what a settlement looks like.
    pub fn public_activity(&self, limit: usize) -> Value {
        // Pick the rows FIRST, render them second.
        //
        // This used to build the public JSON for every settled job on
        // the node - two contract lookups apiece - sort the lot, and
        // throw away all but the last fifty. At four thousand jobs that
        // is four thousand allocations per page load and per API call,
        // under the global state lock. Choosing by sequence first makes
        // it fifty.
        let mut recent: Vec<&JobRecord> = self.recent_jobs.iter().collect();
        recent.sort_by_key(|r| std::cmp::Reverse(r.seq));
        recent.truncate(limit);
        let jobs: Vec<Value> = recent.into_iter().map(|r| self.public_job_row(r)).collect();
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
                "provider": { "did": self.node_did(), "legal_name": "Geta.Team" },
                // Seal confidential payloads to this key (spec 01 §1.2).
                "encryption_key": {
                    "alg": crate::sealed::SealedEnvelope::ALG,
                    "x25519": self.node_encryption_key()
                }
            },
            "capabilities": [],
            "endpoints": {
                "invoke": "/v1/contract/propose",
                "discover": "/v1/discover",
                "billing": "/v1/escrow/park",
                // The steps an agent most often cannot find on its own.
                // A card that omits them leaves a provider guessing when
                // it may start, and a buyer with no way to collect what
                // it paid for.
                "start": "/v1/contract/{id}/start",
                "deliver": "/v1/contract/{id}/deliver",
                "deliverable": "/v1/contract/{id}/deliverable",
                "verify": "/v1/contract/{id}/verify",
                "settle": "/v1/contract/{id}/accept-delivery",
                "reputation": "/v1/reputation/{did}",
                "subscriptions": "/v1/subscriptions",
                "events": "/v1/events",
                "activity": "/v1/activity/stream"
            },
            // Where an agent reads the rules rather than inferring them.
            "documentation": {
                "for_agents": "/for-agents",
                "agents_md": "https://github.com/autonomous-lab/GAP/blob/main/AGENTS.md",
                "openapi": "https://github.com/autonomous-lab/GAP/blob/main/docs/openapi.yaml"
            },
            "auth": ["bearer"],
            // RFC-0016: who holds funds between park and release. A
            // buyer should not have to guess, and an agent can filter
            // on it the way it already filters on reputation.
            "custody": self.custody,
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
/// The Open Graph card, embedded in the binary.
///
/// Embedded rather than read from disk because everything else this
/// node serves is, and a container that renders its own pages but 404s
/// its preview image depending on the working directory is a deployment
/// trap nobody should have to find.
const OG_IMAGE: &[u8] = include_bytes!("ui/og.png");

/// The card's URL path, versioned by the bytes it serves.
///
/// A fixed `/og.png` cached for a day is a fixed `/og.png` that is
/// WRONG for a day. Measured, not assumed: after redeploying a new
/// card, the edge kept answering the old one with `cf-cache-status:
/// HIT` and `age: 648`, while the same URL with a cache-busting query
/// returned the new bytes. Every future tweak would have looked broken
/// for 24 hours.
///
/// Naming the file after its own content makes that impossible: new
/// bytes are a new URL that nothing has cached, and the old URL keeps
/// serving the old bytes to whatever already embedded it. This is why
/// a long `max-age` is safe here rather than reckless.
pub fn og_image_path() -> &'static str {
    static PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PATH.get_or_init(|| format!("/og-{}.png", &crate::sha256_hex(OG_IMAGE)[..12]))
}

/// Static binary assets, served before any HTML route.
///
/// An allow-list, not a file server: a path that is not one of these
/// finds nothing, however it is spelled.
///
/// `/og.png` stays as an alias so that a link shared before the card
/// was versioned still resolves - it simply serves whatever the current
/// card is, which is the honest answer to an unversioned request.
pub fn static_asset(path: &str) -> Option<(&'static str, &'static [u8])> {
    if path == "/og.png" || path == og_image_path() {
        return Some(("image/png", OG_IMAGE));
    }
    None
}

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
    let missing = |kind: &str, what: &str| {
        Some((
            404u16,
            "text/html; charset=utf-8",
            crate::ui::not_found_page(kind, what),
        ))
    };
    match clean {
        // The home page is not the directory. A visitor who has never
        // heard of GAP needs the mechanism explained before a list of
        // strangers means anything to them - and a crawler needs the
        // page to say what this site is.
        "/" => {
            let stats = guard.public_stats();
            let dir = guard.public_directory();
            let recent = guard.public_activity(6);
            html(crate::ui::home_page(&stats, &dir, &recent))
        }
        "/agents" => {
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
            // One number for both, and the same one the page enforces
            // client-side: a server render of 60 that the browser
            // immediately trims to 50 is 10 rows of wasted bytes on
            // every page load.
            let rows = crate::ui::FEED_ROWS;
            let recent = guard.public_activity(rows);
            let lifecycle = guard.public_lifecycle(rows);
            let stats = guard.public_stats();
            html(crate::ui::activity_page(&recent, &lifecycle, &stats))
        }
        "/how-it-works" => {
            let stats = guard.public_stats();
            html(crate::ui::how_it_works_page(&stats))
        }
        "/for-agents" => {
            let did = guard.node_did().to_string();
            let stats = guard.public_stats();
            html(crate::ui::for_agents_page(&did, &stats))
        }
        "/for-humans" => {
            let stats = guard.public_stats();
            html(crate::ui::for_humans_page(&stats))
        }
        "/docs" => {
            let did = guard.node_did().to_string();
            let v = guard.verifier_name();
            html(crate::ui::docs_page(&did, v.as_deref()))
        }
        // A deliberately trivial upstream, so the gateway can be
        // exercised end to end against something whose correct answer
        // is knowable: five percent WIN, ninety-five percent LOST.
        //
        // It lives in the node rather than in a service beside it
        // because the point is to test the PATH - 402, contract, escrow,
        // forward, settle - and a second deployment would only add ways
        // for the test to fail that have nothing to do with the code
        // under test. Registered as a gateway upstream it makes the node
        // call itself over its own public URL, which is a more honest
        // rehearsal than a loopback shortcut.
        //
        // Free, unauthenticated, and it changes nothing: it is a coin
        // toss with a timestamp.
        "/demo/loto" => {
            use rand::RngCore;
            let roll = rand::rngs::OsRng.next_u32() % 100;
            let win = roll < 5;
            Some((
                200,
                "application/json",
                json!({
                    "result": if win { "WIN" } else { "LOST" },
                    "roll": roll,
                    "odds": "5 in 100",
                    "at": now_unix(),
                })
                .to_string(),
            ))
        }
        "/robots.txt" => Some((200, "text/plain; charset=utf-8", crate::ui::robots(&base))),
        "/llms.txt" => {
            let stats = guard.public_stats();
            let did = guard.node_did().to_string();
            let judges: Vec<String> = [guard.verifier_name(), guard.verifier_b_name()]
                .into_iter()
                .flatten()
                .collect();
            Some((
                200,
                "text/plain; charset=utf-8",
                crate::ui::llms_txt(&base, &did, &stats, &judges),
            ))
        }
        "/sitemap.xml" => {
            let dir = guard.public_directory();
            // Jobs are listed too: each settled verdict is a page worth
            // indexing on its own, and it is the evidence behind a score.
            let activity = guard.public_activity(5000);
            Some((
                200,
                "application/xml; charset=utf-8",
                crate::ui::sitemap(&base, &dir, &activity, &guard.known_capabilities()),
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
            let s = guard.public_stats();
            html(crate::ui::admin_page(&e, &d, &a, &s))
        }
        // These three answer 404 rather than falling through. Returning
        // `None` here handed the request to the JSON API, which told a
        // visitor clicking a link on our own pages that the route was
        // unknown - when the route was ours and only the record was
        // missing.
        p if p.starts_with("/capability/") => {
            let id = percent_decode(p.trim_start_matches("/capability/"));
            match guard.public_capability(&id) {
                Ok(cap) => html(crate::ui::capability_page(&cap)),
                Err(_) => missing("capability", &id),
            }
        }
        p if p.starts_with("/job/") => {
            let job_ref = percent_decode(p.trim_start_matches("/job/"));
            match guard.public_job(&job_ref) {
                Ok(job) => html(crate::ui::job_page(&job)),
                Err(_) => missing("job", &job_ref),
            }
        }
        p if p.starts_with("/agent/") => {
            let did = percent_decode(p.trim_start_matches("/agent/"));
            let rep = match guard.reputation_of(&did) {
                Ok(r) => r,
                Err(_) => return missing("agent", &did),
            };
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

/// Read an `amount` field, accepting a decimal string or a number.
fn amount_from(body: &Value) -> Result<Amount> {
    match body.get("amount") {
        Some(v) if v.is_string() => Amount::parse(v.as_str().unwrap_or("")),
        Some(v) if v.is_number() => Ok(Amount::from_f64_rounding(v.as_f64().unwrap_or(0.0))),
        _ => Err(Error::Other("amount required (decimal string)".into())),
    }
}

/// Route with client IP for rate limiting (audit H-03).
/// Serve `/x402/{slug}/{tail}` - the gateway.
///
/// Deliberately outside `route()`: the upstream call must happen with
/// the node lock RELEASED, and a function that takes a guard cannot
/// promise that. The shape here - lock, decide, unlock, network, lock,
/// settle - is the whole point, and it is why `gateway_begin` hands
/// back the opened credential instead of making the call itself.
pub fn gateway_serve(
    state: &Arc<Mutex<NodeState>>,
    method: &str,
    path: &str,
    body: &[u8],
    auth: Option<&str>,
    contract_hdr: Option<&str>,
    base: &str,
) -> Option<(u16, String, Vec<u8>)> {
    let rest = path.strip_prefix("/x402/")?;
    let (slug, tail) = match rest.split_once('/') {
        Some((s, t)) => (s, t),
        None => (rest, ""),
    };
    let resource = format!("{base}{path}");
    let token = auth.and_then(|a| a.strip_prefix("Bearer "));

    let step = {
        let mut guard = state.lock().ok()?;
        match guard.gateway_begin(slug, tail, &resource, token, contract_hdr) {
            Ok(step) => step,
            Err(e) => {
                let (code, payload) = crate::server::error_response(&e);
                return Some((
                    code,
                    "application/json".into(),
                    payload.to_string().into_bytes(),
                ));
            }
        }
    };

    let (url, auth_header, auth_value, contract_id, provider_token, client_token) = match step {
        crate::gateway::GatewayStep::Challenge(v) => {
            return Some((402, "application/json".into(), v.to_string().into_bytes()))
        }
        crate::gateway::GatewayStep::Forward {
            url,
            auth_header,
            auth_value,
            contract_id,
            provider_token,
            client_token,
        } => (
            url,
            auth_header,
            auth_value,
            contract_id,
            provider_token,
            client_token,
        ),
    };

    // ---- lock released, network from here ----
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .new_agent();
    // Built per branch rather than shared: ureq types a request with a
    // body differently from one without, and forcing them into one
    // variable would mean sending a body on GET to satisfy the compiler.
    let sent = if method == "GET" {
        let mut req = agent.get(&url);
        if !auth_value.is_empty() {
            req = req.header(&auth_header, &auth_value);
        }
        req.call()
    } else {
        let mut req = agent.post(&url).header("Content-Type", "application/json");
        if !auth_value.is_empty() {
            req = req.header(&auth_header, &auth_value);
        }
        req.send(body.to_vec())
    };
    let (status, media, payload) = match sent {
        Ok(mut r) => {
            let status = r.status().as_u16();
            let media = r
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string();
            let bytes = r.body_mut().read_to_vec().unwrap_or_default();
            (status, media, bytes)
        }
        // The upstream failed, so nothing was delivered and nothing is
        // owed. The contract is left funded and unfulfilled rather than
        // settled - the buyer can retry, and the expiry sweep refunds it
        // if nobody does. Settling here would pay for a call that never
        // returned.
        Err(e) => {
            return Some((
                502,
                "application/json".into(),
                json!({ "error": { "code": "upstream_failed", "message": e.to_string(),
                        "contract_id": contract_id, "note": "escrow still parked; retry or let it expire" } })
                .to_string()
                .into_bytes(),
            ))
        }
    };

    // ---- back under the lock, only to record ----
    let receipt = {
        let mut guard = state.lock().ok()?;
        guard.gateway_complete(
            &contract_id,
            &provider_token,
            &client_token,
            media.split(';').next().unwrap_or(&media),
            &payload,
        )
    };
    let job_ref = receipt
        .as_ref()
        .ok()
        .and_then(|v| v["job_ref"].as_str().map(str::to_string))
        .unwrap_or_else(|| crate::server::pseudonym(&contract_id));
    let _ = receipt;
    Some((status, media, payload)).map(|(s, m, p)| (s, format!("{m}; gap-job={job_ref}"), p))
}

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

    let token = auth.and_then(|h| h.strip_prefix("Bearer "));

    // This route is reachable only over the compose-internal network and is
    // authenticated with the sandbox shared secret. It deliberately bypasses
    // agent auth: the runner never receives an owner's bearer token.
    if method == "POST" && path == "/internal/functions/capability" {
        let authorized = token.is_some() && guard.function_sandbox_token.as_deref() == token;
        if !authorized {
            return error_response(&Error::Unauthorized("invalid sandbox token".into()));
        }
        let project_id = body
            .get("project_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let request = body.get("request").cloned().unwrap_or(Value::Null);
        let root = guard.cloud_root.clone();
        drop(guard);
        return match execute_function_capability(&root, &project_id, &request) {
            Ok(value) => (200, value),
            Err(error) => error_response(&error),
        };
    }

    // Rate limiting first (audit H-03): 429 when over limit.
    if guard.check_rate_limit(token, client_ip).is_err() {
        return (
            429,
            json!({ "error": { "code": "rate_limited", "message": "too many requests" } }),
        );
    }

    if let Some((project_id, name, tail)) = public_function_route(path) {
        let store = match crate::cloud::ProjectStore::open(&guard.cloud_root, project_id) {
            Ok(store) => store,
            Err(error) => return error_response(&error),
        };
        let policy = match store.function_http_policy(name) {
            Ok(policy) => policy,
            Err(error) => return error_response(&error),
        };
        if method == "OPTIONS" {
            return (200, json!({ "ok": true }));
        }
        let allowed = match policy.auth.as_str() {
            "public" => true,
            "token" => token.is_some_and(|candidate| {
                verify_function_token(
                    guard.realtime_secret.as_deref(),
                    candidate,
                    project_id,
                    name,
                )
            }),
            _ => token
                .is_some_and(|candidate| guard.cloud_owned_project(candidate, project_id).is_ok()),
        };
        if !allowed {
            return error_response(&Error::Unauthorized("function HTTP access denied".into()));
        }
        let query = raw_path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let request =
            json!({ "method": method, "path": format!("/{tail}"), "query": query, "body": body });
        let prepared = guard.cloud_prepare_public_invocation(project_id, name, request);
        drop(guard);
        return match prepared.and_then(|(url, sandbox_token, payload, version, digest)| {
            invoke_function_sandbox(&url, &sandbox_token, &payload)
                .map(|output| (output, version, digest))
        }) {
            Ok((output, version, digest)) => (
                200,
                json!({ "result": output, "version": version, "digest": digest }),
            ),
            Err(error) => error_response(&error),
        };
    }

    // Sandbox I/O must happen without the global protocol lock. A slow or
    // hostile function gets its own timeout; it must not stop contracts,
    // payments and unrelated agents while that timeout elapses.
    if method == "POST" {
        if let Some((project_id, action)) = cloud_database_action(path) {
            let prepared = match token {
                Some(t) => guard.cloud_prepare_database(t, project_id),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            };
            let sql = body.get("sql").and_then(Value::as_str).unwrap_or("");
            let params = body
                .get("params")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            drop(guard);
            let result = prepared.and_then(|root| {
                let store = crate::cloud::ProjectStore::open(&root, project_id)?;
                match action {
                    "query" => store.database_query(sql, &params),
                    "execute" => store.database_execute(sql, &params),
                    _ => unreachable!(),
                }
            });
            return match result {
                Ok(output) => {
                    if let Ok(mut state) = state.lock() {
                        state.record(
                            &format!("cloud.database.{action}"),
                            json!({
                                "project_id": project_id,
                                "sql_digest": crate::sha256_hex(sql.as_bytes()),
                                "affected_rows": output.affected_rows,
                                "returned_rows": output.rows.len(),
                                "truncated": output.truncated
                            }),
                        );
                    }
                    match serde_json::to_value(output) {
                        Ok(value) => (200, value),
                        Err(error) => error_response(&Error::Other(error.to_string())),
                    }
                }
                Err(error) => error_response(&error),
            };
        }
        if let Some((project_id, name)) = cloud_function_action(path, "invoke") {
            let prepared = match token {
                Some(t) => guard.cloud_prepare_invocation(
                    t,
                    project_id,
                    name,
                    body.get("request").cloned().unwrap_or_else(|| body.clone()),
                ),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            };
            drop(guard);
            let result = prepared.and_then(|(url, sandbox_token, payload, version, digest)| {
                invoke_function_sandbox(&url, &sandbox_token, &payload)
                    .map(|output| (output, version, digest))
            });
            return match result {
                Ok((output, version, digest)) => {
                    if let Ok(mut state) = state.lock() {
                        state.record(
                            "cloud.function.invoked",
                            json!({ "project_id": project_id, "name": name, "version": version, "digest": digest }),
                        );
                    }
                    (
                        200,
                        json!({ "result": output, "version": version, "digest": digest }),
                    )
                }
                Err(e) => error_response(&e),
            };
        }
    }

    if let Some(project_id) = cloud_egress_route(path) {
        let result = match token {
            Some(t) => guard.cloud_owned_project(t, project_id).and_then(|_| {
                let mut store = crate::cloud::ProjectStore::open(&guard.cloud_root, project_id)?;
                match method {
                    "GET" => Ok(json!({ "hosts": store.egress_hosts()? })),
                    "PUT" => {
                        let hosts = body
                            .get("hosts")
                            .and_then(Value::as_array)
                            .ok_or_else(|| Error::Other("hosts must be an array".into()))?
                            .iter()
                            .map(|v| {
                                v.as_str().map(str::to_string).ok_or_else(|| {
                                    Error::Other("each host must be a string".into())
                                })
                            })
                            .collect::<Result<Vec<_>>>()?;
                        store.set_egress_hosts(&hosts)?;
                        Ok(json!({ "hosts": store.egress_hosts()? }))
                    }
                    _ => Err(Error::Other("method not allowed".into())),
                }
            }),
            None => Err(Error::Unauthorized("missing bearer token".into())),
        };
        return match result {
            Ok(value) => (200, value),
            Err(error) => error_response(&error),
        };
    }

    if let Some((project_id, schedule_id)) = cloud_schedule_route(path) {
        let result = match token {
            Some(t) => guard.cloud_owned_project(t, project_id).and_then(|_| {
                let mut store = crate::cloud::ProjectStore::open(&guard.cloud_root, project_id)?;
                match (method, schedule_id) {
                    ("GET", None) => Ok(json!({ "schedules": store.schedules()? })),
                    ("PUT", Some(id)) => {
                        let function = body.get("function").and_then(Value::as_str).unwrap_or("");
                        let cron = body.get("cron").and_then(Value::as_str).unwrap_or("");
                        let request = body.get("request").unwrap_or(&Value::Null);
                        serde_json::to_value(store.put_schedule(
                            id,
                            function,
                            cron,
                            request,
                            now_unix(),
                        )?)
                        .map_err(|e| Error::Other(e.to_string()))
                    }
                    ("DELETE", Some(id)) => Ok(json!({ "deleted": store.delete_schedule(id)? })),
                    _ => Err(Error::Other("method not allowed".into())),
                }
            }),
            None => Err(Error::Unauthorized("missing bearer token".into())),
        };
        return match result {
            Ok(value) => (200, value),
            Err(error) => error_response(&error),
        };
    }

    let response = match (method, path) {
        // ---- health & card ----
        ("GET", "/health") => Ok(json!({ "status": "ok", "node": guard.node_did() })),
        // ---- public directory data (consumed by the web UI) ----
        ("GET", "/v1/directory") => Ok(guard.public_directory()),
        ("POST", "/v1/cloud/projects") => match token {
            Some(t) => guard
                .cloud_create_project(t)
                .and_then(|p| serde_json::to_value(p).map_err(|e| Error::Other(e.to_string()))),
            None => Err(Error::Unauthorized("missing bearer token".into())),
        },
        ("GET", "/v1/cloud/projects") => match token {
            Some(t) => guard
                .cloud_list_projects(t)
                .map(|projects| json!({ "projects": projects })),
            None => Err(Error::Unauthorized("missing bearer token".into())),
        },
        ("POST", p) if cloud_realtime_token_route(p).is_some() => {
            let project_id = cloud_realtime_token_route(p).expect("guarded above");
            let channels = match body.get("channels") {
                None => Ok(Vec::new()),
                Some(Value::Array(values)) if values.iter().all(Value::is_string) => Ok(values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()),
                _ => Err(Error::Other("channels must be an array of strings".into())),
            };
            let permissions = match body.get("permissions") {
                None => Ok(vec!["subscribe".to_string(), "publish".to_string()]),
                Some(Value::Array(values)) if values.iter().all(Value::is_string) => Ok(values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()),
                _ => Err(Error::Other(
                    "permissions must be an array of strings".into(),
                )),
            };
            let subject = body.get("subject").and_then(Value::as_str);
            match (token, channels, permissions) {
                (Some(t), Ok(channels), Ok(permissions)) => guard.cloud_issue_realtime_token(
                    t,
                    project_id,
                    &channels,
                    &permissions,
                    subject,
                ),
                (_, Err(error), _) | (_, _, Err(error)) => Err(error),
                (None, _, _) => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        ("PUT", p) if cloud_function_http_route(p).is_some() => {
            let (project_id, name, _) = cloud_function_http_route(p).expect("guarded above");
            let auth_mode = body
                .get("auth")
                .and_then(Value::as_str)
                .unwrap_or("private");
            let origins = body
                .get("cors_origins")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| Error::Other("cors_origins must contain strings".into()))
                })
                .collect::<Result<Vec<_>>>();
            match (token, origins) {
                (Some(t), Ok(origins)) => guard.cloud_owned_project(t, project_id).and_then(|_| {
                    let mut store =
                        crate::cloud::ProjectStore::open(&guard.cloud_root, project_id)?;
                    store.set_function_http_policy(name, auth_mode, &origins)?;
                    serde_json::to_value(store.function_http_policy(name)?)
                        .map_err(|e| Error::Other(e.to_string()))
                }),
                (_, Err(error)) => Err(error),
                _ => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        ("POST", p)
            if cloud_function_http_route(p).is_some_and(|(_, _, action)| action == "tokens") =>
        {
            let (project_id, name, _) = cloud_function_http_route(p).expect("guarded above");
            match token {
                Some(t) => guard.cloud_issue_function_token(t, project_id, name),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        (m, p) if matches!(m, "PUT" | "GET") && cloud_route(p, "kv").is_some() => {
            let (project_id, key) = cloud_route(p, "kv").expect("guarded above");
            match (token, m) {
                (Some(t), "PUT") => (|| -> Result<Value> {
                    let encoded = body
                        .get("value_base64")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let value = crate::artifact::decode_base64(encoded)
                        .ok_or_else(|| Error::Other("value_base64 is invalid".into()))?;
                    guard.cloud_put_kv(
                        t,
                        project_id,
                        &key,
                        &value,
                        body.get("expires_at").and_then(Value::as_u64),
                    )?;
                    Ok(json!({ "stored": true }))
                })(),
                (Some(t), "GET") => {
                    use base64::Engine;
                    guard.cloud_get_kv(t, project_id, &key).map(|value| match value {
                        Some(bytes) => json!({
                            "found": true,
                            "value_base64": base64::engine::general_purpose::STANDARD.encode(bytes)
                        }),
                        None => json!({ "found": false }),
                    })
                }
                (None, _) => Err(Error::Unauthorized("missing bearer token".into())),
                _ => unreachable!(),
            }
        }
        (m, p) if matches!(m, "PUT" | "GET") && cloud_route(p, "objects").is_some() => {
            let (project_id, key) = cloud_route(p, "objects").expect("guarded above");
            match (token, m) {
                (Some(t), "PUT") => (|| -> Result<Value> {
                    let encoded = body
                        .get("content_base64")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let content = crate::artifact::decode_base64(encoded)
                        .ok_or_else(|| Error::Other("content_base64 is invalid".into()))?;
                    let media_type = body
                        .get("media_type")
                        .and_then(Value::as_str)
                        .unwrap_or("application/octet-stream");
                    guard
                        .cloud_put_object(t, project_id, &key, &content, media_type)
                        .map(|digest| json!({ "stored": true, "digest": digest }))
                })(),
                (Some(t), "GET") => {
                    use base64::Engine;
                    guard.cloud_get_object(t, project_id, &key).map(|object| match object {
                        Some(object) => json!({
                            "found": true,
                            "content_base64": base64::engine::general_purpose::STANDARD.encode(object.content),
                            "media_type": object.media_type,
                            "digest": object.digest
                        }),
                        None => json!({ "found": false }),
                    })
                }
                (None, _) => Err(Error::Unauthorized("missing bearer token".into())),
                _ => unreachable!(),
            }
        }
        ("POST", p) if cloud_function_route(p, true).is_some() => {
            let (project_id, name) = cloud_function_route(p, true).expect("guarded above");
            let version = body.get("version").and_then(Value::as_u64).unwrap_or(0);
            match token {
                Some(t) if version > 0 => guard
                    .cloud_activate_function(t, project_id, name, version)
                    .map(|_| json!({ "active": true, "version": version })),
                Some(_) => Err(Error::Other("version required".into())),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        ("POST", p) if cloud_function_route(p, false).is_some() => {
            let (project_id, name) = cloud_function_route(p, false).expect("guarded above");
            let runtime = body.get("runtime").and_then(Value::as_str).unwrap_or("");
            let source = body.get("source").and_then(Value::as_str).unwrap_or("");
            match token {
                Some(t) => guard
                    .cloud_deploy_function(t, project_id, name, runtime, source.as_bytes())
                    .and_then(|v| serde_json::to_value(v).map_err(|e| Error::Other(e.to_string()))),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        ("DELETE", p) if cloud_function_version_route(p).is_some() => {
            let (project_id, name, version) =
                cloud_function_version_route(p).expect("guarded above");
            match token {
                Some(t) => guard
                    .cloud_delete_function_version(t, project_id, name, version)
                    .map(|deleted| json!({ "deleted": deleted, "name": name, "version": version })),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        ("DELETE", p) if cloud_function_route(p, false).is_some() => {
            let (project_id, name) = cloud_function_route(p, false).expect("guarded above");
            match token {
                Some(t) => guard
                    .cloud_delete_function(t, project_id, name)
                    .map(|deleted| json!({ "deleted": deleted, "name": name })),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        ("POST", "/v1/gateway") => match auth.and_then(|a| a.strip_prefix("Bearer ")) {
            Some(t) => guard.gateway_register(t, &body),
            None => Err(Error::Unauthorized("missing bearer token".into())),
        },
        ("GET", "/v1/gateway") => Ok(guard.gateway_list()),
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
        // The whole deal lifecycle, not just what settled. Same cursor
        // and same pseudonyms as /v1/activity, so the two feeds can be
        // read side by side or merged on `deal_ref`.
        ("GET", "/v1/activity/lifecycle") => {
            let params = parse_url_params(raw_path);
            let limit = params
                .get("limit")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(60)
                .min(500);
            match params.get("after").and_then(|v| v.parse::<u64>().ok()) {
                Some(after) => Ok(guard.public_lifecycle_after(after, limit.max(200))),
                None => Ok(guard.public_lifecycle(limit)),
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
            // Self-declared identity. Re-announcing with a different
            // name is how an agent renames itself - there is no separate
            // update call to forget to implement.
            let profile = crate::discovery::AgentProfile::new(
                body.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                body.get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            );
            match (token, caps.is_empty()) {
                (Some(t), false) => guard
                    .announce_request(
                        t,
                        crate::discovery::AnnounceRequest {
                            capabilities: caps,
                            languages,
                            regions,
                            ttl_seconds: ttl,
                            reachability,
                            profile,
                        },
                    )
                    .map(|id| json!({ "announcement_id": id })),
                (None, _) => Err(Error::Unauthorized("missing bearer token".into())),
                (_, true) => Err(Error::Other("capabilities required".into())),
            }
        }
        ("GET", "/v1/discover") => {
            let q = parse_query(&body, raw_path);
            // Spec 02 §2.4.3: the result SET is signed by the registry,
            // so a node that quietly drops a competitor from an answer
            // can be caught. `results` stays at the top level for
            // existing clients.
            let signed = guard.discover_signed(&q);
            Ok(json!({
                "results": signed.results,
                "registry": signed.registry.to_string(),
                "query_digest": signed.query_digest,
                "at": signed.at,
                "signature": signed.signature,
            }))
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
            // Accept both spellings: `uri` is what the examples and the
            // OpenAPI description use, `deliverable_uri` mirrors the
            // hash field. Silently ignoring the one an agent happened to
            // pick is how a provider ends up believing it handed over an
            // artifact that never arrived.
            let uri = body
                .get("deliverable_uri")
                .or_else(|| body.get("uri"))
                .and_then(|v| v.as_str());
            let artifact = crate::artifact::Artifact::parse(&body);
            match token {
                Some(t) => guard.deliver(t, id, hash, uri, artifact).map(|_| {
                    json!({
                        "state": "delivered",
                        "artifact_held": guard.deliverable_of(id).is_some(),
                        "retrieve": format!("/v1/contract/{id}/deliverable"),
                    })
                }),
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
        // The artifact itself. Restricted to the two parties: a public
        // node directory is one thing, handing anyone the work somebody
        // paid for is another.
        ("GET", p) if p.starts_with("/v1/contract/") && p.ends_with("/deliverable") => {
            let id = percent_decode(
                p.trim_start_matches("/v1/contract/")
                    .trim_end_matches("/deliverable"),
            );
            match token {
                Some(t) => guard.fetch_deliverable(t, &id),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        ("GET", p) if p.starts_with("/v1/contract/") => {
            let id = p.trim_start_matches("/v1/contract/");
            match guard.contracts.get(id) {
                Some(c) => Ok(json!({
                    "contract_id": c.contract_id,
                    "state": c.state.wire_name(),
                    // Whether it is safe to start working. `state` alone
                    // never answered that question, so a provider had to
                    // guess - and the ones that guessed wrong paid for
                    // the compute before finding out.
                    "escrow_required": c.escrow,
                    "escrow_funded": guard.escrow_is_funded(id),
                    "provider_may_start": !c.escrow || guard.escrow_is_funded(id),
                    "contract": c,
                    // The whole spine, not the first hundred events ever
                    // written. `events_after(0, 100)` returns sequences
                    // 1..100, so every contract after the hundredth
                    // event on this node showed an EMPTY history while
                    // its events sat in storage - and the older the node,
                    // the more contracts it silently applied to.
                    "events": guard.storage.events_after(0, u64::MAX).unwrap_or_default()
                        .into_iter()
                        .filter(|e| e.payload.get("contract_id").and_then(|v| v.as_str()) == Some(id))
                        .collect::<Vec<_>>(),
                })),
                None => Err(Error::UnknownContract(id.into())),
            }
        }

        // ---- custody & balances (RFC-0016) ----
        ("GET", "/v1/balance") => match token {
            Some(t) => guard
                .agent_by_token(t)
                .map(|a| a.identity.did().to_string())
                .map(|did| {
                    let b = guard.balance_of(&did);
                    json!({
                        "agent_did": did,
                        "available": b.available.to_string_decimal(),
                        "held": b.held.to_string_decimal(),
                        // Money the agent has asked for and the node
                        // has not sent yet. Omitting it made a pending
                        // payout look like a vanished one.
                        "withdrawing": b.withdrawing.to_string_decimal(),
                        "total": b.total().to_string_decimal(),
                        "currency": b.currency,
                        "custody": guard.custody(),
                    })
                }),
            None => Err(Error::Unauthorized("missing bearer token".into())),
        },
        // The amount is deliberately NOT read from the body: the
        // depositor is the party that benefits from overstating it.
        ("POST", "/v1/balance/deposit") => {
            let tx = body
                .get("tx")
                .or_else(|| body.get("transaction"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match token {
                Some(t) => guard.deposit_from_chain(t, tx),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        // Close the deals time already ended. Operator authority, and
        // it defaults to a DRY RUN: a bulk state change on contracts is
        // the one call where "show me first" must be what happens when
        // the caller says nothing.
        ("POST", "/v1/admin/expire") => {
            let secs = |k: &str, d: u64| body.get(k).and_then(|v| v.as_u64()).unwrap_or(d);
            let dry_run = body
                .get("dry_run")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            match token {
                Some(t) => guard.expire_stale_contracts(
                    t,
                    now_unix(),
                    secs("grace_seconds", 86_400),
                    secs("max_idle_seconds", crate::EXPIRE_AFTER_SECS),
                    body.get("auto_accept_delivered")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true),
                    dry_run,
                ),
                None => Err(Error::Unauthorized("operator token required".into())),
            }
        }
        // Rails the node cannot read itself (bank, card). Operator
        // authority, and it must point at an external reference.
        ("POST", "/v1/balance/credit") => {
            let amount = amount_from(&body);
            let agent = body.get("agent_did").and_then(|v| v.as_str()).unwrap_or("");
            let reference = body.get("reference").and_then(|v| v.as_str()).unwrap_or("");
            match (token, amount) {
                (Some(t), Ok(a)) => guard.credit_off_chain(t, agent, &a, reference),
                (None, _) => Err(Error::Unauthorized("operator token required".into())),
                (_, Err(e)) => Err(e),
            }
        }
        ("POST", "/v1/balance/withdraw") => {
            let amount = amount_from(&body);
            let destination = body
                .get("destination")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match (token, amount) {
                (Some(t), Ok(a)) => guard.request_withdrawal(t, &a, destination),
                (None, _) => Err(Error::Unauthorized("missing bearer token".into())),
                (_, Err(e)) => Err(e),
            }
        }
        // The other half of the rail. A payout the node cannot watch
        // leave is a payout an operator has to attest to, exactly as it
        // does for an incoming bank transfer.
        ("POST", "/v1/balance/withdraw/settle") => {
            let id = body
                .get("request_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let reference = body.get("reference").and_then(|v| v.as_str()).unwrap_or("");
            match token {
                Some(t) => guard.settle_withdrawal(t, id, reference),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        ("POST", "/v1/balance/withdraw/cancel") => {
            let id = body
                .get("request_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let reason = body
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("no reason given");
            match token {
                Some(t) => guard.cancel_withdrawal(t, id, reason),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        ("GET", "/v1/balance/withdrawals") => {
            let authed = token
                .map(|t| guard.admin_token_ref() == Some(t))
                .unwrap_or(false);
            if !authed {
                return (
                    403,
                    json!({"error":{"code":"forbidden",
                        "message":"the payout queue names agents and destinations; operator token required"}}),
                );
            }
            Ok(guard.pending_withdrawals())
        }
        // Where a buyer with no crypto goes to fund a balance.
        ("GET", p) if p.starts_with("/v1/onramp") => {
            let params = parse_url_params(path);
            let currency = params
                .get("currency")
                .cloned()
                .unwrap_or_else(|| "EUR".into());
            let amount = params.get("amount").cloned();
            match token {
                Some(t) => guard.onramp_links(t, &currency, amount.as_deref()),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        // The agent's own deposit address, for funding it directly.
        ("GET", "/v1/balance/address") => match token {
            Some(t) => guard
                .agent_by_token(t)
                .map(|a| a.identity.did().to_string())
                .and_then(|did| {
                    guard.deposit_address_for(&did).map(|addr| {
                        json!({
                            "agent_did": did,
                            "deposit_address": addr,
                            "currency": guard.custody().currency,
                            "note": "Send the settlement token here, or use the deposit contract \
with your agent id. Either way the node credits it after enough confirmations.",
                        })
                    })
                }),
            None => Err(Error::Unauthorized("missing bearer token".into())),
        },

        // Public on purpose: a reserves attestation nobody can read is
        // not a reserves attestation.
        ("GET", "/v1/reserves") => Ok(guard.reserves()),

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
        // Public on purpose: a tamper-evidence claim nobody can check
        // is a claim, not evidence.
        ("GET", "/v1/audit/verify") => Ok(guard.verify_spine()),
        // Public: a node that will not say what it speaks is a node you
        // have to guess about.
        ("GET", "/v1/conformance") => Ok(guard.conformance()),
        (m, p)
            if m == "POST"
                && p.starts_with("/v1/contract/")
                && p.ends_with("/withdraw-consent") =>
        {
            let id = p
                .trim_start_matches("/v1/contract/")
                .trim_end_matches("/withdraw-consent");
            match token {
                Some(t) => guard.withdraw_consent(t, id),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
        ("POST", "/v1/policy") => match token {
            Some(t) => guard.add_policy(t, &body),
            None => Err(Error::Unauthorized("missing bearer token".into())),
        },
        ("POST", "/v1/delegation") => {
            let chain = body.get("chain").cloned().unwrap_or(body.clone());
            match token {
                Some(t) => guard.register_delegation(t, &chain),
                None => Err(Error::Unauthorized("missing bearer token".into())),
            }
        }
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
            let uri = body
                .get("deliverable_uri")
                .or_else(|| body.get("uri"))
                .and_then(|v| v.as_str());
            let artifact = crate::artifact::Artifact::parse(&body);
            match token {
                Some(t) => guard.remedy(t, id, hash, uri, artifact),
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
        ("GET", p) if p.starts_with("/v1/capability/") => {
            let id = percent_decode(p.trim_start_matches("/v1/capability/"));
            guard.public_capability(&id)
        }
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
        Err(e) => error_response(&e),
    }
}

/// One error, one status, one code - wherever it is answered from.
///
/// Extracted when the gateway became a second place that answers
/// errors. Two copies of this table would drift, and the drift would
/// show up as the same failure returning 400 on one path and 401 on
/// another, which is worse than either being wrong consistently.
pub fn error_response(e: &Error) -> (u16, Value) {
    let code = match e {
        Error::BadSignature => "bad_signature",
        Error::Unauthorized(_) => "unauthorized",
        Error::SandboxBusy => "sandbox_busy",
        Error::AutonomyViolation(_) => "budget_exceeded",
        Error::UnknownContract(_) => "contract_not_found",
        Error::InvalidTransition { .. } => "invalid_transition",
        Error::EscrowViolation(_) => "escrow_violation",
        _ => "invalid_request",
    };
    // Status classes, not a blanket 400: clients (and the OpenAPI
    // contract) distinguish "you are not allowed" from "your request was
    // malformed" from "it does not exist".
    let status = match e {
        Error::Unauthorized(_) => 401,
        Error::SandboxBusy => 429,
        Error::UnknownContract(_) => 404,
        _ => 400,
    };
    (
        status,
        json!({ "error": { "code": code, "message": e.to_string() } }),
    )
}

/// Parse `/v1/cloud/projects/{project}/{kind}/{key...}` without ever
/// interpreting the key as a filesystem path.
fn cloud_route<'a>(path: &'a str, kind: &str) -> Option<(&'a str, String)> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.len() < 6
        || parts[0] != "v1"
        || parts[1] != "cloud"
        || parts[2] != "projects"
        || parts[4] != kind
    {
        return None;
    }
    let key = parts[5..]
        .iter()
        .map(|part| percent_decode(part))
        .collect::<Vec<_>>()
        .join("/");
    Some((parts[3], key))
}

/// Function deployment and activation routes deliberately do not accept
/// additional path components; the function name is an identifier, not a path.
fn cloud_function_route(path: &str, activation: bool) -> Option<(&str, &str)> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let expected = if activation { 7 } else { 6 };
    if parts.len() != expected
        || parts[0] != "v1"
        || parts[1] != "cloud"
        || parts[2] != "projects"
        || parts[4] != "functions"
        || (activation && parts[6] != "activate")
    {
        return None;
    }
    Some((parts[3], parts[5]))
}

fn cloud_function_action<'a>(path: &'a str, action: &str) -> Option<(&'a str, &'a str)> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.len() != 7
        || parts[0] != "v1"
        || parts[1] != "cloud"
        || parts[2] != "projects"
        || parts[4] != "functions"
        || parts[6] != action
    {
        return None;
    }
    Some((parts[3], parts[5]))
}

fn cloud_egress_route(path: &str) -> Option<&str> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.len() == 5
        && parts[0] == "v1"
        && parts[1] == "cloud"
        && parts[2] == "projects"
        && parts[4] == "egress"
    {
        Some(parts[3])
    } else {
        None
    }
}

fn cloud_schedule_route(path: &str) -> Option<(&str, Option<&str>)> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if (parts.len() == 5 || parts.len() == 6)
        && parts[0] == "v1"
        && parts[1] == "cloud"
        && parts[2] == "projects"
        && parts[4] == "schedules"
    {
        Some((parts[3], parts.get(5).copied()))
    } else {
        None
    }
}

fn cloud_function_http_route(path: &str) -> Option<(&str, &str, &str)> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.len() == 7
        && parts[0] == "v1"
        && parts[1] == "cloud"
        && parts[2] == "projects"
        && parts[4] == "functions"
        && matches!(parts[6], "http" | "tokens")
    {
        Some((parts[3], parts[5], parts[6]))
    } else {
        None
    }
}

fn public_function_route(path: &str) -> Option<(&str, &str, String)> {
    let clean = path.split('?').next().unwrap_or(path);
    let parts: Vec<&str> = clean.trim_matches('/').split('/').collect();
    if parts.len() >= 3 && parts[0] == "functions" {
        Some((parts[1], parts[2], parts[3..].join("/")))
    } else {
        None
    }
}

fn verify_function_token(secret: Option<&str>, token: &str, project_id: &str, name: &str) -> bool {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    let Some(secret) = secret else { return false };
    let Some((encoded, signature)) = token.split_once('.') else {
        return false;
    };
    let Ok(signature) = hex::decode(signature) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(encoded.as_bytes());
    if mac.verify_slice(&signature).is_err() {
        return false;
    }
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded) else {
        return false;
    };
    let Ok(claims) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    claims.get("project_id").and_then(Value::as_str) == Some(project_id)
        && claims.get("function").and_then(Value::as_str) == Some(name)
        && claims
            .get("exp")
            .and_then(Value::as_u64)
            .is_some_and(|exp| exp >= now_unix())
}

fn capability_text(value: &Value, field: &str) -> Result<Vec<u8>> {
    let value = value
        .get(field)
        .ok_or_else(|| Error::Other(format!("missing {field}")))?;
    match value {
        Value::String(text) => Ok(text.as_bytes().to_vec()),
        other => serde_json::to_vec(other).map_err(|e| Error::Other(e.to_string())),
    }
}

fn execute_function_capability(
    root: &std::path::Path,
    project_id: &str,
    request: &Value,
) -> Result<Value> {
    use base64::Engine;
    let kind = request
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Other("missing capability kind".into()))?;
    let args = request.get("args").unwrap_or(&Value::Null);
    let key = || {
        args.get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Other("missing capability key".into()))
    };
    let mut store = crate::cloud::ProjectStore::open(root, project_id)?;
    match kind {
        "kv.get" => Ok(match store.get_kv(key()?, now_unix())? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())),
            None => Value::Null,
        }),
        "kv.put" => {
            let bytes = capability_text(args, "value")?;
            let expires_at = args.get("expires_at").and_then(Value::as_u64);
            store.put_kv(key()?, &bytes, expires_at, now_unix())?;
            Ok(json!({ "ok": true }))
        }
        "objects.get" => Ok(match store.get_object(key()?)? {
            Some(object) => json!({
                "content_base64": base64::engine::general_purpose::STANDARD.encode(object.content),
                "media_type": object.media_type,
                "digest": object.digest
            }),
            None => Value::Null,
        }),
        "objects.put" => {
            let content = capability_text(args, "content")?;
            let media_type = args
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream");
            let digest = store.put_object(key()?, &content, media_type, now_unix())?;
            Ok(json!({ "ok": true, "digest": digest }))
        }
        "db.query" | "db.execute" => {
            let sql = args.get("sql").and_then(Value::as_str).unwrap_or("");
            let params = args
                .get("params")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let result = if kind == "db.query" {
                store.database_query(sql, &params)?
            } else {
                store.database_execute(sql, &params)?
            };
            serde_json::to_value(result).map_err(|e| Error::Other(e.to_string()))
        }
        "http.request" => execute_function_http(&store, args),
        _ => Err(Error::Other("unknown function capability".into())),
    }
}

fn execute_function_http(store: &crate::cloud::ProjectStore, args: &Value) -> Result<Value> {
    let method = args.get("method").and_then(Value::as_str).unwrap_or("GET");
    if !matches!(method, "GET" | "POST") {
        return Err(Error::Other(
            "HTTP capability only supports GET and POST".into(),
        ));
    }
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Other("missing HTTP URL".into()))?;
    crate::delivery::validate_webhook_url(url)?;
    let authority = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or("")
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    let host = authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority)
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if !store.egress_hosts()?.iter().any(|allowed| allowed == &host) {
        return Err(Error::Unauthorized(format!(
            "HTTP host is not in project egress allowlist: {host}"
        )));
    }
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(crate::cloud::FUNCTION_HTTP_TIMEOUT))
        .max_redirects(0)
        .build()
        .new_agent();
    let mut safe_headers = Vec::new();
    if let Some(headers) = args.get("headers").and_then(Value::as_object) {
        for (name, value) in headers {
            let lower = name.to_ascii_lowercase();
            if !matches!(
                lower.as_str(),
                "accept" | "content-type" | "cookie" | "user-agent"
            ) {
                return Err(Error::Other(format!("HTTP header is not allowed: {name}")));
            }
            let value = value
                .as_str()
                .ok_or_else(|| Error::Other("HTTP header values must be strings".into()))?;
            if value.contains(['\r', '\n']) {
                return Err(Error::Other("invalid HTTP header value".into()));
            }
            safe_headers.push((name.clone(), value.to_string()));
        }
    }
    // DNS is validated again immediately before the network operation to
    // narrow the rebinding window; redirects remain disabled.
    crate::delivery::validate_webhook_url(url)?;
    let sent = if method == "POST" {
        let body = args
            .get("body")
            .map(|v| {
                if let Some(s) = v.as_str() {
                    s.to_string()
                } else {
                    v.to_string()
                }
            })
            .unwrap_or_default();
        let mut request = agent.post(url);
        for (name, value) in &safe_headers {
            request = request.header(name, value);
        }
        request.send(body)
    } else {
        let mut request = agent.get(url);
        for (name, value) in &safe_headers {
            request = request.header(name, value);
        }
        request.call()
    };
    let mut response = sent.map_err(|e| Error::Other(format!("HTTP capability failed: {e}")))?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = response
        .body_mut()
        .read_to_vec()
        .map_err(|e| Error::Other(format!("HTTP response failed: {e}")))?;
    if bytes.len() > crate::cloud::MAX_FUNCTION_HTTP_RESPONSE_BYTES {
        return Err(Error::Other("HTTP response exceeds 3 MiB".into()));
    }
    Ok(
        json!({ "status": status, "content_type": content_type, "body": String::from_utf8_lossy(&bytes) }),
    )
}

fn cloud_function_version_route(path: &str) -> Option<(&str, &str, u64)> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.len() != 8
        || parts[0] != "v1"
        || parts[1] != "cloud"
        || parts[2] != "projects"
        || parts[4] != "functions"
        || parts[6] != "versions"
    {
        return None;
    }
    let version = parts[7].parse().ok()?;
    Some((parts[3], parts[5], version))
}

fn cloud_database_action(path: &str) -> Option<(&str, &str)> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.len() != 6
        || parts[0] != "v1"
        || parts[1] != "cloud"
        || parts[2] != "projects"
        || parts[4] != "database"
        || !matches!(parts[5], "query" | "execute")
    {
        return None;
    }
    Some((parts[3], parts[5]))
}

fn cloud_realtime_token_route(path: &str) -> Option<&str> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.len() == 6
        && parts[0] == "v1"
        && parts[1] == "cloud"
        && parts[2] == "projects"
        && parts[4] == "realtime"
        && parts[5] == "tokens"
    {
        Some(parts[3])
    } else {
        None
    }
}

fn invoke_function_sandbox(url: &str, token: &str, payload: &Value) -> Result<Value> {
    let endpoint = format!("{}/invoke", url.trim_end_matches('/'));
    let mut response = ureq::post(&endpoint)
        .header("Authorization", &format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .send(payload.to_string())
        .map_err(|e| match e {
            ureq::Error::StatusCode(429) => Error::SandboxBusy,
            _ => Error::Other(format!("function sandbox request failed: {e}")),
        })?;
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| Error::Other(format!("function sandbox response failed: {e}")))?;
    let body: Value = serde_json::from_str(&text)
        .map_err(|e| Error::Other(format!("invalid function sandbox response: {e}")))?;
    body.get("result")
        .cloned()
        .ok_or_else(|| Error::Other("function sandbox returned no result".into()))
}

/// Execute schedules due at `now`. Preparation and bookkeeping briefly hold
/// the protocol lock; untrusted execution and all capability I/O do not.
pub fn run_due_function_schedules(state: &Arc<Mutex<NodeState>>, now: u64) -> usize {
    let prepared = {
        let guard = match state.lock() {
            Ok(g) => g,
            Err(_) => return 0,
        };
        let mut jobs = Vec::new();
        for project_id in guard.cloud_projects.keys() {
            let Ok(store) = crate::cloud::ProjectStore::open(&guard.cloud_root, project_id) else {
                continue;
            };
            let Ok(schedules) = store.due_schedules(now) else {
                continue;
            };
            for schedule in schedules {
                if let Ok(invocation) = guard.cloud_prepare_public_invocation(
                    project_id,
                    &schedule.function,
                    schedule.request.clone(),
                ) {
                    jobs.push((project_id.clone(), schedule.id, invocation));
                }
            }
        }
        jobs
    };
    let count = prepared.len();
    for (project_id, schedule_id, (url, token, payload, _, _)) in prepared {
        let status = match invoke_function_sandbox(&url, &token, &payload) {
            Ok(_) => "ok",
            Err(_) => "error",
        };
        if let Ok(guard) = state.lock() {
            if let Ok(mut store) = crate::cloud::ProjectStore::open(&guard.cloud_root, &project_id)
            {
                let _ = store.finish_schedule(&schedule_id, now, status);
            }
        }
    }
    count
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

/// Render the contract's `input` as a brief the judge can read.
///
/// This is what was ORDERED. Until it existed, a judge asked to check
/// "the output matches the source image" or "the translation is
/// faithful to the original" received only the answer, never the
/// question, and had no honest option other than `inconclusive`.
///
/// Two things it deliberately does:
///
///  - it strips embedded blobs. An input can legitimately carry a
///    base64 source file, and pasting a megabyte of it into a prompt
///    would push the criteria out of the window it was meant to serve.
///    The shape is kept, the payload is not.
///  - it returns `None` for an empty input rather than `"{}"`, so the
///    prompt says nothing instead of saying "the brief was blank".
fn brief_of(terms: &Terms) -> Option<String> {
    fn strip(v: &Value) -> Value {
        match v {
            // Long opaque strings are payloads, not instructions.
            Value::String(s) if s.len() > 512 => {
                Value::String(format!("[{} characters, elided]", s.len()))
            }
            Value::Array(a) => Value::Array(a.iter().map(strip).collect()),
            Value::Object(o) => {
                Value::Object(o.iter().map(|(k, v)| (k.clone(), strip(v))).collect())
            }
            other => other.clone(),
        }
    }

    let empty = match &terms.input {
        Value::Null => true,
        Value::Object(o) => o.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::String(s) => s.trim().is_empty(),
        _ => false,
    };
    if empty {
        return None;
    }
    let mut brief = serde_json::to_string_pretty(&strip(&terms.input)).ok()?;
    // The expected deliverable is part of the order too.
    if !matches!(terms.deliverable, Value::Null) {
        if let Ok(d) = serde_json::to_string(&strip(&terms.deliverable)) {
            if d != "null" && d != "{}" {
                brief.push_str(&format!("\n\nExpected deliverable: {d}"));
            }
        }
    }
    Some(brief)
}

/// Does this look like something a payout can actually be sent to?
///
/// The node settles in an ERC-20 stablecoin, so a destination is an
/// EVM address. Checking the shape is not checking that the address
/// exists or is controlled by the agent - nothing off chain can - but
/// it does stop the failure that was reachable before: an empty string,
/// a DID pasted into the wrong field, or a truncated address, all of
/// which send money nowhere in a way nobody notices until it is gone.
fn is_payout_destination(value: &str) -> bool {
    match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => hex.len() == 40 && hex.chars().all(|c| c.is_ascii_hexdigit()),
        None => false,
    }
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
    fn sandbox_saturation_maps_to_retryable_429() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let sandbox_url = format!("http://{}", listener.local_addr().unwrap());
        let sandbox = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            socket.read(&mut request).unwrap();
            let body = r#"{"error":{"code":"sandbox_busy","message":"sandbox is busy"}}"#;
            write!(
                socket,
                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let error = invoke_function_sandbox(&sandbox_url, "test", &json!({})).unwrap_err();
        sandbox.join().unwrap();
        assert_eq!(error, Error::SandboxBusy);
        let (status, body) = error_response(&error);
        assert_eq!(status, 429);
        assert_eq!(body["error"]["code"], "sandbox_busy");
    }

    /// Client, provider, and a signed contract that requires escrow.
    /// Returns (contract id, client token, provider token).
    fn signed_contract(arc: &Arc<Mutex<NodeState>>) -> (String, String, String) {
        let (_, client_tok) = arc.lock().unwrap().create_identity();
        let (provider_did, provider_tok) = arc.lock().unwrap().create_identity();
        let now = crate::message::now_unix();
        let body = json!({
            "provider": provider_did, "capability_id": "cap:img",
            "terms": {
                "input": {}, "deliverable": {}, "acceptance_criteria": ["ok"],
                "deadline": now + 3600,
                // Priced in USDC on purpose: the escrow used to
                // hardcode EUR, and a fixture in EUR could never catch it.
                "price": { "amount": 0.2, "currency": "USDC", "model": "fixed", "cap": 1.0 },
                "autonomy": "propose", "confidentiality": null
            },
            "escrow": true
        });
        let (status, out) = route(
            arc,
            "POST",
            "/v1/contract/propose",
            body.to_string().as_bytes(),
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(status, 200, "propose failed: {out}");
        let id = out["contract_id"].as_str().unwrap().to_string();
        let (status, out) = route(
            arc,
            "POST",
            &format!("/v1/contract/{id}/accept"),
            b"{}",
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_eq!(status, 200, "accept failed: {out}");
        (id, client_tok, provider_tok)
    }

    #[test]
    fn verifying_the_spine_twice_only_walks_it_once() {
        // The endpoint is public, unauthenticated, and O(chain): it
        // recomputes every hash on the spine. Measured live at roughly
        // three microseconds an event, which is 25 ms at eight thousand
        // and about three SECONDS at a million. Uncached, a stranger
        // could pin a core to it in a loop.
        let arc = state();
        {
            let mut g = arc.lock().unwrap();
            for _ in 0..5 {
                g.record("ctr.propose", json!({ "contract_id": "c" }));
            }
        }
        let first = arc.lock().unwrap().verify_spine();
        assert_eq!(first["intact"], true);
        assert_eq!(first["checked_ms_ago"], 0, "computed, not served");

        // Nothing has been appended, so the answer cannot have changed:
        // this is not a stale reply, it is the current one.
        let second = arc.lock().unwrap().verify_spine();
        assert_eq!(second["intact"], true);
        assert_eq!(second["checked_at_seq"], first["checked_at_seq"]);
        assert_eq!(
            second["links_verified"], first["links_verified"],
            "same answer, not recomputed"
        );

        // Every served answer says which height it was taken at, so a
        // caller never has to guess how current it is.
        assert!(second["checked_at_seq"].is_u64());
        assert!(second["checked_ms_ago"].is_u64());
    }

    #[test]
    fn a_grown_spine_is_re_verified_once_the_backoff_has_passed() {
        // The cache must not outlive the truth it describes. It is
        // allowed to be briefly behind - that is the whole point, and
        // the reply says so - but once the backoff has elapsed and the
        // chain has moved, it walks it again.
        let arc = state();
        {
            let mut g = arc.lock().unwrap();
            g.record("ctr.propose", json!({ "contract_id": "c" }));
        }
        let before = arc.lock().unwrap().verify_spine();
        {
            let mut g = arc.lock().unwrap();
            for _ in 0..4 {
                g.record("ctr.accept", json!({ "contract_id": "c" }));
            }
            // Within the backoff the previous answer still stands, and
            // it is honest about which height it was taken at.
            let stale = g.verify_spine();
            assert_eq!(stale["checked_at_seq"], before["checked_at_seq"]);
            assert_eq!(stale["events_total"], before["events_total"]);

            // Age the cache rather than sleeping through it.
            if let Some(c) = g.spine_check.as_mut() {
                c.at -= std::time::Duration::from_secs(30);
            }
        }
        let after = arc.lock().unwrap().verify_spine();
        assert!(
            after["checked_at_seq"].as_u64().unwrap() > before["checked_at_seq"].as_u64().unwrap(),
            "the chain grew and the backoff passed: {before} then {after}"
        );
        assert_eq!(after["events_total"], 5);
        assert_eq!(after["intact"], true);
    }

    #[test]
    fn expiry_defaults_to_a_dry_run_and_changes_nothing() {
        // A bulk state change over every contract on the node is the one
        // call where saying nothing must mean "show me first".
        let arc = state();
        arc.lock().unwrap().set_admin_token("adm");
        let (id, _, _) = signed_contract(&arc);
        arc.lock()
            .unwrap()
            .contracts
            .get_mut(&id)
            .unwrap()
            .terms
            .deadline = 1;

        let (status, out) = route(&arc, "POST", "/v1/admin/expire", b"{}", Some("Bearer adm"));
        assert_eq!(status, 200, "{out}");
        assert_eq!(out["dry_run"], true);
        assert_eq!(out["cancelled"], 1);
        assert_eq!(
            arc.lock().unwrap().contracts.get(&id).unwrap().state,
            ContractState::Signed,
            "a dry run must not move the contract"
        );
    }

    #[test]
    fn expiry_closes_a_contract_whose_deadline_has_passed() {
        let arc = state();
        arc.lock().unwrap().set_admin_token("adm");
        let (id, _, _) = signed_contract(&arc);
        arc.lock()
            .unwrap()
            .contracts
            .get_mut(&id)
            .unwrap()
            .terms
            .deadline = 1;

        let (status, out) = route(
            &arc,
            "POST",
            "/v1/admin/expire",
            br#"{"dry_run":false}"#,
            Some("Bearer adm"),
        );
        assert_eq!(status, 200, "{out}");
        assert_eq!(out["cancelled"], 1);
        assert_eq!(out["contracts"][0]["reason"], "deadline passed");
        assert_eq!(
            arc.lock().unwrap().contracts.get(&id).unwrap().state,
            ContractState::Cancelled
        );
        // Closed, not erased: the cancellation is on the spine, and the
        // spine still verifies. Deleting the deal instead would remove
        // the only property this node sells.
        let mut guard = arc.lock().unwrap();
        let kinds: Vec<String> = guard
            .storage
            .events_after(0, 500)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&"ctr.cancel".to_string()));
        assert!(kinds.contains(&"ctr.propose".to_string()), "history kept");
        assert_eq!(guard.verify_spine()["intact"], true);
    }

    #[test]
    fn expiry_leaves_a_live_contract_alone() {
        let arc = state();
        arc.lock().unwrap().set_admin_token("adm");
        let (id, _, _) = signed_contract(&arc);
        // Deadline an hour out, created just now: neither rule applies.
        let (status, out) = route(
            &arc,
            "POST",
            "/v1/admin/expire",
            br#"{"dry_run":false}"#,
            Some("Bearer adm"),
        );
        assert_eq!(status, 200, "{out}");
        assert_eq!(out["cancelled"], 0);
        assert_eq!(
            arc.lock().unwrap().contracts.get(&id).unwrap().state,
            ContractState::Signed
        );
    }

    #[test]
    fn idleness_is_measured_from_the_last_move_not_from_creation() {
        // The trap: a contract raised six hours ago and signed one
        // minute ago is a live negotiation. Judging it by age since
        // creation kills it mid-deal. The spine says when it last
        // moved, and propose/accept above are both on the spine.
        let arc = state();
        arc.lock().unwrap().set_admin_token("adm");
        let (id, _, _) = signed_contract(&arc);
        {
            let mut guard = arc.lock().unwrap();
            let c = guard.contracts.get_mut(&id).unwrap();
            c.created_at = 1; // ancient
            c.terms.deadline = crate::message::now_unix() + 3600;
        }
        let (status, out) = route(
            &arc,
            "POST",
            "/v1/admin/expire",
            br#"{"dry_run":false,"max_idle_seconds":60}"#,
            Some("Bearer adm"),
        );
        assert_eq!(status, 200, "{out}");
        assert_eq!(out["cancelled"], 0, "it moved a moment ago: {out}");
        assert_eq!(
            arc.lock().unwrap().contracts.get(&id).unwrap().state,
            ContractState::Signed
        );
    }

    #[test]
    fn a_delivered_contract_is_paid_out_not_cancelled() {
        // The exploit this closes: if silence refunded the buyer, any
        // buyer could take delivery, say nothing for a day, and keep
        // both the work and the money. Silence favours the side that
        // actually did something.
        let arc = state();
        arc.lock().unwrap().set_admin_token("adm");
        let (id, client_tok, provider_tok) = signed_contract(&arc);
        let body = json!({ "contract_id": id, "amount": "0.2" });
        let (status, out) = route(
            &arc,
            "POST",
            "/v1/escrow/park",
            body.to_string().as_bytes(),
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(status, 200, "park failed: {out}");
        let (status, out) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/start"),
            b"{}",
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_eq!(status, 200, "start failed: {out}");
        let body = json!({
            "deliverable_hash": format!("sha256:{}", crate::sha256_hex(b"work")),
            "artifact": { "media_type": "text/plain", "encoding": "utf8", "content": "work" }
        });
        let (status, out) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/deliver"),
            body.to_string().as_bytes(),
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_eq!(status, 200, "deliver failed: {out}");

        // The buyer goes quiet: push the deadline into the past.
        arc.lock()
            .unwrap()
            .contracts
            .get_mut(&id)
            .unwrap()
            .terms
            .deadline = 1;

        let (status, out) = route(
            &arc,
            "POST",
            "/v1/admin/expire",
            br#"{"dry_run":false}"#,
            Some("Bearer adm"),
        );
        assert_eq!(status, 200, "{out}");
        assert_eq!(out["cancelled"], 0, "delivered work is never cancelled");
        assert_eq!(out["auto_accepted"], 1, "{out}");
        assert_eq!(
            arc.lock().unwrap().contracts.get(&id).unwrap().state,
            ContractState::Accepted
        );
        // And the spine says the node did it, not the buyer. Replaying
        // the chain must never show a buyer accepting something it
        // never looked at.
        let guard = arc.lock().unwrap();
        let kinds: Vec<String> = guard
            .storage
            .events_after(0, 500)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&"exe.accept.auto".to_string()), "{kinds:?}");
        assert!(kinds.contains(&"pay.released".to_string()), "{kinds:?}");
    }

    #[test]
    fn a_failed_auto_acceptance_writes_nothing_to_the_spine() {
        // The bug this pins, which shipped and was visible on the public
        // feed: the annotation was written BEFORE the acceptance it
        // described. Five deliveries whose escrow had not survived then
        // failed inside accept_delivery, and the chain was left
        // asserting an auto-acceptance that never happened, immediately
        // followed by the cancellation - two events, one of them false.
        let arc = state();
        arc.lock().unwrap().set_admin_token("adm");
        let (id, _, _) = signed_contract(&arc);
        {
            let mut guard = arc.lock().unwrap();
            let c = guard.contracts.get_mut(&id).unwrap();
            c.state = ContractState::Delivered;
            c.terms.deadline = 1;
            // No escrow was ever parked: settlement cannot succeed.
            guard.escrows.remove(&id);
        }
        let (status, out) = route(
            &arc,
            "POST",
            "/v1/admin/expire",
            br#"{"dry_run":false}"#,
            Some("Bearer adm"),
        );
        assert_eq!(status, 200, "{out}");
        assert_eq!(out["auto_accepted"], 0);
        assert_eq!(out["cancelled"], 1, "closed as unsettleable: {out}");

        let guard = arc.lock().unwrap();
        let kinds: Vec<String> = guard
            .storage
            .events_after(0, 500)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(
            !kinds.contains(&"exe.accept.auto".to_string()),
            "an acceptance that failed must leave no trace claiming it happened: {kinds:?}"
        );
        assert!(kinds.contains(&"ctr.cancel".to_string()));
    }

    #[test]
    fn a_delivery_whose_escrow_survives_is_never_closed_as_unsettleable() {
        // The narrow escape hatch must stay narrow. If an escrow record
        // exists, the deal is settleable and closing it would cancel
        // work somebody received - the very thing the state machine
        // refuses. Only the absence of any money makes it safe.
        let arc = state();
        let (id, client_tok, provider_tok) = signed_contract(&arc);
        let body = json!({ "contract_id": id, "amount": "0.2" });
        let (status, _) = route(
            &arc,
            "POST",
            "/v1/escrow/park",
            body.to_string().as_bytes(),
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(status, 200);
        let _ = provider_tok;
        arc.lock().unwrap().contracts.get_mut(&id).unwrap().state = ContractState::Delivered;

        let err = arc.lock().unwrap().close_unsettleable(&id);
        assert!(err.is_err(), "an escrow that exists must block this path");
        assert_eq!(
            arc.lock().unwrap().contracts.get(&id).unwrap().state,
            ContractState::Delivered
        );
    }

    #[test]
    fn delivered_work_is_left_alone_when_auto_acceptance_is_off() {
        let arc = state();
        arc.lock().unwrap().set_admin_token("adm");
        let (id, _, _) = signed_contract(&arc);
        {
            let mut guard = arc.lock().unwrap();
            let c = guard.contracts.get_mut(&id).unwrap();
            c.state = ContractState::Delivered;
            c.terms.deadline = 1;
        }
        let (status, out) = route(
            &arc,
            "POST",
            "/v1/admin/expire",
            br#"{"dry_run":false,"auto_accept_delivered":false}"#,
            Some("Bearer adm"),
        );
        assert_eq!(status, 200, "{out}");
        assert_eq!(out["cancelled"], 0);
        assert_eq!(out["auto_accepted"], 0);
        assert_eq!(out["left_alone"], 1);
        assert_eq!(
            arc.lock().unwrap().contracts.get(&id).unwrap().state,
            ContractState::Delivered
        );
    }

    #[test]
    fn expiry_requires_the_operator_token() {
        let arc = state();
        arc.lock().unwrap().set_admin_token("adm");
        let (_, _, provider_tok) = signed_contract(&arc);
        let (status, _) = route(
            &arc,
            "POST",
            "/v1/admin/expire",
            br#"{"dry_run":false}"#,
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_eq!(status, 401, "a party must not be able to sweep the node");
    }

    #[test]
    fn a_bare_digest_is_normalised_at_delivery_not_punished_at_verification() {
        // What this cost on the public node: a provider delivered a
        // market briefing with `f84a9db...` instead of `sha256:f84a9db...`,
        // got a 200, and the deal settled nonconforming on a
        // deterministic check. No judge ever saw the work. The verdict
        // is on its public record for a missing prefix.
        let arc = state();
        let (id, client_tok, provider_tok) = signed_contract(&arc);
        let body = json!({ "contract_id": id, "amount": "0.2" });
        let (status, _) = route(
            &arc,
            "POST",
            "/v1/escrow/park",
            body.to_string().as_bytes(),
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(status, 200);
        let (status, _) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/start"),
            b"{}",
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_eq!(status, 200);

        let bare = crate::sha256_hex(b"work");
        let body = json!({
            "deliverable_hash": bare,
            "artifact": { "media_type": "text/plain", "encoding": "utf8", "content": "work" }
        });
        let (status, out) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/deliver"),
            body.to_string().as_bytes(),
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_eq!(status, 200, "a bare digest still delivers: {out}");
        // Stored in the form the deterministic check demands, so the
        // deal is judged on the work rather than on a prefix.
        assert_eq!(
            arc.lock()
                .unwrap()
                .contracts
                .get(&id)
                .unwrap()
                .deliverable_hash
                .as_deref(),
            Some(format!("sha256:{bare}").as_str())
        );
    }

    fn b64(bytes: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn held(media_type: &str, encoding: &str, content: &str) -> crate::storage::DeliverableRecord {
        crate::storage::DeliverableRecord {
            contract_id: "c".into(),
            digest: "sha256:x".into(),
            encoding: encoding.into(),
            media_type: media_type.into(),
            content: content.into(),
            uri: String::new(),
            delivered_at: 0,
        }
    }

    #[test]
    fn a_base64_document_reaches_the_judge_as_text() {
        // Six of the eight inconclusive verdicts on the public node were
        // this: markdown and JSON delivered base64-encoded were handed
        // to the judges as a sentence describing their own size, and the
        // judges answered - correctly - that they could not verify
        // anything. `inconclusive` releases no money, so a good delivery
        // could not be paid for. What matters is whether the artifact is
        // readable, which the media type says; not how it travelled,
        // which the encoding says.
        let doc = "# Brief\n\nTrois secteurs, un classement.";
        let encoded = b64(doc.as_bytes());
        for media in [
            "text/markdown",
            "text/plain; charset=utf-8",
            "application/json",
            "application/ld+json",
            "",
        ] {
            assert_eq!(
                judge_readable_text(&held(media, "base64", &encoded)).as_deref(),
                Some(doc),
                "a {media} artifact must reach the judge as text"
            );
        }
    }

    #[test]
    fn binary_is_still_described_rather_than_pasted_at_a_judge() {
        // A judge shown a megabyte of base64 hallucinates an opinion
        // about it. Images take the separate path that attaches them as
        // images.
        let encoded = b64(&[0u8, 159, 146, 150]);
        for media in ["image/png", "application/pdf", "application/octet-stream"] {
            assert!(
                judge_readable_text(&held(media, "base64", &encoded)).is_none(),
                "{media} must not be pasted at a judge"
            );
        }
        // Undecodable, or empty once decoded, falls back to the
        // description rather than handing a judge an empty document.
        assert!(judge_readable_text(&held("text/plain", "base64", "!!not base64!!")).is_none());
        assert!(judge_readable_text(&held("text/plain", "base64", "")).is_none());
        // Text that never was base64 is passed through by the caller.
        assert!(judge_readable_text(&held("text/plain", "utf8", "hello")).is_none());
    }

    #[test]
    fn a_provider_cannot_start_work_before_escrow_is_parked() {
        // The bug this pins: escrow was only checked at DELIVERY, so a
        // provider could accept, spend real compute producing the
        // artifact, and only then be told nobody had funded the deal.
        // The money it burned getting there was unrecoverable.
        let arc = state();
        let (id, client_tok, provider_tok) = signed_contract(&arc);

        let (status, out) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/start"),
            b"{}",
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_ne!(status, 200, "starting unfunded work must fail");
        let msg = out.to_string();
        assert!(
            msg.contains("escrow is not parked"),
            "and must say why: {msg}"
        );
        assert!(
            msg.contains("do not start work yet"),
            "the error has to be actionable, not merely accurate: {msg}"
        );

        // Fund it, and the same call now succeeds.
        let (status, out) = route(
            &arc,
            "POST",
            "/v1/escrow/park",
            json!({ "contract_id": id, "amount": "0.20" })
                .to_string()
                .as_bytes(),
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(status, 200, "park failed: {out}");
        let (status, _) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/start"),
            b"{}",
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_eq!(status, 200, "funded work must be allowed to start");
    }

    #[test]
    fn settling_without_asking_for_verification_still_records_a_verdict() {
        // Observed in production: buyers go straight from deliver to
        // accept-delivery, never calling /verify. Every settled job then
        // landed in the public record with no ruling at all, and the
        // node's conformance rate stayed undefined while jobs piled up.
        // The deterministic tier costs nothing, so it runs anyway.
        let arc = state();
        let (id, client_tok, provider_tok) = funded_contract(&arc);
        let digest = format!("sha256:{}", crate::sha256_hex(b"the work"));
        route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/deliver"),
            json!({ "deliverable_hash": digest, "content": "the work" })
                .to_string()
                .as_bytes(),
            Some(&format!("Bearer {provider_tok}")),
        );
        // No /verify call at all.
        let (status, _) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/accept-delivery"),
            b"",
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(status, 200);

        let (_, act) = route(&arc, "GET", "/v1/activity", b"", None);
        let job = &act["jobs"][0];
        assert!(
            job["verdict"].is_string(),
            "a settled job must carry a ruling: {act}"
        );
        assert_eq!(job["verdict"], json!("conforms"), "integrity held: {act}");
    }

    #[test]
    fn an_automatic_verdict_records_but_never_blocks_the_buyer() {
        // The buyer has seen the work and says it is fine. A digest the
        // node computed on its own must land in the provider's record
        // without overruling that - blocking here would turn a
        // convenience into a veto nobody asked for.
        let arc = state();
        let (id, client_tok, provider_tok) = funded_contract(&arc);
        // Commit to one thing, hand over nothing: the node holds no
        // artifact, so integrity is unprovable rather than violated.
        route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/deliver"),
            json!({ "deliverable_hash": format!("sha256:{}", "ab".repeat(32)) })
                .to_string()
                .as_bytes(),
            Some(&format!("Bearer {provider_tok}")),
        );
        let (status, out) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/accept-delivery"),
            b"",
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(status, 200, "acceptance must still succeed: {out}");
    }

    #[test]
    fn a_contract_says_whether_it_is_safe_to_start_working() {
        // `state` alone never answered that question, so providers
        // guessed. Now the projection answers it outright.
        let arc = state();
        let (id, client_tok, _provider_tok) = signed_contract(&arc);

        let (status, out) = route(&arc, "GET", &format!("/v1/contract/{id}"), b"", None);
        assert_eq!(status, 200);
        assert_eq!(out["escrow_required"], json!(true));
        assert_eq!(out["escrow_funded"], json!(false));
        assert_eq!(out["provider_may_start"], json!(false));

        route(
            &arc,
            "POST",
            "/v1/escrow/park",
            json!({ "contract_id": id, "amount": "0.20" })
                .to_string()
                .as_bytes(),
            Some(&format!("Bearer {client_tok}")),
        );
        let (_, out) = route(&arc, "GET", &format!("/v1/contract/{id}"), b"", None);
        assert_eq!(out["escrow_funded"], json!(true));
        assert_eq!(out["provider_may_start"], json!(true));
    }

    #[test]
    fn delivery_records_a_retrieval_url_for_artifacts_too_large_to_inline() {
        // A provider that sent `uri` used to have it silently dropped,
        // so it believed it had handed over an artifact the client had
        // no way to fetch.
        let arc = state();
        let (id, client_tok, provider_tok) = signed_contract(&arc);
        route(
            &arc,
            "POST",
            "/v1/escrow/park",
            json!({ "contract_id": id, "amount": "0.20" })
                .to_string()
                .as_bytes(),
            Some(&format!("Bearer {client_tok}")),
        );

        let (status, out) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/deliver"),
            json!({
                "deliverable_hash": "sha256:abc123",
                "uri": "https://cdn.example/artifact.png"
            })
            .to_string()
            .as_bytes(),
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_eq!(status, 200, "deliver failed: {out}");

        let (_, view) = route(&arc, "GET", &format!("/v1/contract/{id}"), b"", None);
        assert_eq!(
            view["contract"]["deliverable_uri"],
            json!("https://cdn.example/artifact.png"),
            "the URL a provider supplied must survive to the client"
        );
        assert_eq!(view["contract"]["deliverable_hash"], json!("sha256:abc123"));
    }

    /// Drive a contract to the point where the provider may deliver.
    fn funded_contract(arc: &Arc<Mutex<NodeState>>) -> (String, String, String) {
        let (id, client_tok, provider_tok) = signed_contract(arc);
        route(
            arc,
            "POST",
            "/v1/escrow/park",
            json!({ "contract_id": id, "amount": "0.20" })
                .to_string()
                .as_bytes(),
            Some(&format!("Bearer {client_tok}")),
        );
        (id, client_tok, provider_tok)
    }

    #[test]
    fn the_node_holds_the_artifact_and_hands_it_to_the_buyer() {
        // The gap this closes: the node kept only a digest, so a buyer
        // whose provider had no out-of-band channel could not fetch what
        // it had just paid for. There was no endpoint at all.
        let arc = state();
        let (id, client_tok, provider_tok) = funded_contract(&arc);
        let digest = format!("sha256:{}", crate::sha256_hex(b"hello"));

        let (status, out) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/deliver"),
            json!({ "deliverable_hash": digest, "content_base64": "aGVsbG8=" })
                .to_string()
                .as_bytes(),
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_eq!(status, 200, "deliver failed: {out}");
        assert_eq!(out["artifact_held"], json!(true));

        let (status, art) = route(
            &arc,
            "GET",
            &format!("/v1/contract/{id}/deliverable"),
            b"",
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(status, 200, "the buyer must be able to fetch it: {art}");
        assert_eq!(art["content"], json!("aGVsbG8="));
        assert_eq!(art["encoding"], json!("base64"));
        assert_eq!(art["digest"], json!(digest));
    }

    #[test]
    fn settlement_reports_the_currency_the_parties_signed() {
        // The escrow used to hardcode EUR, so a contract priced in USDC
        // settled with a receipt naming a currency nobody agreed to.
        // The amount was right and the unit was wrong - the sort of
        // discrepancy an accounting system inherits without questioning.
        let arc = state();
        let (id, client_tok, provider_tok) = funded_contract(&arc);
        let digest = format!("sha256:{}", crate::sha256_hex(b"done"));
        route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/deliver"),
            json!({ "deliverable_hash": digest, "content": "done" })
                .to_string()
                .as_bytes(),
            Some(&format!("Bearer {provider_tok}")),
        );
        let (status, out) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/accept-delivery"),
            b"",
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(status, 200, "settle failed: {out}");
        assert_eq!(
            out["settlement"]["currency"],
            json!("USDC"),
            "the receipt must name the currency the contract was priced in: {out}"
        );
    }

    #[test]
    fn a_delivery_whose_bytes_do_not_match_its_digest_is_refused_immediately() {
        // Checked at delivery, not at verification: the provider learns
        // now, while it can still fix it.
        let arc = state();
        let (id, _client_tok, provider_tok) = funded_contract(&arc);
        let (status, out) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/deliver"),
            json!({ "deliverable_hash": "sha256:deadbeef", "content_base64": "aGVsbG8=" })
                .to_string()
                .as_bytes(),
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_ne!(status, 200);
        assert!(
            out.to_string()
                .contains("does not match the content supplied"),
            "and says exactly what disagreed: {out}"
        );
    }

    #[test]
    fn a_stranger_cannot_fetch_an_artifact() {
        let arc = state();
        let (id, _client_tok, provider_tok) = funded_contract(&arc);
        let digest = format!("sha256:{}", crate::sha256_hex(b"secret work"));
        route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/deliver"),
            json!({ "deliverable_hash": digest, "content": "secret work" })
                .to_string()
                .as_bytes(),
            Some(&format!("Bearer {provider_tok}")),
        );

        let (_, stranger_tok) = arc.lock().unwrap().create_identity();
        let (status, _) = route(
            &arc,
            "GET",
            &format!("/v1/contract/{id}/deliverable"),
            b"",
            Some(&format!("Bearer {stranger_tok}")),
        );
        assert_eq!(
            status, 401,
            "the work belongs to the parties, not the world"
        );

        // ...and the provider, who authored it, still can.
        let (status, _) = route(
            &arc,
            "GET",
            &format!("/v1/contract/{id}/deliverable"),
            b"",
            Some(&format!("Bearer {provider_tok}")),
        );
        assert_eq!(status, 200);
    }

    #[test]
    fn verification_uses_the_held_artifact_when_the_client_supplies_none() {
        // The bug behind a spurious `inconclusive`: with no content and
        // no fallback, the integrity check had nothing to compare and
        // the judge had nothing to read.
        let arc = state();
        let (id, client_tok, provider_tok) = funded_contract(&arc);
        let digest = format!("sha256:{}", crate::sha256_hex(b"the report"));
        route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/deliver"),
            json!({ "deliverable_hash": digest, "content": "the report" })
                .to_string()
                .as_bytes(),
            Some(&format!("Bearer {provider_tok}")),
        );

        let (status, out) = route(
            &arc,
            "POST",
            &format!("/v1/contract/{id}/verify"),
            b"{}",
            Some(&format!("Bearer {client_tok}")),
        );
        assert_eq!(status, 200, "verify failed: {out}");
        // No judge is configured in tests, so the subjective tier is
        // inconclusive - but the deterministic tier must have run and
        // matched, which is what was impossible before.
        let checks = out["checks"].as_array().cloned().unwrap_or_default();
        let integrity = checks
            .iter()
            .find(|c| c["name"] == json!("deliverable_hash_matches"))
            .unwrap_or_else(|| panic!("no integrity check ran; verdict was {out}"));
        assert_eq!(
            integrity["passed"],
            json!(true),
            "the held artifact must satisfy the commitment: {out}"
        );
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

    #[test]
    fn cloud_project_kv_and_function_lifecycle_are_owner_scoped() {
        let arc = state();
        arc.lock()
            .unwrap()
            .set_verifier(Box::new(crate::verifier::MockVerifier::new(
                crate::verifier::Ruling::Conforms,
            )));
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        arc.lock()
            .unwrap()
            .set_cloud_root(std::env::temp_dir().join(format!("gap-cloud-http-{nonce}")));
        let owner = register(&arc);
        let stranger = register(&arc);
        let auth = format!("Bearer {owner}");

        let (status, project) = route(&arc, "POST", "/v1/cloud/projects", b"{}", Some(&auth));
        assert_eq!(status, 200, "{project}");
        let project_id = project["project_id"].as_str().unwrap();

        arc.lock()
            .unwrap()
            .set_realtime_secret("test-realtime-secret");
        let token_path = format!("/v1/cloud/projects/{project_id}/realtime/tokens");
        let token_request = json!({
            "channels": ["contract:demo"],
            "permissions": ["subscribe"],
            "subject": "visitor:42"
        });
        let (status, realtime) = route(
            &arc,
            "POST",
            &token_path,
            token_request.to_string().as_bytes(),
            Some(&auth),
        );
        assert_eq!(status, 200, "{realtime}");
        assert_eq!(realtime["token"].as_str().unwrap().split('.').count(), 2);
        let encoded = realtime["token"]
            .as_str()
            .unwrap()
            .split('.')
            .next()
            .unwrap();
        use base64::Engine;
        let claims: Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(claims["permissions"], json!(["subscribe"]));
        assert_eq!(claims["subject"], "visitor:42");
        assert_eq!(
            route(
                &arc,
                "POST",
                &token_path,
                token_request.to_string().as_bytes(),
                Some(&format!("Bearer {stranger}")),
            )
            .0,
            401
        );

        let value = json!({ "value_base64": b64(b"hello"), "expires_at": now_unix() + 60 });
        let kv_path = format!("/v1/cloud/projects/{project_id}/kv/greeting");
        assert_eq!(
            route(
                &arc,
                "PUT",
                &kv_path,
                value.to_string().as_bytes(),
                Some(&auth)
            )
            .0,
            200
        );
        let (status, stored) = route(&arc, "GET", &kv_path, b"", Some(&auth));
        assert_eq!(status, 200);
        assert_eq!(stored["value_base64"], b64(b"hello"));
        assert_eq!(
            route(
                &arc,
                "GET",
                &kv_path,
                b"",
                Some(&format!("Bearer {stranger}")),
            )
            .0,
            401
        );

        let object_path = format!("/v1/cloud/projects/{project_id}/objects/report.json");
        let object = json!({
            "content_base64": b64(br#"{"ok":true}"#),
            "media_type": "application/json"
        });
        assert_eq!(
            route(
                &arc,
                "PUT",
                &object_path,
                object.to_string().as_bytes(),
                Some(&auth),
            )
            .0,
            200
        );
        let (status, stored_object) = route(&arc, "GET", &object_path, b"", Some(&auth));
        assert_eq!(status, 200);
        assert_eq!(stored_object["content_base64"], b64(br#"{"ok":true}"#));
        assert_eq!(stored_object["media_type"], "application/json");

        let database_path = format!("/v1/cloud/projects/{project_id}/database");
        let create_table = json!({
            "sql": "CREATE TABLE messages(id INTEGER PRIMARY KEY, body TEXT)",
            "params": []
        });
        assert_eq!(
            route(
                &arc,
                "POST",
                &format!("{database_path}/execute"),
                create_table.to_string().as_bytes(),
                Some(&auth),
            )
            .0,
            200
        );
        let insert = json!({
            "sql": "INSERT INTO messages(body) VALUES(?1)",
            "params": ["hello"]
        });
        let (status, inserted) = route(
            &arc,
            "POST",
            &format!("{database_path}/execute"),
            insert.to_string().as_bytes(),
            Some(&auth),
        );
        assert_eq!(status, 200, "{inserted}");
        assert_eq!(inserted["affected_rows"], 1);
        let query = json!({ "sql": "SELECT body FROM messages", "params": [] });
        let (status, queried) = route(
            &arc,
            "POST",
            &format!("{database_path}/query"),
            query.to_string().as_bytes(),
            Some(&auth),
        );
        assert_eq!(status, 200, "{queried}");
        assert_eq!(queried["rows"][0][0], "hello");
        assert_eq!(
            route(
                &arc,
                "POST",
                &format!("{database_path}/query"),
                query.to_string().as_bytes(),
                Some(&format!("Bearer {stranger}")),
            )
            .0,
            401
        );

        let fn_path = format!("/v1/cloud/projects/{project_id}/functions/answer");
        let deploy = json!({ "runtime": "javascript", "source": "() => 42" });
        let (status, version) = route(
            &arc,
            "POST",
            &fn_path,
            deploy.to_string().as_bytes(),
            Some(&auth),
        );
        assert_eq!(status, 200, "{version}");
        assert_eq!(version["ruling"], "approved_with_constraints");
        let activate = json!({ "version": version["version"] });
        assert_eq!(
            route(
                &arc,
                "POST",
                &format!("{fn_path}/activate"),
                activate.to_string().as_bytes(),
                Some(&auth),
            )
            .0,
            200
        );

        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let sandbox_url = format!("http://{}", listener.local_addr().unwrap());
        let sandbox = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let read = socket.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer internal-test"));
            let body = r#"{"result":{"answer":42}}"#;
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        arc.lock()
            .unwrap()
            .set_function_sandbox(&sandbox_url, "internal-test");
        let (status, invoked) = route(
            &arc,
            "POST",
            &format!("{fn_path}/invoke"),
            br#"{"request":{"question":"life"}}"#,
            Some(&auth),
        );
        sandbox.join().unwrap();
        assert_eq!(status, 200, "{invoked}");
        assert_eq!(invoked["result"]["answer"], 42);

        let version_delete = format!("{fn_path}/versions/{}", version["version"]);
        assert_eq!(
            route(&arc, "DELETE", &version_delete, b"", Some(&auth)).0,
            400,
            "the active version must be protected"
        );
        assert_eq!(
            route(
                &arc,
                "DELETE",
                &fn_path,
                b"",
                Some(&format!("Bearer {stranger}")),
            )
            .0,
            401
        );
        let (status, deleted) = route(&arc, "DELETE", &fn_path, b"", Some(&auth));
        assert_eq!(status, 200, "{deleted}");
        assert_eq!(deleted["deleted"], true);
        let (status, deleted_again) = route(&arc, "DELETE", &fn_path, b"", Some(&auth));
        assert_eq!(status, 200, "{deleted_again}");
        assert_eq!(deleted_again["deleted"], false);
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

    /// Eviction must bound memory WITHOUT changing what the node says.
    ///
    /// The three things this asserts are the three ways the previous
    /// attempt at this would have gone wrong silently: a count that
    /// shrinks to the cache size, a contract still in flight thrown out
    /// because it was old, and a settled deal that stops being readable
    /// once it leaves the window.
    #[test]
    fn finished_contracts_are_evicted_but_stay_readable() {
        let arc = state();
        let mut guard = arc.lock().unwrap();

        let client = crate::identity::AgentIdentity::generate();
        let provider = crate::identity::AgentIdentity::generate();
        let terms = Terms {
            input: json!({}),
            deliverable: json!({}),
            acceptance_criteria: vec!["ok".into()],
            deadline: now_unix() + 3600,
            price: Price {
                amount: 1.0,
                currency: "USDC".into(),
                model: "fixed".into(),
                cap: Some(5.0),
            },
            autonomy: "propose".into(),
            confidentiality: None,
            human_review_above: None,
            cooling_off_seconds: None,
        };
        let template =
            Contract::propose(&client, provider.did().clone(), "cap:evict", terms, false);

        // One contract that is still moving, inserted first so that
        // every later insert is a chance to evict it by mistake.
        let live_id = "live-one".to_string();
        let mut live = template.clone();
        live.contract_id = live_id.clone();
        live.state = ContractState::Executing;
        guard.set_contract(live);

        let overflow = TERMINAL_WINDOW + 50;
        let mut first_id = String::new();
        for i in 0..overflow {
            let mut c = template.clone();
            c.contract_id = format!("done-{i}");
            c.state = ContractState::Accepted;
            if i == 0 {
                first_id = c.contract_id.clone();
            }
            guard.persist_contract(&c);
            guard.set_contract(c);
        }

        // The count is history, not occupancy.
        assert_eq!(guard.contracts.total(), overflow as u64 + 1);
        assert!(
            guard.contracts.resident() <= TERMINAL_WINDOW + 1,
            "resident set unbounded: {}",
            guard.contracts.resident()
        );

        // In flight survives regardless of age.
        assert!(
            guard.contracts.get(&live_id).is_some(),
            "a contract still in flight was evicted"
        );

        // The oldest finished one is out of memory...
        assert!(
            guard.contracts.get(&first_id).is_none(),
            "nothing was evicted; the window is not doing anything"
        );
        // ...and still readable, which is the whole promise.
        let recovered = guard
            .contract_for_read(&first_id)
            .expect("evicted contract must still be readable from storage");
        assert_eq!(recovered.contract_id, first_id);
        assert_eq!(recovered.state, ContractState::Accepted);
    }

    /// The legacy jobs shape must survive the change that replaces it.
    ///
    /// The migration is irreversible - it tombstones the old row - so
    /// the failure it has to be pinned against is silent data loss: a
    /// hydrate that only understood the new shape would drop every job
    /// recorded before it, including the 21,290 rebuilt from contracts
    /// on 2026-08-20, and say nothing.
    #[test]
    fn legacy_job_lists_are_migrated_not_dropped() {
        let mut storage = SqliteStorage::open(":memory:").unwrap();
        let agent = "did:gap:legacy";
        let list = vec![
            JobRecord {
                job_ref: "aaaa1111".into(),
                capability_id: "cap:one".into(),
                counterparty_ref: "cccc".into(),
                outcome: "accepted".into(),
                remedied: false,
                verdict: Some("conforms".into()),
                judged_by: None,
                on_time: true,
                at: 100,
                seq: 0,
                amount: Some("1.500000".into()),
                currency: Some("USDC".into()),
            },
            JobRecord {
                job_ref: "bbbb2222".into(),
                capability_id: "cap:two".into(),
                counterparty_ref: "cccc".into(),
                outcome: "accepted".into(),
                remedied: false,
                verdict: None,
                judged_by: None,
                on_time: true,
                at: 200,
                seq: 0,
                amount: None,
                currency: None,
            },
        ];
        storage
            .upsert_state(&crate::storage::StateRecord {
                scope: "jobs".into(),
                key: agent.into(),
                value: serde_json::to_string(&list).unwrap(),
                updated_at: 1,
            })
            .unwrap();

        let state = NodeState::new(Box::new(storage));

        // Loaded, in order, nothing lost.
        let loaded = state.jobs.get(agent).expect("legacy agent history dropped");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].job_ref, "aaaa1111");
        assert_eq!(loaded[1].job_ref, "bbbb2222");

        // Rewritten one row per job...
        let rows = state.storage.list_state("jobs").unwrap();
        let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert!(keys.contains(&"did:gap:legacy/aaaa1111"), "{keys:?}");
        assert!(keys.contains(&"did:gap:legacy/bbbb2222"), "{keys:?}");

        // ...and the list row is gone, or the next boot loads both
        // shapes for ever.
        assert!(
            !keys.contains(&agent),
            "legacy row still present after migration: {keys:?}"
        );
    }

    /// llms.txt must carry the limits, not just the pitch.
    ///
    /// This is the one document written to be read BEFORE a contract is
    /// signed, by something that has no operator to ask. A version of it
    /// that lists the endpoints and omits what does not work would be
    /// misleading by omission, and the omission would be invisible -
    /// nothing errors, the file just quietly sells.
    #[test]
    fn llms_txt_publishes_the_limits_and_the_live_numbers() {
        let arc = state();
        let guard = arc.lock().unwrap();
        let stats = guard.public_stats();
        let txt = crate::ui::llms_txt(
            "https://example.test",
            &guard.node_did().to_string(),
            &stats,
            &[],
        );

        // The limits, each one a thing an agent would otherwise find out
        // the expensive way.
        for must in [
            "WHAT DOES NOT WORK",
            "SELF-DECLARED",
            "ADVISORY",
            "NEVER RUN ON A REAL EVM",
            "regulated activity",
        ] {
            assert!(txt.contains(must), "llms.txt lost its limits: {must}");
        }

        // Generated from live state, so it cannot go stale in the way a
        // checked-in file does.
        assert!(txt.contains(&guard.node_did().to_string()));
        assert!(txt.contains("/v1/audit/verify"), "no way to check it");

        // The rules that are easy to get wrong and expensive to learn.
        assert!(txt.contains("before the work"), "escrow ordering rule gone");
        assert!(txt.contains("sha256:"), "digest format rule gone");
        assert!(txt.contains("LEASE"), "announcement TTL rule gone");
    }

    /// The gateway must refuse before it forwards, not after.
    ///
    /// Everything expensive happens on the far side of these checks: a
    /// call to someone else's paid API, made with a credential this
    /// node holds on their behalf. A gateway that forwards first and
    /// reconciles later is a way to spend a provider's quota for free.
    #[test]
    fn the_gateway_will_not_forward_until_it_has_been_paid() {
        let arc = state();
        let (provider_did, provider_tok) = arc.lock().unwrap().create_identity();
        let (_, client_tok) = arc.lock().unwrap().create_identity();

        // No master key on this node, so registering must be refused
        // outright rather than storing the credential in the clear.
        let body = json!({
            "slug": "acme", "upstream": "https://api.acme.test/v1",
            "capability_id": "cap:acme:search", "amount": "0.010000",
            "currency": "USDC", "auth_value": "sk-secret",
            "acceptance_criteria": ["returns JSON"]
        });
        let refused = arc.lock().unwrap().gateway_register(&provider_tok, &body);
        assert!(
            refused.is_err(),
            "a node with no master key accepted someone else's API key"
        );

        // With a vault, the route registers - and the credential must
        // not appear in what comes back.
        arc.lock().unwrap().vault = Some(crate::vault::Vault::new(&[7u8; 32]));
        let public = arc
            .lock()
            .unwrap()
            .gateway_register(&provider_tok, &body)
            .expect("registration failed with a vault present");
        let rendered = public.to_string();
        assert!(!rendered.contains("sk-secret"), "{rendered}");
        assert!(rendered.contains(&provider_did));

        // An anonymous caller gets a challenge and no contract: there is
        // no party to draft one for.
        let step = arc
            .lock()
            .unwrap()
            .gateway_begin(
                "acme",
                "search",
                "https://node/x402/acme/search",
                None,
                None,
            )
            .unwrap();
        match step {
            crate::gateway::GatewayStep::Challenge(v) => {
                assert_eq!(v["x402Version"], 1);
                assert_eq!(v["gap"]["contract_id"], "");
                // The criteria are readable BEFORE paying.
                assert_eq!(v["gap"]["acceptance_criteria"][0], "returns JSON");
            }
            _ => panic!("an unpaid, unidentified call was allowed through"),
        }

        // An identified caller gets a drafted contract, still a 402.
        let step = arc
            .lock()
            .unwrap()
            .gateway_begin(
                "acme",
                "search",
                "https://node/x402/acme/search",
                Some(&client_tok),
                None,
            )
            .unwrap();
        let cid = match step {
            crate::gateway::GatewayStep::Challenge(v) => {
                let cid = v["gap"]["contract_id"].as_str().unwrap_or("").to_string();
                assert!(!cid.is_empty(), "no contract was drafted for a known agent");
                cid
            }
            _ => panic!("an unfunded call was forwarded"),
        };

        // And presenting that contract UNFUNDED is still refused - this
        // is the check that stands between a stranger and the
        // provider's paid quota.
        let err = arc
            .lock()
            .unwrap()
            .gateway_begin(
                "acme",
                "search",
                "https://node/x402/acme/search",
                Some(&client_tok),
                Some(&cid),
            )
            .unwrap_err();
        assert!(
            matches!(err, Error::EscrowViolation(_)),
            "unfunded contract was accepted: {err:?}"
        );
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
            cooling_off_seconds: None,
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
            cooling_off_seconds: None,
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
            cooling_off_seconds: None,
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
