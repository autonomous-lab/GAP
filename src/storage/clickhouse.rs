//! ClickHouse storage backend — production scale.
//!
//! ClickHouse is an OLAP store, and GAP's event-sourcing design makes
//! that a feature: the event spine is a `MergeTree` table (append-only,
//! massively compressible), and materialized state lives in
//! `ReplacingMergeTree` tables that collapse on the latest version.
//!
//! # Atomicity: the sequencer
//!
//! ClickHouse has no multi-row transactions. Operations that require
//! atomicity (escrow park/release, contract state + event) MUST go
//! through the [`Sequencer`]: a single-process writer that serializes
//! critical sections, writes the event, then confirms. This matches
//! the reality of an escrow agent: one process, one order.
//!
//! # Transport
//!
//! The backend talks to ClickHouse over its HTTP JSON interface via a
//! [`HttpTransport`] trait, so tests can use a mock transport while
//! production uses a real HTTP client (ureq).

use super::{
    validate_event, AnnouncementRecord, ContractRecord, DeliverableRecord, EscrowRecord,
    EventRecord, IdentityRecord, StateRecord, Storage,
};
use crate::error::{Error, Result};
use std::collections::HashMap;
use std::sync::Mutex;

/// Minimal HTTP transport abstraction (mockable).
/// A bound query parameter for ClickHouse's `{name:Type}` placeholders.
#[derive(Debug, Clone)]
pub struct QueryParam {
    pub name: &'static str,
    pub value: String,
}

impl QueryParam {
    pub fn new(name: &'static str, value: impl Into<String>) -> Self {
        Self {
            name,
            value: value.into(),
        }
    }
}

/// Minimal HTTP transport abstraction (mockable).
pub trait HttpTransport: Send {
    /// POST a raw query to ClickHouse, return the raw response body.
    fn post(&self, query: &str) -> Result<String>;

    /// POST a query with bound parameters. Implementations MUST bind
    /// values through ClickHouse's `{name:Type}` placeholder mechanism
    /// (`param_<name>` in the request) — never by string interpolation.
    /// This is the SQL-injection-safe path (audit finding C-02).
    fn post_params(&self, query: &str, params: &[QueryParam]) -> Result<String> {
        // Default: append params as query-string bindings after
        // encoding the query itself.
        let mut url = format!("{}/?query={}", self.base_url(), urlencode(query));
        url.push_str(&insert_settings(query));
        for p in params {
            url.push_str(&format!(
                "&param_{}={}",
                p.name,
                urlencode(&escape_param(&p.value))
            ));
        }
        self.post_url(&url)
    }

    /// POST a query whose DATA travels in the request body.
    ///
    /// The `param_<name>=` mechanism puts every value in the URL, and a
    /// URL has a length limit that nothing in this codebase respected.
    /// An agent's job history is stored as one row per agent, so the
    /// value grew with every job it completed: at roughly 32 KB the
    /// writes started failing with `uri too long`, and the jobs
    /// projection stopped persisting on 2026-08-14 while the node kept
    /// serving a public settled-jobs count that was 78 times too small.
    ///
    /// Values sent this way have no size limit worth naming, and they
    /// are not escaped by ClickHouse's parameter parser either, so the
    /// backslash-doubling that `escape_param` exists for does not apply.
    fn post_body(&self, query: &str, body: &str) -> Result<String>;

    /// The base URL (used by the default `post_params`).
    fn base_url(&self) -> String;

    /// POST to a fully-built URL.
    fn post_url(&self, url: &str) -> Result<String>;
}

/// Protect a parameter value from ClickHouse's own unescaping.
///
/// A `param_x=` binding is not delivered verbatim: ClickHouse parses it
/// with the escaping rules of its text formats, so a backslash is an
/// escape introducer and is consumed. Everything this node stores in
/// `gap_state` is JSON, and JSON escapes quotes as `\"` - which arrived
/// as a bare `"` and turned a valid document into an invalid one.
///
/// Nothing complained. The write returned 200, the row was there, and
/// the value looked almost right. It only failed on the way back, at
/// `serde_json::from_str`, where a record that will not parse is a
/// record that silently disappears: a verdict that vanished from the
/// index took its job page with it, and an agent's public history went
/// on advertising a link to it.
///
/// Measured against a live ClickHouse 24.8, not inferred:
///
/// ```text
/// sent    {"a":"say \"hi\" ok"}
/// naive   {"a":"say "hi" ok"}      <- does not parse
/// escaped {"a":"say \"hi\" ok"}    <- exact round trip
/// ```
///
/// Doubling backslashes is enough and is a no-op on any value that has
/// none, which is why it is applied to every parameter rather than to
/// the ones that happen to look like JSON today.
fn escape_param(value: &str) -> String {
    value.replace('\\', "\\\\")
}

/// Settings appended to INSERT statements, and only to those.
///
/// ClickHouse makes one PART per insert, and this node inserts one row
/// at a time. Under load that was about 1,400 parts a minute across four
/// tables: the merges kept up, the disk did not, and eight minutes of
/// merged-away parts reached 5.5 GB over 8 MiB of live data.
///
/// `async_insert` fixes that at the source - the server collects
/// concurrent inserts and writes ONE part per batch. Measured here: 469
/// parts a minute down to 2.
///
/// What it costs turns on one flag, and the answer is NOT the same for
/// every table.
///
/// The spine waits. `wait_for_async_insert=1` on `gap_events` means the
/// call returns only once the batch is durable. This was learned the
/// expensive way: with the flag off, a ClickHouse restart dropped the
/// buffered batch and seventeen consecutive events - 26993 to 27009 -
/// vanished while the node kept writing past them. The comment that
/// used to sit here reasoned that a crash TRUNCATES, and that a prefix
/// of a hash chain still verifies. That is true only if nothing is
/// written afterwards. What actually happens is a HOLE, the link at
/// 27010 points at a hash that no longer exists, and the chain breaks
/// there permanently. An audit chain cannot be reconstructed by
/// retrying, so it is the one table that pays the latency.
///
/// Everything else does not. Contracts, escrows and state are
/// PROJECTIONS written as upserts: a lost row is overwritten by the next
/// write of the same key, and a rebuild reconstructs them. Losing one
/// costs a stale field for a moment, not evidence.
fn insert_settings(query: &str) -> String {
    let trimmed = query.trim_start();
    let is_insert = trimmed
        .get(..6)
        .map(|s| s.eq_ignore_ascii_case("INSERT"))
        .unwrap_or(false);
    // OFF by default, and this is not a tuning preference.
    //
    // ClickHouse cannot combine async_insert with a PARAMETERISED query:
    // deferring the insert loses the `{name:Type}` bindings and the
    // flush fails with `Code: 456 ... Substitution 'seq' is not set:
    // While executing WaitForAsyncInsert`. Every write this node makes
    // is parameterised, because that is the injection-safe path (audit
    // finding C-02), so async_insert breaks all of them.
    //
    // It was enabled here to stop one part being written per row, and
    // it did - by writing no rows at all. The node kept its events in
    // memory, `record` swallowed the error, and the audit chain stopped
    // persisting while every page still answered. The part problem is
    // solved instead by old_parts_lifetime in
    // deploy/clickhouse/merge-tree.xml, which took the disk from 5.9 GB
    // to under 500 MB without touching correctness.
    //
    // Turning this on again requires sending the values inline rather
    // than as bindings, which is the trade this codebase already
    // refused once.
    if !is_insert || !env_flag("GAP_CLICKHOUSE_ASYNC_INSERT", false) {
        return String::new();
    }
    // The audit spine is the one thing here that cannot be rebuilt.
    let is_spine = trimmed.to_ascii_lowercase().contains("gap_events");
    let wait = if is_spine || env_flag("GAP_CLICKHOUSE_INSERT_WAIT", false) {
        1
    } else {
        0
    };
    // A shorter window on the waiting path: the caller is blocked for
    // it, so it is latency rather than buffering.
    let default_ms = if wait == 1 { 200 } else { 1000 };
    let busy_ms = std::env::var("GAP_CLICKHOUSE_INSERT_WINDOW_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default_ms);
    format!("&async_insert=1&wait_for_async_insert={wait}&async_insert_busy_timeout_ms={busy_ms}")
}

fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => !matches!(v.trim(), "0" | "false" | "no" | ""),
        Err(_) => default,
    }
}

