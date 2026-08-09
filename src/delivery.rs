//! Event delivery (RFC-0013): signed webhooks and resumable streams.
//!
//! GAP agents used to discover that a counterparty had signed, or that
//! a delivery had landed, only by polling. This module adds push:
//!
//! - [`Subscription`] — an agent's registered delivery target.
//! - [`DeliveryBody`] — what the node POSTs, signed with the node key.
//! - [`WebhookSender`] — the transport seam ([`UreqSender`] in
//!   production, [`MockSender`] in tests).
//! - [`validate_webhook_url`] — the SSRF guard (the dominant threat:
//!   a node that POSTs to an agent-supplied URL is a confused deputy
//!   inside the operator's network).
//!
//! Delivery is **at-least-once with exact resume**: every event carries
//! the audit spine's monotonic `seq`, so a receiver that missed a
//! webhook catches up with `GET /v1/audit?after=<seq>`. Push is an
//! optimization; the cursor is the contract.

use crate::error::{Error, Result};
use crate::identity::{AgentIdentity, Did};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, ToSocketAddrs};

/// Maximum delivery attempts for a single event.
pub const MAX_ATTEMPTS: u32 = 5;
/// Consecutive failed events after which a subscription is disabled.
pub const MAX_CONSECUTIVE_FAILURES: u32 = 10;
/// First backoff step, in seconds.
pub const BACKOFF_BASE_SECS: u64 = 2;
/// Backoff ceiling, in seconds (5 minutes).
pub const BACKOFF_CAP_SECS: u64 = 300;

/// Exponential backoff for attempt `n` (1-based), capped.
pub fn backoff_secs(attempt: u32) -> u64 {
    if attempt <= 1 {
        return BACKOFF_BASE_SECS;
    }
    BACKOFF_BASE_SECS
        .saturating_mul(1u64 << (attempt - 1).min(16))
        .min(BACKOFF_CAP_SECS)
}

/// How a subscriber wants events delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// The node POSTs each event to the subscriber's URL.
    Webhook,
    /// The subscriber holds an SSE connection to `GET /v1/events`.
    Stream,
}

impl Transport {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "webhook" => Ok(Transport::Webhook),
            "stream" => Ok(Transport::Stream),
            other => Err(Error::Other(format!("unknown transport: {other}"))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Transport::Webhook => "webhook",
            Transport::Stream => "stream",
        }
    }
}

/// An agent's registered intent to receive events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub subscription_id: String,
    /// The owning agent (delivery is scoped to events this DID is a party to).
    pub agent_did: Did,
    pub transport: Transport,
    /// Target URL (webhook only; empty for `stream`).
    #[serde(default)]
    pub url: String,
    /// Exact event-kind filter. Empty means "everything in scope".
    #[serde(default)]
    pub kinds: Vec<String>,
    pub created_at: u64,
    /// Consecutive failed events; reset on any success.
    #[serde(default)]
    pub failures: u32,
    /// Disabled after too many consecutive failures (RFC-0013 §3.4).
    #[serde(default)]
    pub active: bool,
}

impl Subscription {
    pub fn new(agent_did: Did, transport: Transport, url: String, kinds: Vec<String>) -> Self {
        Self {
            subscription_id: crate::new_id("sub"),
            agent_did,
            transport,
            url,
            kinds,
            created_at: crate::message::now_unix(),
            failures: 0,
            active: true,
        }
    }

    /// Whether this subscription wants an event of the given kind.
    pub fn wants(&self, kind: &str) -> bool {
        self.kinds.is_empty() || self.kinds.iter().any(|k| k == kind)
    }

    /// Public view (no internal counters beyond the failure count).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "subscription_id": self.subscription_id,
            "agent_did": self.agent_did.to_string(),
            "transport": self.transport.as_str(),
            "url": self.url,
            "kinds": self.kinds,
            "active": self.active,
            "failures": self.failures,
            "created_at": self.created_at,
        })
    }
}

/// One event as delivered to subscribers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveredEvent {
    pub seq: u64,
    pub kind: String,
    pub payload: serde_json::Value,
    pub at: u64,
}