/// A real transport using `ureq`.
pub struct UreqTransport {
    base_url: String,
    /// Credentials, sent as headers rather than embedded in the URL:
    /// a password in a URL ends up in access logs, proxy logs and error
    /// messages. Recent ClickHouse images generate a random password
    /// for `default` when none is configured, so a node with no
    /// credentials is simply refused with AUTHENTICATION_FAILED.
    user: Option<String>,
    password: Option<String>,
}

impl UreqTransport {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            user: None,
            password: None,
        }
    }

    /// Authenticate as `user`. An empty password is still sent, because
    /// ClickHouse distinguishes "no credentials" from "empty password".
    pub fn with_credentials(mut self, user: &str, password: &str) -> Self {
        self.user = Some(user.to_string());
        self.password = Some(password.to_string());
        self
    }

    /// Build the transport from the environment.
    pub fn from_env(base_url: &str) -> Self {
        let t = Self::new(base_url);
        match (
            std::env::var("GAP_CLICKHOUSE_USER").ok(),
            std::env::var("GAP_CLICKHOUSE_PASSWORD").ok(),
        ) {
            (None, None) => t,
            (user, password) => t.with_credentials(
                &user.unwrap_or_else(|| "default".into()),
                &password.unwrap_or_default(),
            ),
        }
    }
}

impl HttpTransport for UreqTransport {
    fn post(&self, query: &str) -> Result<String> {
        let url = format!("{}/?query={}", self.base_url, urlencode(query));
        self.post_url(&url)
    }

    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn post_url(&self, url: &str) -> Result<String> {
        let mut req = ureq::post(url);
        if let (Some(user), Some(password)) = (&self.user, &self.password) {
            req = req
                .header("X-ClickHouse-User", user)
                .header("X-ClickHouse-Key", password);
        }
        let mut resp = req
            .send_empty()
            .map_err(|e| Error::Other(format!("clickhouse request failed: {e}")))?;
        resp.body_mut()
            .read_to_string()
            .map_err(|e| Error::Other(format!("clickhouse response failed: {e}")))
    }

    fn post_body(&self, query: &str, body: &str) -> Result<String> {
        let url = format!("{}/?query={}", self.base_url, urlencode(query));
        let mut req = ureq::post(&url);
        if let (Some(user), Some(password)) = (&self.user, &self.password) {
            req = req
                .header("X-ClickHouse-User", user)
                .header("X-ClickHouse-Key", password);
        }
        let mut resp = req
            .send(body)
            .map_err(|e| Error::Other(format!("clickhouse request failed: {e}")))?;
        resp.body_mut()
            .read_to_string()
            .map_err(|e| Error::Other(format!("clickhouse response failed: {e}")))
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The single-writer sequencer: serializes critical sections.
///
/// `T` is the state protected by the critical section (e.g. escrow
/// balances). The sequencer guarantees: one process, one order, atomic
/// read-modify-write.
pub struct Sequencer<T> {
    inner: Mutex<T>,
}

impl<T> Sequencer<T> {
    pub fn new(initial: T) -> Self {
        Self {
            inner: Mutex::new(initial),
        }
    }

    /// Run a critical section exclusively. Returns the outcome.
    pub fn critical<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut T) -> Result<R>,
    {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| Error::Other("sequencer lock poisoned".into()))?;
        f(&mut guard)
    }
}

/// The ClickHouse table DDL. State tables use ReplacingMergeTree so
/// that upserts collapse to the latest row on merge.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS gap_events (
    seq UInt64,
    kind String,
    at UInt64,
    payload String,
    prev_hash String DEFAULT '',
    hash String DEFAULT ''
) ENGINE = MergeTree ORDER BY seq;
-- Additive for a table that predates the chain. Both are no-ops when
-- the columns already exist.
ALTER TABLE gap_events ADD COLUMN IF NOT EXISTS prev_hash String DEFAULT '';
ALTER TABLE gap_events ADD COLUMN IF NOT EXISTS hash String DEFAULT '';

CREATE TABLE IF NOT EXISTS gap_contracts (
    contract_id String,
    client String,
    provider String,
    capability_id String,
    state String,
    contract_json String,
    updated_at UInt64
) ENGINE = ReplacingMergeTree(updated_at) ORDER BY contract_id;

CREATE TABLE IF NOT EXISTS gap_announcements (
    agent_did String,
    announcement_json String,
    expires_at UInt64
) ENGINE = ReplacingMergeTree(expires_at) ORDER BY agent_did;

CREATE TABLE IF NOT EXISTS gap_identities (
    token String,
    did String,
    seed_hex String,
    created_at UInt64
) ENGINE = ReplacingMergeTree(created_at) ORDER BY token;

CREATE TABLE IF NOT EXISTS gap_escrows (
    contract_id String,
    state String,
    held String,
    currency String,
    updated_at UInt64
) ENGINE = ReplacingMergeTree(updated_at) ORDER BY contract_id;

CREATE TABLE IF NOT EXISTS gap_state (
    scope String,
    key String,
    value String,
    updated_at UInt64
) ENGINE = ReplacingMergeTree(updated_at) ORDER BY (scope, key);

CREATE TABLE IF NOT EXISTS gap_deliverables (
    contract_id String,
    digest String,
    encoding String,
    media_type String,
    content String,
    uri String,
    delivered_at UInt64
) ENGINE = ReplacingMergeTree(delivered_at) ORDER BY contract_id;
"#;

/// What [`ClickHouseStorage::hydrate`] read back from the cluster.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HydrateSummary {
    pub events: usize,
    pub contracts: usize,
    pub announcements: usize,
    pub identities: usize,
    pub escrows: usize,
    pub deliverables: usize,
    pub state: usize,
}

/// ClickHouse-backed storage.
pub struct ClickHouseStorage<T: HttpTransport + Send> {
    transport: T,
    /// In-memory mirror of state (ClickHouse does not serve point
    /// reads in real time; the sequencer keeps the hot state).
    contracts: Mutex<HashMap<String, ContractRecord>>,
    announcements: Mutex<HashMap<String, AnnouncementRecord>>,
    identities: Mutex<HashMap<String, IdentityRecord>>,
    escrows: Mutex<HashMap<String, EscrowRecord>>,
    deliverables: Mutex<HashMap<String, DeliverableRecord>>,
    /// (scope, key) -> record, the mirror behind `list_state`.
    node_state: Mutex<HashMap<(String, String), StateRecord>>,
    events: Mutex<Spine>,
    /// The atomicity gate for escrow-style operations.
    pub sequencer: Sequencer<()>,
}

/// How many recent events stay in memory.
///
/// Measured on the live node at roughly 2.7 KB an event, so this is
/// about 27 MB and it does not move. Everything older is answered from
/// ClickHouse, which is where it has always been.
pub const SPINE_WINDOW: usize = 10_000;

/// The hot end of the audit chain, and the three facts about the whole
/// of it that must not require holding the whole of it.
///
/// The mirror used to be a `Vec` containing every event ever written,
/// because three things read it: the next sequence number came from its
/// maximum, the total came from its length, and the previous hash came
/// from its last element. All three are O(1) facts that were being paid
/// for in O(chain) memory - measured at 1 GB after 400,000 events, on a
/// box with 5.7 GB free and no mechanism that ever gave any of it back.
/// A node that dies of its own history after four days is not a node
/// with a memory leak, it is a node with a deadline.
#[derive(Default)]
struct Spine {
    /// The most recent events, capped at `SPINE_WINDOW`.
    recent: std::collections::VecDeque<EventRecord>,
    /// Highest sequence written. NOT derived from `recent`.
    head: u64,
    /// How many events exist, in ClickHouse, not in memory.
    total: u64,
    /// The hash the next event links to.
    tip: String,
}

impl Spine {
    /// The oldest sequence this mirror can answer for, or 0 when empty.
    fn oldest(&self) -> u64 {
        self.recent.front().map(|e| e.seq).unwrap_or(0)
    }

    fn push(&mut self, rec: EventRecord) {
        self.head = self.head.max(rec.seq);
        self.total += 1;
        self.tip = rec.hash.clone();
        self.recent.push_back(rec);
        while self.recent.len() > SPINE_WINDOW {
            self.recent.pop_front();
        }
    }
}

impl<T: HttpTransport> ClickHouseStorage<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            contracts: Mutex::new(HashMap::new()),
            announcements: Mutex::new(HashMap::new()),
            identities: Mutex::new(HashMap::new()),
            escrows: Mutex::new(HashMap::new()),
            deliverables: Mutex::new(HashMap::new()),
            node_state: Mutex::new(HashMap::new()),
            events: Mutex::new(Spine::default()),
            sequencer: Sequencer::new(()),
        }
    }

    /// Create tables on the cluster.
    /// Create the schema.
    ///
    /// ClickHouse's HTTP interface refuses multi-statement queries
    /// ("Multi-statements are not allowed"), so the DDL is split and
    /// sent one statement at a time. Sending it as one blob failed with
    /// a 400 the first time this ever ran against a real server.
    pub fn migrate(&self) -> Result<()> {
        for statement in Self::statements(DDL) {
            self.transport.post(&statement)?;
        }
        Ok(())
    }

    /// Load persisted state back into the in-memory mirrors.
    ///
    /// The mirrors are what every read goes through, and they start
    /// empty - so without this call ClickHouse was a write-only sink.
    /// Everything was durably stored and nothing was ever read back:
    /// a restart emptied the agent directory, invalidated every bearer
    /// token, and restarted the event sequence at 1 on top of rows that
    /// already existed.
    ///
    /// `FINAL` is required: the tables are `ReplacingMergeTree`, so
    /// without it a row updated twice comes back twice and the older
    /// version can win.
    ///
    /// `output_format_json_quote_64bit_integers=0` is required too -
    /// ClickHouse quotes 64-bit integers in JSON by default, which does
    /// not deserialize into `u64`.
    pub fn hydrate(&self) -> Result<HydrateSummary> {
        let mut summary = HydrateSummary::default();

        // Load only the TAIL of the spine.
        //
        // This used to read every event ever written into memory, which
        // is where the gigabyte went. The three facts the node actually
        // needs about the whole chain - its head, its length, its tip
        // hash - are one cheap aggregate away, and the rest of the
        // chain is answered from ClickHouse when somebody asks for it.
        #[derive(serde::Deserialize)]
        struct SpineHead {
            head: u64,
            total: u64,
        }
        let agg: Vec<SpineHead> =
            self.select("SELECT ifNull(max(seq), 0) AS head, count() AS total FROM gap_events")?;
        let (head, total) = agg.first().map(|a| (a.head, a.total)).unwrap_or((0, 0));
        let tail: Vec<EventRecord> = self.select(&format!(
            "SELECT seq, kind, at, payload, prev_hash, hash FROM gap_events \
             ORDER BY seq DESC LIMIT {SPINE_WINDOW}"
        ))?;
        {
            let mut spine = self
                .events
                .lock()
                .map_err(|_| Error::Other("events lock poisoned".into()))?;
            spine.head = head;
            spine.total = total;
            spine.recent = tail.into_iter().rev().collect();
            spine.tip = spine
                .recent
                .back()
                .map(|e| e.hash.clone())
                .unwrap_or_default();
            summary.events = total as usize;
        }

        for rec in self.select_paged::<ContractRecord>(
            "SELECT contract_id, client, provider, capability_id, state, contract_json, \
             updated_at FROM gap_contracts FINAL",
            "contract_id",
        )? {
            self.contracts
                .lock()
                .map_err(|_| Error::Other("contracts lock poisoned".into()))?
                .insert(rec.contract_id.clone(), rec);
            summary.contracts += 1;
        }

        for rec in self.select_paged::<AnnouncementRecord>(
            "SELECT agent_did, announcement_json, expires_at FROM gap_announcements FINAL",
            "agent_did",
        )? {
            // An empty body is a tombstone from `delete_announcement`.
            // Loading it would put a withdrawn agent back in the
            // directory, which is the bug the tombstone exists to fix.
            if rec.announcement_json.trim().is_empty() {
                continue;
            }
            self.announcements
                .lock()
                .map_err(|_| Error::Other("announcements lock poisoned".into()))?
                .insert(rec.agent_did.clone(), rec);
            summary.announcements += 1;
        }

        for rec in self.select_paged::<IdentityRecord>(
            "SELECT token, did, seed_hex, created_at FROM gap_identities FINAL",
            "did",
        )? {
            self.identities
                .lock()
                .map_err(|_| Error::Other("identities lock poisoned".into()))?
                .insert(rec.token.clone(), rec);
            summary.identities += 1;
        }

        for rec in self.select_paged::<EscrowRecord>(
            "SELECT contract_id, state, held, currency, updated_at FROM gap_escrows FINAL",
            "contract_id",
        )? {
            self.escrows
                .lock()
                .map_err(|_| Error::Other("escrows lock poisoned".into()))?
                .insert(rec.contract_id.clone(), rec);
            summary.escrows += 1;
        }

        for rec in self.select_paged::<DeliverableRecord>(
            "SELECT contract_id, digest, encoding, media_type, content, uri, delivered_at \
             FROM gap_deliverables FINAL",
            "contract_id",
        )? {
            self.deliverables
                .lock()
                .map_err(|_| Error::Other("deliverables lock poisoned".into()))?
                .insert(rec.contract_id.clone(), rec);
            summary.deliverables += 1;
        }

        for rec in self.select_paged::<StateRecord>(
            "SELECT scope, key, value, updated_at FROM gap_state FINAL",
            "scope, key",
        )? {
            // A tombstone: deleted entries are written back with an
            // empty value rather than mutated away, because a
            // ReplacingMergeTree collapses on the sort key and a delete
            // mutation is asynchronous.
            if rec.value.is_empty() {
                continue;
            }
            self.node_state
                .lock()
                .map_err(|_| Error::Other("state lock poisoned".into()))?
                .insert((rec.scope.clone(), rec.key.clone()), rec);
            summary.state += 1;
        }

        Ok(summary)
    }

    /// Run a SELECT and deserialize one record per line.
    ///
    /// A single unparseable row is skipped rather than failing the whole
    /// load: one malformed record must not stop a node from booting.
    /// Read a whole table in pages.
    ///
    /// One SELECT for a whole table works until the table outgrows the
    /// HTTP client's response cap, and then it fails at boot - which is
    /// how a node came up with empty mirrors over 38,773 stored events.
    /// Any fixed ceiling is a date, not a limit.
    ///
    /// Ordered offset paging rather than a keyset: these are projections
    /// read once at startup, the ordering column is the primary key, and
    /// a keyset walk would need the key value bound back into the query
    /// for six differently-typed tables. The spine does use a keyset, on
    /// `seq`, because it is the one table where a skipped row is not
    /// recoverable.
    fn select_paged<R: serde::de::DeserializeOwned>(
        &self,
        query: &str,
        order_by: &str,
    ) -> Result<Vec<R>> {
        const PAGE: usize = 2_000;
        let mut out: Vec<R> = Vec::new();
        let mut offset = 0usize;
        loop {
            let page: Vec<R> = self.select(&format!(
                "{query} ORDER BY {order_by} LIMIT {PAGE} OFFSET {offset}"
            ))?;
            let n = page.len();
            out.extend(page);
            if n < PAGE {
                return Ok(out);
            }
            offset += PAGE;
        }
    }

    /// `select_paged`, with bound parameters.
    ///
    /// Separate from `select_paged` rather than folded into it because
    /// the parameter-free form is used by hydrate on fixed SQL, and a
    /// single function taking an always-empty slice invites the next
    /// person to interpolate "just this once". Audit finding C-02 says
    /// values go through `{name:Type}`; this is how a WHERE clause on
    /// caller-supplied input obeys that.
    fn select_paged_params<R: serde::de::DeserializeOwned>(
        &self,
        query: &str,
        order_by: &str,
        params: &[QueryParam],
    ) -> Result<Vec<R>> {
        const PAGE: usize = 2_000;
        let mut out: Vec<R> = Vec::new();
        let mut offset = 0usize;
        loop {
            let q = format!(
                "{query} ORDER BY {order_by} LIMIT {PAGE} OFFSET {offset} \
                 FORMAT JSONEachRow SETTINGS output_format_json_quote_64bit_integers=0"
            );
            let body = self.transport.post_params(&q, params)?;
            let page: Vec<R> = body
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str::<R>(l).ok())
                .collect();
            let n = page.len();
            out.extend(page);
            if n < PAGE {
                return Ok(out);
            }
            offset += PAGE;
        }
    }

    fn select<R: serde::de::DeserializeOwned>(&self, query: &str) -> Result<Vec<R>> {
        let q = format!(
            "{query} FORMAT JSONEachRow SETTINGS output_format_json_quote_64bit_integers=0"
        );
        let body = self.transport.post(&q)?;
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<R>(l).ok())
            .collect())
    }

    /// Split a DDL script into individual statements, ignoring blank
    /// lines and `--` comments.
    pub fn statements(ddl: &str) -> Vec<String> {
        ddl.split(';')
            .map(|s| {
                s.lines()
                    .filter(|l| !l.trim_start().starts_with("--"))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim()
                    .to_string()
            })
            .filter(|s| !s.is_empty())
            .collect()
    }
}