/// The body the node POSTs to a webhook, signed with the node's key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryBody {
    pub delivery_id: String,
    pub subscription_id: String,
    /// The node's DID — the key that signed this delivery.
    pub node: String,
    pub event: DeliveredEvent,
    pub attempt: u32,
    pub sent_at: u64,
    /// Ed25519 signature by the node over the canonical body.
    ///
    /// Skipped when absent, exactly like [`Envelope`](crate::message::Envelope):
    /// the signed bytes are therefore the body with **no `signature`
    /// key at all**, not `"signature":null`. A receiver verifies by
    /// deleting the key and re-serializing canonically — the natural
    /// implementation. (Serializing an explicit null here made every
    /// independent receiver compute different bytes; caught by an
    /// out-of-process receiver during RFC-0013 bring-up.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl DeliveryBody {
    pub fn new(node: &AgentIdentity, subscription_id: &str, event: DeliveredEvent) -> Self {
        Self {
            delivery_id: crate::new_id("dlv"),
            subscription_id: subscription_id.to_string(),
            node: node.did().to_string(),
            event,
            attempt: 1,
            sent_at: crate::message::now_unix(),
            signature: None,
        }
    }

    /// Sign (or re-sign, after bumping `attempt`) with the node key.
    pub fn sign(&mut self, node: &AgentIdentity) {
        self.signature = None;
        let sig = node.sign(&self.canonical_bytes());
        self.signature = Some(format!("ed25519:{}", sig.to_hex()));
    }

    /// Verify a delivery against the node DID it claims to come from.
    /// This is what a receiving agent runs before trusting the body.
    pub fn verify(&self) -> Result<()> {
        let did = Did::parse(&self.node)?;
        let sig_hex = self
            .signature
            .as_ref()
            .ok_or(Error::BadSignature)?
            .strip_prefix("ed25519:")
            .ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            &did,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.signature = None;
        let v = serde_json::to_value(&clone).expect("delivery serializes");
        serde_json::to_vec(&v).expect("delivery serializes")
    }
}

/// A queued delivery: one event for one subscription.
#[derive(Debug, Clone)]
pub struct PendingDelivery {
    pub subscription_id: String,
    pub url: String,
    pub body: DeliveryBody,
    /// Earliest unix second at which this attempt may be sent.
    pub not_before: u64,
}

/// The transport seam for webhook delivery: real HTTP in production,
/// an in-memory recorder in tests.
pub trait WebhookSender: Send + Sync {
    /// POST `body` to `url` with the given headers. Returns the HTTP
    /// status. Implementations MUST NOT follow redirects and MUST apply
    /// short timeouts (RFC-0013 §4).
    fn post(&self, url: &str, headers: &[(&str, String)], body: &[u8]) -> Result<u16>;
}

/// Production sender: ureq with short timeouts and no redirect following.
pub struct UreqSender {
    timeout_secs: u64,
}

impl Default for UreqSender {
    fn default() -> Self {
        Self { timeout_secs: 5 }
    }
}

impl UreqSender {
    pub fn new(timeout_secs: u64) -> Self {
        Self { timeout_secs }
    }
}

impl WebhookSender for UreqSender {
    fn post(&self, url: &str, headers: &[(&str, String)], body: &[u8]) -> Result<u16> {
        // Re-validate at send time: the subscription may have been
        // registered before a DNS record was repointed at a private
        // address (rebinding).
        validate_webhook_url(url)?;
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(self.timeout_secs)))
            // A 302 to 169.254.169.254 would defeat a literal-only
            // check, so redirects are not followed (RFC-0013 §4.4).
            .max_redirects(0)
            .build()
            .new_agent();
        let mut req = agent.post(url);
        for (k, v) in headers {
            req = req.header(*k, v.as_str());
        }
        match req.send(body) {
            Ok(resp) => Ok(resp.status().as_u16()),
            Err(ureq::Error::StatusCode(code)) => Ok(code),
            Err(e) => Err(Error::Other(format!("webhook transport error: {e}"))),
        }
    }
}

/// Test sender: records every call, replays a scripted status.
/// One recorded call: (url, headers, body).
pub type RecordedCall = (String, Vec<(String, String)>, Vec<u8>);

#[derive(Default)]
pub struct MockSender {
    pub calls: std::sync::Mutex<Vec<RecordedCall>>,
    /// Status to return; defaults to 200.
    pub status: std::sync::Mutex<u16>,
    /// When set, `post` returns a transport error instead of a status.
    pub fail: std::sync::Mutex<bool>,
}

impl MockSender {
    pub fn new() -> Self {
        Self {
            calls: std::sync::Mutex::new(vec![]),
            status: std::sync::Mutex::new(200),
            fail: std::sync::Mutex::new(false),
        }
    }

    pub fn set_status(&self, status: u16) {
        *self.status.lock().unwrap() = status;
    }

    pub fn set_fail(&self, fail: bool) {
        *self.fail.lock().unwrap() = fail;
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// Parse the bodies delivered so far.
    pub fn bodies(&self) -> Vec<DeliveryBody> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(_, _, b)| serde_json::from_slice(b).ok())
            .collect()
    }

    pub fn header_of(&self, index: usize, name: &str) -> Option<String> {
        self.calls.lock().unwrap().get(index).and_then(|(_, h, _)| {
            h.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        })
    }
}