impl<T: HttpTransport> Storage for ClickHouseStorage<T> {
    fn append_event(&mut self, kind: &str, payload: serde_json::Value) -> Result<u64> {
        validate_event(kind, &payload)?;
        let payload = crate::storage::canonical_payload(payload);
        let mut spine = self
            .events
            .lock()
            .map_err(|_| Error::Other("events lock poisoned".into()))?;
        // 1-based: seq 0 is reserved so that a cursor of `after=0`
        // means "everything" (RFC-0013 section 3.2 rule 14).
        //
        // Taken from the tracked head, not from the maximum of what is
        // in memory. Those were the same number only while the mirror
        // held the whole chain; now that it holds a window, deriving it
        // from the window would re-issue sequences the moment the
        // window rolled - silently forking the audit spine, which is
        // the failure this comment has warned about since the row-count
        // version of the same mistake.
        let seq = spine.head + 1;
        let at = crate::message::now_unix();
        // Link to the tip, tracked for the same reason.
        let prev = spine.tip.clone();
        let hash = crate::storage::event_hash(seq, kind, at, &payload, &prev);
        spine.push(EventRecord {
            seq,
            kind: kind.to_string(),
            at,
            payload: payload.clone(),
            prev_hash: prev.clone(),
            hash: hash.clone(),
        });
        drop(spine);
        // Insert with BOUND parameters - never string interpolation
        // (audit fix C-02: SQL injection).
        let q = "INSERT INTO gap_events (seq, kind, at, payload, prev_hash, hash) \
                 VALUES ({seq:UInt64}, {kind:String}, {at:UInt64}, {payload:String}, \
                 {prev_hash:String}, {hash:String})";
        let params = [
            QueryParam::new("seq", seq.to_string()),
            QueryParam::new("kind", kind),
            QueryParam::new("at", at.to_string()),
            QueryParam::new("payload", payload.to_string()),
            QueryParam::new("prev_hash", &prev),
            QueryParam::new("hash", &hash),
        ];
        self.transport.post_params(q, &params)?;
        Ok(seq)
    }

    fn events_after(&self, seq: u64, limit: u64) -> Result<Vec<EventRecord>> {
        {
            let spine = self
                .events
                .lock()
                .map_err(|_| Error::Other("events lock poisoned".into()))?;
            // Served from memory when the cursor is inside the window,
            // which is the case for every live reader: the feed, the
            // stream, the expiry sweep. Only an audit walking from the
            // beginning falls through.
            if spine.recent.is_empty() || seq + 1 >= spine.oldest() {
                return Ok(spine
                    .recent
                    .iter()
                    .filter(|e| e.seq > seq)
                    .take(limit as usize)
                    .cloned()
                    .collect());
            }
        }
        // Older than the window: ask the store. Bounded by `limit`, so
        // a caller paging through the chain never materialises more
        // than one page of it.
        self.select(&format!(
            "SELECT seq, kind, at, payload, prev_hash, hash FROM gap_events \
             WHERE seq > {seq} ORDER BY seq LIMIT {limit}"
        ))
    }

    fn head_seq(&self) -> Result<u64> {
        Ok(self
            .events
            .lock()
            .map_err(|_| Error::Other("events lock poisoned".into()))?
            .head)
    }

    fn event_count(&self) -> Result<u64> {
        // The whole chain's length, which is no longer the mirror's.
        Ok(self
            .events
            .lock()
            .map_err(|_| Error::Other("events lock poisoned".into()))?
            .total)
    }

    fn upsert_contract(&mut self, record: &ContractRecord) -> Result<()> {
        let mut contracts = self
            .contracts
            .lock()
            .map_err(|_| Error::Other("contracts lock poisoned".into()))?;
        contracts.insert(record.contract_id.clone(), record.clone());
        // Async mirror write with BOUND parameters (audit fix C-02).
        let q = "INSERT INTO gap_contracts (contract_id, client, provider, capability_id, state, contract_json, updated_at) \
                 VALUES ({contract_id:String}, {client:String}, {provider:String}, \
                         {capability_id:String}, {state:String}, {contract_json:String}, {updated_at:UInt64})";
        let params = [
            QueryParam::new("contract_id", &record.contract_id),
            QueryParam::new("client", &record.client),
            QueryParam::new("provider", &record.provider),
            QueryParam::new("capability_id", &record.capability_id),
            QueryParam::new("state", &record.state),
            QueryParam::new("contract_json", &record.contract_json),
            QueryParam::new("updated_at", record.updated_at.to_string()),
        ];
        self.transport.post_params(q, &params)?;
        Ok(())
    }

    fn get_contract(&self, contract_id: &str) -> Result<Option<ContractRecord>> {
        let contracts = self
            .contracts
            .lock()
            .map_err(|_| Error::Other("contracts lock poisoned".into()))?;
        Ok(contracts.get(contract_id).cloned())
    }

    fn contracts_for_agent(&self, did: &str) -> Result<Vec<ContractRecord>> {
        // Pushed down rather than filtered in the node: an agent's own
        // history is unbounded, and reading every contract on the node
        // to answer a question about one party is how an authenticated
        // endpoint becomes a denial-of-service lever.
        self.select_paged_params::<ContractRecord>(
            "SELECT contract_id, client, provider, capability_id, state, contract_json, \
             updated_at FROM gap_contracts FINAL \
             WHERE client = {did:String} OR provider = {did:String}",
            "contract_id",
            &[QueryParam::new("did", did)],
        )
    }

    fn contracts_in_state(&self, state: &str) -> Result<Vec<ContractRecord>> {
        let contracts = self
            .contracts
            .lock()
            .map_err(|_| Error::Other("contracts lock poisoned".into()))?;
        Ok(contracts
            .values()
            .filter(|c| c.state == state)
            .cloned()
            .collect())
    }

    fn list_contracts(&self) -> Result<Vec<ContractRecord>> {
        let contracts = self
            .contracts
            .lock()
            .map_err(|_| Error::Other("contracts lock poisoned".into()))?;
        Ok(contracts.values().cloned().collect())
    }

    fn upsert_announcement(&mut self, record: &AnnouncementRecord) -> Result<()> {
        let mut announcements = self
            .announcements
            .lock()
            .map_err(|_| Error::Other("announcements lock poisoned".into()))?;
        announcements.insert(record.agent_did.clone(), record.clone());
        // BOUND parameters (audit fix C-02).
        let q = "INSERT INTO gap_announcements (agent_did, announcement_json, expires_at) \
                 VALUES ({agent_did:String}, {announcement_json:String}, {expires_at:UInt64})";
        let params = [
            QueryParam::new("agent_did", &record.agent_did),
            QueryParam::new("announcement_json", &record.announcement_json),
            QueryParam::new("expires_at", record.expires_at.to_string()),
        ];
        self.transport.post_params(q, &params)?;
        Ok(())
    }

    fn get_announcement(&self, agent_did: &str) -> Result<Option<AnnouncementRecord>> {
        let announcements = self
            .announcements
            .lock()
            .map_err(|_| Error::Other("announcements lock poisoned".into()))?;
        Ok(announcements.get(agent_did).cloned())
    }

    fn list_announcements(&self) -> Result<Vec<AnnouncementRecord>> {
        let announcements = self
            .announcements
            .lock()
            .map_err(|_| Error::Other("announcements lock poisoned".into()))?;
        Ok(announcements.values().cloned().collect())
    }

    fn delete_announcement(&mut self, agent_did: &str) -> Result<()> {
        let previous = {
            let mut announcements = self
                .announcements
                .lock()
                .map_err(|_| Error::Other("announcements lock poisoned".into()))?;
            announcements.remove(agent_did)
        };
        // A tombstone, not an ALTER DELETE: mutations are asynchronous,
        // and a restart in between resurrects the row (same reasoning as
        // `delete_state`).
        //
        // The version column here is `expires_at`, which is a FUTURE
        // timestamp, so a tombstone written at "now" would lose to the
        // row it is meant to bury. It has to outrank whatever is there,
        // and by exactly one - using u64::MAX would win forever and the
        // agent could never announce again.
        let version = previous
            .map(|p| p.expires_at)
            .unwrap_or_else(crate::message::now_unix)
            .saturating_add(1);
        let q = "INSERT INTO gap_announcements (agent_did, announcement_json, expires_at) \
                 VALUES ({agent_did:String}, '', {expires_at:UInt64})";
        let params = [
            QueryParam::new("agent_did", agent_did),
            QueryParam::new("expires_at", version.to_string()),
        ];
        self.transport.post_params(q, &params)?;
        Ok(())
    }

    fn reap_expired(&mut self) -> Result<usize> {
        let now = crate::message::now_unix();
        let mut announcements = self
            .announcements
            .lock()
            .map_err(|_| Error::Other("announcements lock poisoned".into()))?;
        let before = announcements.len();
        announcements.retain(|_, a| a.expires_at > now);
        let removed = before - announcements.len();
        let q = "ALTER TABLE gap_announcements DELETE WHERE expires_at <= {now:UInt64}";
        let params = [QueryParam::new("now", now.to_string())];
        self.transport.post_params(q, &params)?;
        Ok(removed)
    }

    fn upsert_identity(&mut self, record: &IdentityRecord) -> Result<()> {
        let mut identities = self
            .identities
            .lock()
            .map_err(|_| Error::Other("identities lock poisoned".into()))?;
        identities.insert(record.token.clone(), record.clone());
        let q = "INSERT INTO gap_identities (token, did, seed_hex, created_at) \
                 VALUES ({token:String}, {did:String}, {seed_hex:String}, {created_at:UInt64})";
        let params = [
            QueryParam::new("token", &record.token),
            QueryParam::new("did", &record.did),
            QueryParam::new("seed_hex", &record.seed_hex),
            QueryParam::new("created_at", record.created_at.to_string()),
        ];
        self.transport.post_params(q, &params)?;
        Ok(())
    }

    fn get_identity_by_token(&self, token: &str) -> Result<Option<IdentityRecord>> {
        let identities = self
            .identities
            .lock()
            .map_err(|_| Error::Other("identities lock poisoned".into()))?;
        Ok(identities.get(token).cloned())
    }

    fn list_identities(&self) -> Result<Vec<IdentityRecord>> {
        let identities = self
            .identities
            .lock()
            .map_err(|_| Error::Other("identities lock poisoned".into()))?;
        Ok(identities.values().cloned().collect())
    }

    fn upsert_escrow(&mut self, record: &EscrowRecord) -> Result<()> {
        let mut escrows = self
            .escrows
            .lock()
            .map_err(|_| Error::Other("escrows lock poisoned".into()))?;
        escrows.insert(record.contract_id.clone(), record.clone());
        let q = "INSERT INTO gap_escrows (contract_id, state, held, currency, updated_at) \
                 VALUES ({contract_id:String}, {state:String}, {held:String}, {currency:String}, {updated_at:UInt64})";
        let params = [
            QueryParam::new("contract_id", &record.contract_id),
            QueryParam::new("state", &record.state),
            QueryParam::new("held", &record.held),
            QueryParam::new("currency", &record.currency),
            QueryParam::new("updated_at", record.updated_at.to_string()),
        ];
        self.transport.post_params(q, &params)?;
        Ok(())
    }

    fn get_escrow(&self, contract_id: &str) -> Result<Option<EscrowRecord>> {
        let escrows = self
            .escrows
            .lock()
            .map_err(|_| Error::Other("escrows lock poisoned".into()))?;
        Ok(escrows.get(contract_id).cloned())
    }

    fn list_escrows(&self) -> Result<Vec<EscrowRecord>> {
        let escrows = self
            .escrows
            .lock()
            .map_err(|_| Error::Other("escrows lock poisoned".into()))?;
        Ok(escrows.values().cloned().collect())
    }

    fn upsert_deliverable(&mut self, record: &DeliverableRecord) -> Result<()> {
        let mut deliverables = self
            .deliverables
            .lock()
            .map_err(|_| Error::Other("deliverables lock poisoned".into()))?;
        deliverables.insert(record.contract_id.clone(), record.clone());
        let q = "INSERT INTO gap_deliverables \
                 (contract_id, digest, encoding, media_type, content, uri, delivered_at) \
                 VALUES ({contract_id:String}, {digest:String}, {encoding:String}, \
                 {media_type:String}, {content:String}, {uri:String}, {delivered_at:UInt64})";
        let params = [
            QueryParam::new("contract_id", &record.contract_id),
            QueryParam::new("digest", &record.digest),
            QueryParam::new("encoding", &record.encoding),
            QueryParam::new("media_type", &record.media_type),
            QueryParam::new("content", &record.content),
            QueryParam::new("uri", &record.uri),
            QueryParam::new("delivered_at", record.delivered_at.to_string()),
        ];
        self.transport.post_params(q, &params)?;
        Ok(())
    }