impl WebhookSender for MockSender {
    fn post(&self, url: &str, headers: &[(&str, String)], body: &[u8]) -> Result<u16> {
        self.calls.lock().unwrap().push((
            url.to_string(),
            headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            body.to_vec(),
        ));
        if *self.fail.lock().unwrap() {
            return Err(Error::Other("mock transport failure".into()));
        }
        Ok(*self.status.lock().unwrap())
    }
}

/// Reject a webhook URL that would turn the node into a confused deputy.
///
/// The node makes outbound requests to agent-supplied URLs, so this is
/// the SSRF boundary (RFC-0013 §4): scheme, embedded credentials, and —
/// critically — the **resolved** addresses, since a hostname can point
/// at loopback or cloud-metadata space.
pub fn validate_webhook_url(url: &str) -> Result<()> {
    let allow_http = std::env::var("GAP_WEBHOOK_ALLOW_HTTP").as_deref() == Ok("1");
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| Error::Other("webhook url must be absolute".into()))?;
    match scheme {
        "https" => {}
        "http" if allow_http => {}
        "http" => {
            return Err(Error::Other(
                "webhook url must use https (set GAP_WEBHOOK_ALLOW_HTTP=1 for local development)"
                    .into(),
            ))
        }
        other => return Err(Error::Other(format!("unsupported webhook scheme: {other}"))),
    }

    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err(Error::Other("webhook url has no host".into()));
    }
    // Embedded credentials would leak into logs and can smuggle a host.
    if authority.contains('@') {
        return Err(Error::Other(
            "webhook url must not embed credentials".into(),
        ));
    }

    let (host, port) = split_host_port(authority, scheme)?;
    if host.is_empty() {
        return Err(Error::Other("webhook url has no host".into()));
    }

    // Resolve and check EVERY address: a literal check alone is beaten
    // by a hostname resolving into private space (DNS rebinding).
    let addrs: Vec<IpAddr> = match (host.as_str(), port)
        .to_socket_addrs()
        .map(|it| it.map(|sa| sa.ip()).collect::<Vec<_>>())
    {
        Ok(a) => a,
        Err(_) => {
            // Unresolvable: if it is an IP literal we still classify it;
            // otherwise refuse rather than fail open.
            match host.parse::<IpAddr>() {
                Ok(ip) => vec![ip],
                Err(_) => {
                    return Err(Error::Other(format!(
                        "webhook host does not resolve: {host}"
                    )))
                }
            }
        }
    };
    if addrs.is_empty() {
        return Err(Error::Other(format!(
            "webhook host does not resolve: {host}"
        )));
    }
    // Escape hatch for local development and for deployments where the
    // node and its agents legitimately share a private network (a VPC).
    // Opt-in and loud: with this set, the node WILL call internal
    // addresses on an agent's say-so, which is the SSRF primitive the
    // rest of this function exists to deny.
    if std::env::var("GAP_WEBHOOK_ALLOW_PRIVATE").as_deref() == Ok("1") {
        return Ok(());
    }
    for ip in addrs {
        if !is_public_unicast(&ip) {
            return Err(Error::Other(format!(
                "webhook host resolves to a non-public address ({ip}); refusing to call internal infrastructure (set GAP_WEBHOOK_ALLOW_PRIVATE=1 only if the node and its agents share a trusted private network)"
            )));
        }
    }
    Ok(())
}

fn split_host_port(authority: &str, scheme: &str) -> Result<(String, u16)> {
    let default_port = if scheme == "https" { 443 } else { 80 };
    if let Some(rest) = authority.strip_prefix('[') {
        // IPv6 literal: [::1]:8080
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| Error::Other("malformed ipv6 host in webhook url".into()))?;
        let port = tail
            .strip_prefix(':')
            .map(|p| p.parse::<u16>().unwrap_or(default_port))
            .unwrap_or(default_port);
        return Ok((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            Ok((host.to_string(), p.parse().unwrap_or(default_port)))
        }
        _ => Ok((authority.to_string(), default_port)),
    }
}

/// True only for addresses safe to call from the node: globally
/// routable unicast. Everything else is somebody's internal network.
pub fn is_public_unicast(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()          // 169.254/16 — cloud metadata
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || o[0] == 0                   // 0.0.0.0/8
                || o[0] == 127
                || (o[0] == 100 && (64..128).contains(&o[1]))  // CGNAT 100.64/10
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)     // IETF protocol assignments
                || (o[0] == 198 && (18..20).contains(&o[1]))   // benchmarking 198.18/15
                || o[0] >= 240) // reserved 240/4
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xffc0) == 0xfe80   // link-local fe80::/10
                || (s[0] & 0xfe00) == 0xfc00   // unique-local fc00::/7
                || v6.to_ipv4_mapped().map(|v4| !is_public_unicast(&IpAddr::V4(v4))).unwrap_or(false))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event() -> DeliveredEvent {
        DeliveredEvent {
            seq: 41,
            kind: "exe.delivered".into(),
            payload: json!({ "contract_id": "urn:gap:ctr:a1b2" }),
            at: 1_754_000_000,
        }
    }

    #[test]
    fn delivery_is_signed_by_the_node_and_verifies() {
        let node = AgentIdentity::generate();
        let mut body = DeliveryBody::new(&node, "urn:gap:sub:1", event());
        body.sign(&node);
        assert!(body.verify().is_ok());
        assert!(body.signature.as_ref().unwrap().starts_with("ed25519:"));
    }

    #[test]
    fn tampered_delivery_fails_verification() {
        let node = AgentIdentity::generate();
        let mut body = DeliveryBody::new(&node, "urn:gap:sub:1", event());
        body.sign(&node);

        // Payload swapped in flight.
        let mut forged = body.clone();
        forged.event.payload = json!({ "contract_id": "urn:gap:ctr:evil" });
        assert!(forged.verify().is_err());

        // Attacker re-points the delivery at their own node DID.
        let attacker = AgentIdentity::generate();
        let mut spoofed = body.clone();
        spoofed.node = attacker.did().to_string();
        assert!(spoofed.verify().is_err());

        // Unsigned body is rejected outright.
        let mut unsigned = body;
        unsigned.signature = None;
        assert!(unsigned.verify().is_err());
    }

    #[test]
    fn resigning_after_attempt_bump_stays_valid() {
        let node = AgentIdentity::generate();
        let mut body = DeliveryBody::new(&node, "urn:gap:sub:1", event());
        body.sign(&node);
        body.attempt = 2;
        assert!(
            body.verify().is_err(),
            "stale signature must not cover a new attempt"
        );
        body.sign(&node);
        assert!(body.verify().is_ok());
    }

    #[test]
    fn kind_filter_matches_exactly() {
        let did = AgentIdentity::generate().did().clone();
        let all = Subscription::new(did.clone(), Transport::Webhook, "u".into(), vec![]);
        assert!(all.wants("anything"));
        let filtered = Subscription::new(
            did,
            Transport::Webhook,
            "u".into(),
            vec!["ctr.signed".into()],
        );
        assert!(filtered.wants("ctr.signed"));
        assert!(!filtered.wants("ctr.signed.extra"));
        assert!(!filtered.wants("pay.released"));
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(2), 4);
        assert_eq!(backoff_secs(3), 8);
        assert_eq!(backoff_secs(4), 16);
        assert!(backoff_secs(20) <= BACKOFF_CAP_SECS);
        assert_eq!(backoff_secs(20), BACKOFF_CAP_SECS);
    }

    /// The guard reads process environment, and cargo runs tests in
    /// parallel threads of ONE process — so env-touching tests must be
    /// serialized or they flip each other's flags.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear_optins() {
        std::env::remove_var("GAP_WEBHOOK_ALLOW_PRIVATE");
        std::env::remove_var("GAP_WEBHOOK_ALLOW_HTTP");
    }

    #[test]
    fn ssrf_guard_rejects_internal_targets() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_optins();
        // The whole point: these are the URLs an attacker registers.
        for url in [
            "http://169.254.169.254/latest/meta-data/", // cloud metadata
            "https://169.254.169.254/",
            "http://127.0.0.1:8080/v1/escrow/rule", // the node itself
            "https://localhost/hook",
            "https://10.0.0.5/hook", // RFC 1918
            "https://192.168.1.10/hook",
            "https://172.16.4.4/hook",
            "https://[::1]/hook",     // IPv6 loopback
            "https://[fe80::1]/hook", // IPv6 link-local
            "https://[fc00::1]/hook", // IPv6 unique-local
            "https://0.0.0.0/hook",
            "https://100.64.0.1/hook", // CGNAT
        ] {
            assert!(
                validate_webhook_url(url).is_err(),
                "SSRF guard must reject {url}"
            );
        }
    }

    #[test]
    fn ssrf_guard_rejects_bad_schemes_and_credentials() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_optins();
        assert!(validate_webhook_url("file:///etc/passwd").is_err());
        assert!(validate_webhook_url("gopher://evil/").is_err());
        assert!(validate_webhook_url("not-a-url").is_err());
        assert!(validate_webhook_url("https://").is_err());
        // Credentials embedded in the authority.
        assert!(validate_webhook_url("https://user:pass@example.com/h").is_err());
        // Plain http is refused unless the operator opts in.
        assert!(validate_webhook_url("http://example.com/h").is_err());
    }

    #[test]
    fn ssrf_guard_rejects_unresolvable_hosts_rather_than_failing_open() {
        assert!(
            validate_webhook_url("https://this-host-does-not-exist.invalid/hook").is_err(),
            "must fail closed"
        );
    }

    #[test]
    fn address_classification_is_correct() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        assert!(is_public_unicast(&IpAddr::V4(Ipv4Addr::new(
            93, 184, 216, 34
        ))));
        assert!(is_public_unicast(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_public_unicast(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(!is_public_unicast(&IpAddr::V4(Ipv4Addr::new(
            169, 254, 169, 254
        ))));
        assert!(!is_public_unicast(&IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        assert!(!is_public_unicast(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
        // IPv4-mapped loopback must not sneak through the v6 path.
        assert!(!is_public_unicast(
            &"::ffff:127.0.0.1".parse::<IpAddr>().unwrap()
        ));
    }

    #[test]
    fn mock_sender_records_calls() {
        let sender = MockSender::new();
        let node = AgentIdentity::generate();
        let mut body = DeliveryBody::new(&node, "urn:gap:sub:1", event());
        body.sign(&node);
        let bytes = serde_json::to_vec(&body).unwrap();
        let status = sender
            .post(
                "https://example.com/h",
                &[("X-Gap-Node", node.did().to_string())],
                &bytes,
            )
            .unwrap();
        assert_eq!(status, 200);
        assert_eq!(sender.call_count(), 1);
        assert_eq!(
            sender.header_of(0, "x-gap-node").as_deref(),
            Some(node.did().to_string().as_str())
        );
        assert!(sender.bodies()[0].verify().is_ok());
    }

    #[test]
    fn transport_parse_roundtrip() {
        assert_eq!(Transport::parse("webhook").unwrap(), Transport::Webhook);
        assert_eq!(Transport::parse("stream").unwrap(), Transport::Stream);
        assert!(Transport::parse("carrier-pigeon").is_err());
        assert_eq!(Transport::Webhook.as_str(), "webhook");
    }

    #[test]
    fn private_targets_allowed_only_with_the_explicit_opt_in() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("GAP_WEBHOOK_ALLOW_PRIVATE", "1");
        std::env::set_var("GAP_WEBHOOK_ALLOW_HTTP", "1");
        let allowed = validate_webhook_url("http://127.0.0.1:19100/hook").is_ok();
        clear_optins();
        assert!(allowed, "the documented dev/VPC escape hatch must work");
        // And with the opt-in gone, it is refused again.
        assert!(validate_webhook_url("http://127.0.0.1:19100/hook").is_err());
    }

    #[test]
    fn canonical_signing_form_omits_the_signature_key() {
        // Pinned because it is an interop contract: a receiver in any
        // language verifies by deleting `signature` and re-serializing
        // with sorted keys. An explicit null here would break every
        // independent implementation.
        let node = AgentIdentity::from_seed(&[3u8; 32]);
        let mut body = DeliveryBody::new(&node, "urn:gap:sub:1", event());
        body.delivery_id = "urn:gap:dlv:fixed".into();
        body.sent_at = 1_754_000_000;
        body.sign(&node);

        let wire: serde_json::Value =
            serde_json::from_slice(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert!(wire.get("signature").is_some(), "the wire form carries it");

        // Strip it the way a receiver would, and the bytes must be what
        // was signed.
        let mut stripped = wire.clone();
        stripped.as_object_mut().unwrap().remove("signature");
        let receiver_bytes = serde_json::to_vec(&stripped).unwrap();
        assert!(
            !String::from_utf8_lossy(&receiver_bytes).contains("signature"),
            "no signature key in the signed bytes"
        );

        let sig_hex = body
            .signature
            .as_ref()
            .unwrap()
            .strip_prefix("ed25519:")
            .unwrap();
        let sig_bytes: [u8; 64] = hex::decode(sig_hex).unwrap().try_into().unwrap();
        assert!(
            crate::identity::verify_signature(
                node.did(),
                &receiver_bytes,
                &crate::identity::Signature(sig_bytes)
            )
            .is_ok(),
            "an independent receiver must reproduce the signed bytes"
        );
    }
}