    fn get_deliverable(&self, contract_id: &str) -> Result<Option<DeliverableRecord>> {
        let deliverables = self
            .deliverables
            .lock()
            .map_err(|_| Error::Other("deliverables lock poisoned".into()))?;
        Ok(deliverables.get(contract_id).cloned())
    }

    fn upsert_state(&mut self, record: &StateRecord) -> Result<()> {
        self.node_state
            .lock()
            .map_err(|_| Error::Other("state lock poisoned".into()))?
            .insert((record.scope.clone(), record.key.clone()), record.clone());
        // Body, not URL. This row holds an agent's entire job history,
        // which is unbounded by construction; sending it as a parameter
        // meant the write failed silently once the agent had worked
        // enough to be worth having a history at all.
        //
        // JSONEachRow is the exchange format here rather than VALUES so
        // that quoting is serde's problem and not a hand-rolled
        // escaper's - the same class of bug that already lost a verdict
        // to a stray backslash.
        let row = serde_json::json!({
            "scope": record.scope,
            "key": record.key,
            "value": record.value,
            "updated_at": record.updated_at,
        });
        self.transport.post_body(
            "INSERT INTO gap_state (scope, key, value, updated_at) FORMAT JSONEachRow",
            &format!("{row}\n"),
        )?;
        Ok(())
    }

    fn upsert_state_many(&mut self, records: &[StateRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        // One request for the lot. JSONEachRow is line-delimited, so a
        // batch is just more lines - the same code path as a single
        // write, which is the point: a migration that used a different
        // write path from the one it migrates to would be testing
        // something other than what runs afterwards.
        let body: String = records
            .iter()
            .map(|r| {
                serde_json::json!({
                    "scope": r.scope,
                    "key": r.key,
                    "value": r.value,
                    "updated_at": r.updated_at,
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.transport.post_body(
            "INSERT INTO gap_state (scope, key, value, updated_at) FORMAT JSONEachRow",
            &format!("{body}\n"),
        )?;
        Ok(())
    }

    fn get_state(&self, scope: &str, key: &str) -> Result<Option<StateRecord>> {
        // `(scope, key)` is this table's ORDER BY, so this is a point
        // read rather than the scope scan the default would do - and the
        // verdicts scope alone holds hundreds of thousands of rows.
        Ok(self
            .select_paged_params::<StateRecord>(
                "SELECT scope, key, value, updated_at FROM gap_state FINAL \
                 WHERE scope = {scope:String} AND key = {key:String}",
                "key",
                &[QueryParam::new("scope", scope), QueryParam::new("key", key)],
            )?
            .into_iter()
            .next())
    }

    fn list_state(&self, scope: &str) -> Result<Vec<StateRecord>> {
        let state = self
            .node_state
            .lock()
            .map_err(|_| Error::Other("state lock poisoned".into()))?;
        Ok(state
            .values()
            .filter(|r| r.scope == scope)
            .cloned()
            .collect())
    }

    fn delete_state(&mut self, scope: &str, key: &str) -> Result<()> {
        self.node_state
            .lock()
            .map_err(|_| Error::Other("state lock poisoned".into()))?
            .remove(&(scope.to_string(), key.to_string()));
        // Write a tombstone rather than issuing a mutation:
        // ReplacingMergeTree collapses on (scope, key) and keeps the
        // highest updated_at, so an empty value at "now" wins and the
        // hydrate skips it. ALTER DELETE would work too, but it is
        // asynchronous and a restart in between would resurrect the row.
        let q = "INSERT INTO gap_state (scope, key, value, updated_at) \
                 VALUES ({scope:String}, {key:String}, '', {updated_at:UInt64})";
        let params = [
            QueryParam::new("scope", scope),
            QueryParam::new("key", key),
            QueryParam::new("updated_at", crate::message::now_unix().to_string()),
        ];
        self.transport.post_params(q, &params)?;
        Ok(())
    }

    fn list_deliverables(&self) -> Result<Vec<DeliverableRecord>> {
        let deliverables = self
            .deliverables
            .lock()
            .map_err(|_| Error::Other("deliverables lock poisoned".into()))?;
        Ok(deliverables.values().cloned().collect())
    }
}

#[cfg(test)]
mod insert_settings_tests {
    use super::*;

    #[test]
    fn async_insert_is_off_because_it_cannot_carry_bindings() {
        // Not a preference. ClickHouse loses a parameterised query's
        // `{name:Type}` bindings when it defers the insert, and the
        // flush fails with:
        //
        //   Code: 456 ... Substitution `seq` is not set:
        //   While executing WaitForAsyncInsert
        //
        // Every write this node makes is parameterised, so async_insert
        // breaks all of them. It was enabled to stop one part being
        // written per row and it did - by writing no rows at all, while
        // `record` swallowed the error and every page kept answering.
        assert!(
            insert_settings("INSERT INTO gap_events (seq) VALUES ({seq:UInt64})").is_empty(),
            "a parameterised insert must not be handed to async_insert"
        );
        assert!(insert_settings("SELECT 1").is_empty());
    }

    #[test]
    fn it_can_still_be_turned_on_deliberately() {
        // Left reachable for a future path that sends values inline
        // rather than as bindings, which is the trade this codebase
        // refused once already.
        std::env::set_var("GAP_CLICKHOUSE_ASYNC_INSERT", "1");
        let on = insert_settings("INSERT INTO gap_events VALUES");
        std::env::remove_var("GAP_CLICKHOUSE_ASYNC_INSERT");
        assert!(on.contains("async_insert=1"), "{on}");
        // And the spine still waits when it is: a dropped batch there
        // leaves a hole in the chain that cannot be repaired.
        assert!(on.contains("wait_for_async_insert=1"), "{on}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_helpers::run_conformance_suite;
    use std::cell::RefCell;

    /// A mock transport recording queries and bound parameters.
    #[derive(Default)]
    pub struct MockTransport {
        pub queries: RefCell<Vec<String>>,
        /// (query, params) pairs recorded via post_params.
        pub param_queries: RefCell<Vec<(String, Vec<QueryParam>)>>,
    }

    impl HttpTransport for MockTransport {
        fn post(&self, query: &str) -> Result<String> {
            self.queries.borrow_mut().push(query.to_string());
            Ok(String::new())
        }

        fn base_url(&self) -> String {
            "http://mock".into()
        }

        fn post_url(&self, _url: &str) -> Result<String> {
            Ok(String::new())
        }

        fn post_params(&self, query: &str, params: &[QueryParam]) -> Result<String> {
            self.param_queries
                .borrow_mut()
                .push((query.to_string(), params.to_vec()));
            Ok(String::new())
        }

        fn post_body(&self, query: &str, body: &str) -> Result<String> {
            self.queries.borrow_mut().push(format!("{query} :: {body}"));
            Ok(String::new())
        }
    }

    /// A transport that answers SELECTs with canned JSONEachRow lines,
    /// keyed by the table named in the query.
    struct ReplayTransport {
        rows: std::collections::HashMap<&'static str, String>,
    }

    impl HttpTransport for ReplayTransport {
        fn post(&self, query: &str) -> Result<String> {
            // The spine head is read as an AGGREGATE, not as rows, so a
            // replay that answers every gap_events query with rows
            // would hand `count()` a list of events. Answer it the way
            // the store does: derive head and total from the canned
            // rows.
            if query.contains("max(seq)") && query.contains("gap_events") {
                let rows = self.rows.get("gap_events").cloned().unwrap_or_default();
                let seqs: Vec<u64> = rows
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                    .filter_map(|v| v["seq"].as_u64())
                    .collect();
                return Ok(format!(
                    "{{\"head\":{},\"total\":{}}}\n",
                    seqs.iter().copied().max().unwrap_or(0),
                    seqs.len()
                ));
            }
            for (table, body) in &self.rows {
                if query.contains(table) {
                    return Ok(body.clone());
                }
            }
            Ok(String::new())
        }
        fn base_url(&self) -> String {
            "http://mock".into()
        }
        fn post_url(&self, _url: &str) -> Result<String> {
            Ok(String::new())
        }
        fn post_body(&self, _query: &str, _body: &str) -> Result<String> {
            Ok(String::new())
        }
    }

    /// A state value must not travel in the URL, whatever its size.
    ///
    /// Pins the bug that stopped the jobs projection on 2026-08-14: an
    /// agent's job history is one row that grows with every job, it
    /// crossed roughly 32 KB, and every write after that failed with
    /// `uri too long` while the node went on publishing a settled count
    /// derived from the stale snapshot - 3,985 against 310,696 contracts
    /// actually accepted.
    #[test]
    fn a_large_state_value_goes_in_the_body_not_the_url() {
        let mut storage = ClickHouseStorage::new(MockTransport::default());
        let big = "x".repeat(64 * 1024);
        storage
            .upsert_state(&StateRecord {
                scope: "jobs".into(),
                key: "did:gap:big".into(),
                value: big.clone(),
                updated_at: 42,
            })
            .unwrap();

        // Nothing about this write may have gone through the parameter
        // path, which is the one that builds a URL.
        let params = storage.transport.param_queries.borrow();
        assert!(
            !params.iter().any(|(q, _)| q.contains("gap_state")),
            "state was written through the URL parameter path"
        );
        let queries = storage.transport.queries.borrow();
        let sent = queries
            .iter()
            .find(|q| q.contains("gap_state"))
            .expect("state was not written at all");
        assert!(
            sent.contains("FORMAT JSONEachRow"),
            "expected a body insert, got: {}",
            &sent[..sent.len().min(120)]
        );
        assert!(sent.contains(&big), "the value did not reach the body");
    }

    #[test]
    fn hydrate_reads_persisted_state_back_into_the_mirrors() {
        // The bug this pins: every read went through in-memory mirrors
        // that started empty, so ClickHouse was a write-only sink. A
        // restart emptied the agent directory and invalidated every
        // bearer token, while the rows sat in the cluster untouched.
        let mut rows = std::collections::HashMap::new();
        rows.insert(
            "gap_events",
            "{\"seq\":2,\"kind\":\"ctr.accept\",\"at\":100,\"payload\":{\"a\":1}}\n\
             {\"seq\":1,\"kind\":\"ctr.propose\",\"at\":99,\"payload\":{\"a\":0}}\n"
                .to_string(),
        );
        rows.insert(
            "gap_identities",
            "{\"token\":\"t1\",\"did\":\"did:gap:aa\",\"seed_hex\":\"ab\",\"created_at\":1}\n"
                .to_string(),
        );
        rows.insert(
            "gap_announcements",
            "{\"agent_did\":\"did:gap:aa\",\"announcement_json\":\"{}\",\"expires_at\":9}\n"
                .to_string(),
        );
        let storage = ClickHouseStorage::new(ReplayTransport { rows });

        let h = storage.hydrate().unwrap();
        assert_eq!(h.events, 2);
        assert_eq!(h.identities, 1);
        assert_eq!(h.announcements, 1);

        // Reads now see the restored state.
        assert_eq!(storage.event_count().unwrap(), 2);
        assert_eq!(storage.list_identities().unwrap().len(), 1);
        assert!(storage.get_identity_by_token("t1").unwrap().is_some());

        // Events come back in sequence order, whatever order the
        // cluster returned them in - `events_after` filters a Vec in
        // place and would otherwise silently skip the tail.
        let after = storage.events_after(0, 10).unwrap();
        assert_eq!(after.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn the_mirror_is_bounded_and_the_chain_facts_are_not_derived_from_it() {
        // The bug this pins cost a gigabyte: the mirror held every event
        // ever written, because the next sequence came from its maximum,
        // the total from its length and the previous hash from its last
        // element. All three are O(1) facts that were being paid for in
        // O(chain) memory, on a box that would have run out in four
        // days with no mechanism to give any of it back.
        let mut storage = ClickHouseStorage::new(MockTransport::default());
        let n = SPINE_WINDOW + 500;
        for _ in 0..n {
            storage
                .append_event("ctr.propose", serde_json::json!({ "contract_id": "c" }))
                .unwrap();
        }
        let spine = storage.events.lock().unwrap();
        assert_eq!(spine.recent.len(), SPINE_WINDOW, "the window must not grow");
        assert_eq!(spine.head, n as u64, "the head is tracked, not scanned");
        assert_eq!(spine.total, n as u64, "so is the total");
        assert!(spine.oldest() > 1, "the oldest events have been let go");
        drop(spine);

        // And the sequence keeps going past the window rolling, which
        // is the failure that would silently fork the chain.
        assert_eq!(
            storage
                .append_event("ctr.accept", serde_json::json!({ "contract_id": "c" }))
                .unwrap(),
            n as u64 + 1
        );
        assert_eq!(storage.head_seq().unwrap(), n as u64 + 1);
        assert_eq!(storage.event_count().unwrap(), n as u64 + 1);
    }

    #[test]
    fn a_cursor_inside_the_window_is_served_without_touching_the_store() {
        let mut storage = ClickHouseStorage::new(MockTransport::default());
        for _ in 0..50 {
            storage
                .append_event("ctr.propose", serde_json::json!({ "contract_id": "c" }))
                .unwrap();
        }
        let got = storage.events_after(47, 10).unwrap();
        assert_eq!(got.len(), 3, "48, 49, 50");
        assert_eq!(got[0].seq, 48);
        // Nothing was asked of the store: the mock records every query
        // it is given, and none of them reads events back.
        assert!(
            !storage
                .transport
                .queries
                .borrow()
                .iter()
                .any(|q| q.contains("SELECT") && q.contains("gap_events")),
            "a live cursor must not hit the store"
        );
    }

    #[test]
    fn appending_after_a_hydrate_continues_the_sequence_instead_of_repeating_it() {
        // A restored spine with a gap: two rows, highest seq 7. Counting
        // rows would issue 3 and overwrite history that already exists.
        let mut rows = std::collections::HashMap::new();
        rows.insert(
            "gap_events",
            "{\"seq\":1,\"kind\":\"a\",\"at\":1,\"payload\":{}}\n\
             {\"seq\":7,\"kind\":\"b\",\"at\":2,\"payload\":{}}\n"
                .to_string(),
        );
        let mut storage = ClickHouseStorage::new(ReplayTransport { rows });
        storage.hydrate().unwrap();
        let seq = storage
            .append_event("ctr.signed", serde_json::json!({ "id": "c1" }))
            .unwrap();
        assert_eq!(seq, 8, "must continue past the highest existing sequence");
    }

    #[test]
    fn hydrate_asks_for_unquoted_integers_and_deduplicated_rows() {
        // Two ClickHouse defaults that silently corrupt a reload:
        // 64-bit integers arrive quoted (and fail to parse into u64),
        // and a ReplacingMergeTree returns superseded rows without
        // FINAL.
        let storage = ClickHouseStorage::new(MockTransport::default());
        let _ = storage.hydrate();
        let queries = storage.transport.queries.borrow();
        assert!(queries.iter().any(|q| q.contains("gap_contracts FINAL")));
        assert!(queries
            .iter()
            .all(|q| q.contains("output_format_json_quote_64bit_integers=0")));
    }

    #[test]
    fn a_single_corrupt_row_does_not_stop_a_node_from_booting() {
        let mut rows = std::collections::HashMap::new();
        rows.insert(
            "gap_identities",
            "{\"token\":\"good\",\"did\":\"did:gap:aa\",\"seed_hex\":\"ab\",\"created_at\":1}\n\
             {not json at all}\n"
                .to_string(),
        );
        let storage = ClickHouseStorage::new(ReplayTransport { rows });
        let h = storage.hydrate().unwrap();
        assert_eq!(h.identities, 1, "the readable row still loads");
    }

    #[test]
    fn clickhouse_passes_conformance_suite() {
        let mut storage = ClickHouseStorage::new(MockTransport::default());
        run_conformance_suite(&mut storage);
    }

    #[test]
    fn clickhouse_emits_queries() {
        let mut storage = ClickHouseStorage::new(MockTransport::default());
        storage
            .append_event("ctr.signed", serde_json::json!({ "id": "c1" }))
            .unwrap();
        let t = &storage.transport;
        assert!(!t.param_queries.borrow().is_empty());
        assert!(t.param_queries.borrow()[0]
            .0
            .contains("INSERT INTO gap_events"));
    }

    #[test]
    fn sql_injection_values_are_bound_not_interpolated() {
        // Audit fix C-02 regression test: hostile values must appear as
        // bound parameters, never inside the query text.
        let mut storage = ClickHouseStorage::new(MockTransport::default());

        let hostile_kind = "x'; DROP TABLE gap_events; --";
        let hostile_payload = serde_json::json!({ "note": "'); DELETE FROM gap_contracts; --" });
        storage.append_event(hostile_kind, hostile_payload).unwrap();

        {
            let (query, params) = &storage.transport.param_queries.borrow()[0];
            // The query text must NOT contain the hostile strings.
            assert!(
                !query.contains("DROP TABLE"),
                "query must not embed hostile kind: {query}"
            );
            assert!(
                !query.contains("DELETE FROM"),
                "query must not embed hostile payload: {query}"
            );
            // The query must use placeholders.
            assert!(
                query.contains("{kind:String}"),
                "query must use bound placeholder: {query}"
            );
            assert!(
                query.contains("{payload:String}"),
                "query must use bound placeholder: {query}"
            );
            // The hostile values travel as bound parameters.
            let kind_param = params
                .iter()
                .find(|p| p.name == "kind")
                .expect("kind param");
            assert_eq!(kind_param.value, hostile_kind);
            let payload_param = params
                .iter()
                .find(|p| p.name == "payload")
                .expect("payload param");
            assert!(payload_param.value.contains("DELETE FROM"));
        }

        // Same for contract upsert.
        let rec = ContractRecord {
            contract_id: "urn:gap:ctr:'; DROP TABLE gap_contracts; --".into(),
            client: "did:gap:a'.client".into(),
            provider: "did:gap:b".into(),
            capability_id: "cap:x'; --".into(),
            state: "signed".into(),
            contract_json: "{\"evil\":\"; DROP\"}".into(),
            updated_at: 1,
        };
        storage.upsert_contract(&rec).unwrap();
        {
            let (query, params) = &storage.transport.param_queries.borrow()[1];
            assert!(
                !query.contains("DROP"),
                "contract query must not embed hostile id: {query}"
            );
            assert!(query.contains("{contract_id:String}"));
            let id_param = params
                .iter()
                .find(|p| p.name == "contract_id")
                .expect("contract_id param");
            assert_eq!(id_param.value, rec.contract_id);
        }
    }

    #[test]
    fn sequencer_serializes_critical_sections() {
        let seq = Sequencer::new(0u64);
        // Simulate an atomic escrow op: check + modify.
        let result = seq
            .critical(|balance| {
                if *balance + 5 > 10 {
                    return Err(Error::AutonomyViolation("over budget".into()));
                }
                *balance += 5;
                Ok(*balance)
            })
            .unwrap();
        assert_eq!(result, 5);
        // Second op over the cap fails and leaves state unchanged.
        let err = seq.critical(|balance| {
            if *balance + 10 > 10 {
                return Err(Error::AutonomyViolation("over budget".into()));
            }
            *balance += 10;
            Ok(*balance)
        });
        assert!(err.is_err());
        let final_balance = seq.critical(|b| Ok(*b)).unwrap();
        assert_eq!(final_balance, 5);
    }

    #[test]
    fn urlencode_handles_special_chars() {
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("a'b"), "a%27b");
        assert_eq!(urlencode("simple"), "simple");
    }

    #[test]
    fn migration_is_split_into_single_statements() {
        // ClickHouse HTTP refuses multi-statement queries outright, so
        // the whole DDL blob was rejected with a 400 the first time it
        // met a real server.
        let stmts = ClickHouseStorage::<MockTransport>::statements(DDL);
        assert!(
            stmts.len() >= 4,
            "expected one statement per table, got {}",
            stmts.len()
        );
        for s in &stmts {
            assert!(
                !s.contains(';'),
                "statement still contains a separator: {s}"
            );
            let upper = s.to_uppercase();
            assert!(
                upper.contains("CREATE TABLE") || upper.contains("ALTER TABLE"),
                "not a DDL statement: {s}"
            );
        }

        // Migrations must be replayable. Every ALTER runs on every boot,
        // including on a database that already has the column, so an
        // unguarded one turns a restart into a startup failure.
        let alters: Vec<&String> = stmts
            .iter()
            .filter(|s| s.to_uppercase().contains("ALTER TABLE"))
            .collect();
        assert!(!alters.is_empty(), "the chain migration must be here");
        for a in alters {
            assert!(
                a.to_uppercase().contains("IF NOT EXISTS"),
                "migration is not idempotent, a second boot will fail: {a}"
            );
        }
    }

    #[test]
    fn statement_splitting_ignores_comments_and_blank_lines() {
        let ddl = "-- a comment\nCREATE TABLE a (x UInt64) ENGINE=Memory;\n\n-- another\nCREATE TABLE b (y UInt64) ENGINE=Memory;\n";
        let stmts = ClickHouseStorage::<MockTransport>::statements(ddl);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].starts_with("CREATE TABLE a"));
        assert!(stmts[1].starts_with("CREATE TABLE b"));
    }

    #[test]
    fn a_json_value_survives_clickhouse_parameter_unescaping() {
        // The defect this pins, measured against a live ClickHouse
        // 24.8: a `param_x=` binding is parsed with text-format escape
        // rules, so JSON's `\"` arrived as a bare `"` and the stored
        // document no longer parsed. The write returned 200 and the row
        // was present, so nothing anywhere reported a problem; the
        // record simply vanished on the next load, and with it the job
        // page that an agent's public history was still linking to.
        let doc = serde_json::json!({
            "reasons": ["The report has the title \"Quarterly Signal Report.\""],
            "multiline": "line1\nline2",
            "windows_path": "C:\\tmp\\x",
        })
        .to_string();

        // What ClickHouse does to the value it receives.
        let unescaped = escape_param(&doc).replace("\\\\", "\\");
        assert_eq!(unescaped, doc, "the round trip must be exact");
        assert!(
            serde_json::from_str::<serde_json::Value>(&unescaped).is_ok(),
            "and the result must still parse"
        );

        // Without the escaping it does not, which is the bug.
        let naive = doc.replace("\\\"", "\"");
        assert!(serde_json::from_str::<serde_json::Value>(&naive).is_err());
    }

    #[test]
    fn escaping_is_a_no_op_on_ordinary_values() {
        // Applied to every parameter, so it must not disturb the ones
        // that carry no backslash at all.
        for v in ["urn:gap:ctr:abc", "did:gap:0123", "1786365080", ""] {
            assert_eq!(escape_param(v), v);
        }
    }
}
