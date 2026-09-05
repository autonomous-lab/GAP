//! The x402 gateway - selling through GAP without implementing GAP.
//!
//! # Why this exists
//!
//! GAP asks a provider for a lot: mint an identity, announce
//! capabilities, accept a contract, park escrow, deliver against signed
//! criteria. That is the right shape for a deal worth arguing about,
//! and it is far too much for someone who just wants to sell an HTTP
//! endpoint by the call.
//!
//! The gateway removes all of it. A provider registers a slug and an
//! upstream URL once. Agents then call
//!
//! ```text
//! GET https://<node>/x402/{slug}/{path}
//! ```
//!
//! and get HTTP 402 until they have paid. The provider's own service
//! never learns that GAP exists, and never has to speak it.
//!
//! # What the buyer gets that a bare payment rail does not give
//!
//! Paying for an HTTP call is a solved problem, and a receipt for it is
//! a transaction hash: proof that money moved, and nothing about what
//! was delivered. Every call through this gateway is a real GAP
//! contract - so afterwards there is a job page carrying the acceptance
//! criteria both sides were bound by, the deterministic checks, the
//! judges' opinions and the node's signature over the verdict.
//!
//! That is the whole argument for putting a protocol behind a 402.
//!
//! # What it is NOT
//!
//! The node holds the provider's upstream credential in order to make
//! the call. That is a real custody responsibility over someone else's
//! secret, and it is why the value is sealed with the node's master key
//! (the same vault that protects custodied identity seeds) and never
//! leaves this process in a response, a log line or an event payload.
//! An operator running without `GAP_MASTER_KEY` gets no gateway at all
//! rather than a plaintext secret store - refusing is the only honest
//! failure here.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

/// A provider's registered pass-through route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayRoute {
    /// The path segment agents call: `/x402/{slug}/...`.
    pub slug: String,
    /// The DID that receives payment and owns the track record.
    pub owner: String,
    /// Where the request is forwarded, without a trailing slash.
    pub upstream: String,
    /// The capability this sells under, so the job lands on the same
    /// public record as any other contract for it.
    pub capability_id: String,
    pub amount: String,
    pub currency: String,
    /// Header used to authenticate to the upstream service.
    #[serde(default)]
    pub auth_header: String,
    /// The credential, SEALED. Never serialised in any public shape -
    /// see `public()`, which is what every response goes through.
    #[serde(default)]
    pub auth_value_sealed: String,
    /// Acceptance criteria the buyer is told about BEFORE paying.
    ///
    /// A gateway call is bought sight unseen, so the criteria cannot be
    /// negotiated the way they are in a normal contract. Publishing them
    /// in the 402 is what keeps the deal honest: the buyer sees what the
    /// verdict will be measured against while it still has the option
    /// not to pay.
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
}

impl GatewayRoute {
    /// The route as anyone outside this process may see it.
    ///
    /// Separate from `Serialize` on purpose. A struct that is safe to
    /// serialise in one place and unsafe in another is a leak waiting
    /// for the next person to add a debug endpoint.
    pub fn public(&self) -> serde_json::Value {
        serde_json::json!({
            "slug": self.slug,
            "owner": self.owner,
            "capability_id": self.capability_id,
            "price": { "amount": self.amount, "currency": self.currency },
            "acceptance_criteria": self.acceptance_criteria,
            "upstream_host": host_of(&self.upstream),
        })
    }

    /// The 402 challenge, in the shape x402 clients already parse.
    ///
    /// `accepts[]` follows the x402 discovery shape so that an agent
    /// built for any x402 endpoint can read the price without knowing
    /// anything about GAP. `gap` carries what makes this different: the
    /// contract that is waiting, and the criteria the work will be
    /// judged against.
    pub fn challenge(
        &self,
        resource: &str,
        contract_id: &str,
        node_did: &str,
    ) -> serde_json::Value {
        // An anonymous caller has no contract yet - there is no party
        // to name - so it is told to identify itself rather than handed
        // steps it cannot run.
        //
        // For everyone else: the buyer already signed this contract by
        // proposing it, and the provider side was accepted for it, so
        // there is nothing left to accept. The one act still required
        // of the buyer - and its moment of consent to the criteria
        // above - is funding.
        let how_to_pay = if contract_id.is_empty() {
            vec![
                "identify yourself: send Authorization: Bearer <your node token>".to_string(),
                "this endpoint will then draft a contract and quote it here".to_string(),
            ]
        } else {
            vec![
                format!(
                    "POST /v1/escrow/park  {{\"contract_id\":\"{contract_id}\",\"amount\":\"{}\"}}  (as the client, with your bearer token)",
                    self.amount
                ),
                format!("retry this request with header:  GAP-Contract: {contract_id}"),
            ]
        };
        serde_json::json!({
            "x402Version": 1,
            "accepts": [{
                "scheme": "gap-escrow",
                "network": "gap",
                "maxAmountRequired": self.amount,
                "asset": self.currency,
                "payTo": self.owner,
                "resource": resource,
                "description": format!("1 call to {}", self.slug),
            }],
            "gap": {
                "node": node_did,
                "contract_id": contract_id,
                "capability_id": self.capability_id,
                "acceptance_criteria": self.acceptance_criteria,
                "how_to_pay": how_to_pay,
                "why": "settlement produces a job page with the verdict, not just a payment record",
            }
        })
    }
}

/// Host of a URL, for showing where a call goes without showing the
/// path or any query string - those can carry credentials.
pub fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Refuse an upstream that points back inside the infrastructure.
///
/// A gateway makes the NODE fetch a URL that somebody else chose. Left
/// unchecked that is a request-forgery primitive: an authenticated
/// provider could register `https://169.254.169.254/...` and have the
/// node read its own cloud credentials, or reach an admin service that
/// is only protected by being unroutable from outside.
///
/// This is a hostname check, and hostname checks are a first line, not
/// a wall - a name that resolves to a private address today, or that
/// re-resolves between this check and the request, gets through. Saying
/// so here rather than implying the problem is solved: closing it
/// properly means resolving the host and validating every address
/// before connecting, which needs a resolver this node does not
/// currently own.
pub fn upstream_is_public(url: &str) -> bool {
    // Brackets FIRST, then the port. Splitting on ':' before stripping
    // them turns `[fd00::1]` into `[fd00`, which parses as no address at
    // all and is therefore treated as a public name - the exact inverse
    // of what this function is for.
    let raw = host_of(url);
    let host = if let Some(inner) = raw.strip_prefix('[') {
        inner.split(']').next().unwrap_or("").to_ascii_lowercase()
    } else {
        raw.split(':').next().unwrap_or("").to_ascii_lowercase()
    };
    if host.is_empty() {
        return false;
    }
    // Names that mean "here".
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".internal") {
        return false;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => {
                !(v4.is_private()
                    || v4.is_loopback()
                    || v4.is_link_local()
                    || v4.is_broadcast()
                    || v4.is_documentation()
                    || v4.is_unspecified()
                    // 100.64.0.0/10, carrier-grade NAT: routable-looking
                    // and reachable inside many hosting networks.
                    || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1])))
            }
            std::net::IpAddr::V6(v6) => {
                !(v6.is_loopback()
                    || v6.is_unspecified()
                    // fc00::/7 unique-local and fe80::/10 link-local.
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                    || (v6.segments()[0] & 0xffc0) == 0xfe80)
            }
        };
    }
    true
}

/// Reject a slug that would let a route escape its own namespace.
///
/// `..`, a slash or an encoded slash in a slug would make
/// `/x402/{slug}/{path}` address something other than the registered
/// route. Allowing only an explicit alphabet is the version of this
/// check that cannot be outflanked by an encoding nobody thought of.
pub fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Build the upstream URL for a forwarded call.
///
/// The tail is whatever the agent asked for after the slug. It is
/// appended, never resolved: a tail containing `..` must not be able to
/// climb above the registered upstream base.
pub fn upstream_url(route: &GatewayRoute, tail: &str) -> Result<String> {
    if tail.split('/').any(|seg| seg == ".." || seg == ".") {
        return Err(Error::Other("path traversal in gateway tail".into()));
    }
    let base = route.upstream.trim_end_matches('/');
    if tail.is_empty() {
        return Ok(base.to_string());
    }
    Ok(format!("{base}/{}", tail.trim_start_matches('/')))
}

/// What the gateway decided to do with a call.
///
/// `Forward` carries the opened credential ON PURPOSE, so the caller can
/// release the node lock before touching the network. Everything about
/// this type exists to make that release possible.
#[allow(clippy::large_enum_variant)]
pub enum GatewayStep {
    /// Answer 402 with this.
    Challenge(serde_json::Value),
    /// Paid and verified - make this call, then settle.
    Forward {
        url: String,
        auth_header: String,
        auth_value: String,
        contract_id: String,
        provider_token: String,
        client_token: String,
    },
}

/// Written by hand, and never derived.
///
/// `Forward` holds the OPENED upstream credential. A derived `Debug`
/// would put it in any panic message, any `{:?}` a future maintainer
/// reaches for, and any log line written in a hurry during an incident
/// (which is exactly when someone reaches for `{:?}`). The secret is the
/// one field whose exposure the provider cannot recover from, because it
/// cannot rotate what it does not know has leaked.
impl std::fmt::Debug for GatewayStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Challenge(v) => f.debug_tuple("Challenge").field(v).finish(),
            Self::Forward {
                url, contract_id, ..
            } => f
                .debug_struct("Forward")
                .field("url", url)
                .field("contract_id", contract_id)
                .field("auth_value", &"<redacted>")
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route() -> GatewayRoute {
        GatewayRoute {
            slug: "acme".into(),
            owner: "did:gap:aa".into(),
            upstream: "https://api.acme.test/v1".into(),
            capability_id: "cap:acme:search".into(),
            amount: "0.010000".into(),
            currency: "USDC".into(),
            auth_header: "Authorization".into(),
            auth_value_sealed: "sealed:deadbeef".into(),
            acceptance_criteria: vec!["returns JSON".into()],
        }
    }

    /// The credential must not be reachable through any public shape.
    ///
    /// This is the one field whose exposure would be unrecoverable -
    /// the provider cannot rotate what it does not know has leaked. The
    /// test asserts on the rendered JSON rather than on the struct so
    /// that adding a field cannot quietly widen it.
    #[test]
    fn the_upstream_credential_never_appears_in_public_output() {
        let r = route();
        let public = r.public().to_string();
        assert!(!public.contains("sealed:deadbeef"), "{public}");
        assert!(!public.contains("auth_value"), "{public}");
        let challenge = r
            .challenge(
                "https://node/x402/acme/search",
                "urn:gap:ctr:1",
                "did:gap:node",
            )
            .to_string();
        assert!(!challenge.contains("sealed:deadbeef"), "{challenge}");
        // The host is useful to a buyer; the full URL can carry a key
        // in its query string, so only the host is published.
        assert_eq!(host_of(&r.upstream), "api.acme.test");
        assert!(!public.contains("/v1"), "the upstream path is published");
    }

    /// The node must not be usable as a probe into its own network.
    ///
    /// Registering a route is authenticated, which lowers the odds but
    /// not the consequence: minting an identity on this node costs one
    /// unauthenticated request.
    #[test]
    fn an_upstream_cannot_point_back_inside_the_infrastructure() {
        assert!(upstream_is_public("https://api.acme.test/v1"));
        assert!(upstream_is_public("https://gap.geta.team/demo/loto"));

        // The one that matters most: cloud metadata.
        assert!(!upstream_is_public(
            "https://169.254.169.254/latest/meta-data/"
        ));
        assert!(!upstream_is_public("https://metadata.google.internal/"));

        assert!(!upstream_is_public("https://localhost:8080/x"));
        assert!(!upstream_is_public("https://127.0.0.1/x"));
        assert!(!upstream_is_public("https://10.0.0.5/x"));
        assert!(!upstream_is_public("https://172.17.0.1:8080/x"));
        assert!(!upstream_is_public("https://192.168.1.1/x"));
        assert!(!upstream_is_public("https://100.100.0.1/x"));
        assert!(!upstream_is_public("https://[::1]/x"));
        assert!(!upstream_is_public("https://[fd00::1]/x"));
        assert!(!upstream_is_public(""));
    }

    /// A slug is a namespace, and a namespace that can be escaped is
    /// not one.
    #[test]
    fn a_slug_cannot_address_another_route() {
        assert!(valid_slug("acme"));
        assert!(valid_slug("io-net-2"));
        assert!(!valid_slug(""));
        assert!(!valid_slug("../admin"));
        assert!(!valid_slug("acme/x"));
        assert!(!valid_slug("acme%2Fx"));
        assert!(!valid_slug("ACME"));
    }

    /// The tail is the agent's input, so it is the attack surface.
    #[test]
    fn a_tail_cannot_climb_above_the_registered_upstream() {
        let r = route();
        assert_eq!(
            upstream_url(&r, "search?q=1").unwrap(),
            "https://api.acme.test/v1/search?q=1"
        );
        assert_eq!(upstream_url(&r, "").unwrap(), "https://api.acme.test/v1");
        assert!(upstream_url(&r, "../admin").is_err());
        assert!(upstream_url(&r, "a/../../b").is_err());
    }

    /// The credential must not be printable, even by accident.
    ///
    /// `{:?}` is what someone reaches for at 3am during an incident,
    /// which is the worst possible moment to print a secret into a log
    /// nobody will think to scrub afterwards.
    #[test]
    fn debug_output_cannot_leak_the_credential() {
        let step = GatewayStep::Forward {
            url: "https://api.acme.test/v1/search".into(),
            auth_header: "Authorization".into(),
            auth_value: "sk-live-do-not-print".into(),
            contract_id: "urn:gap:ctr:1".into(),
            provider_token: "tok-provider".into(),
            client_token: "tok-client".into(),
        };
        let printed = format!("{step:?}");
        assert!(!printed.contains("sk-live-do-not-print"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
        // Bearer tokens are credentials too, and neither belongs in a
        // debug line.
        assert!(!printed.contains("tok-provider"), "{printed}");
        assert!(!printed.contains("tok-client"), "{printed}");
    }

    /// An x402 client must be able to read the price without knowing
    /// anything about GAP, and a GAP client must find the contract.
    #[test]
    fn the_challenge_is_readable_by_both_kinds_of_client() {
        let c = route().challenge("https://node/x402/acme/s", "urn:gap:ctr:9", "did:gap:node");
        assert_eq!(c["x402Version"], 1);
        assert_eq!(c["accepts"][0]["maxAmountRequired"], "0.010000");
        assert_eq!(c["accepts"][0]["payTo"], "did:gap:aa");
        assert_eq!(c["gap"]["contract_id"], "urn:gap:ctr:9");
        // The criteria are visible BEFORE paying, which is the only
        // moment the buyer can still walk away.
        assert_eq!(c["gap"]["acceptance_criteria"][0], "returns JSON");
    }

    /// The instructions have to be the ones that work. An earlier
    /// version told the buyer to accept the contract first, which the
    /// node refuses with a 400 - the buyer signed it by proposing it,
    /// so funding is the only step left. An agent that follows a 402
    /// literally and gets an error concludes the protocol is broken.
    #[test]
    fn how_to_pay_names_only_steps_that_succeed() {
        let c = route().challenge("https://node/x402/acme/s", "urn:gap:ctr:9", "did:gap:node");
        let steps = c["gap"]["how_to_pay"].to_string();
        assert!(steps.contains("/v1/escrow/park"), "{steps}");
        assert!(steps.contains("urn:gap:ctr:9"), "{steps}");
        // The price to park has to be in there: an agent that guesses
        // the amount underfunds and is turned away at the retry.
        assert!(steps.contains("0.010000"), "{steps}");
        assert!(steps.contains("GAP-Contract"), "{steps}");
        assert!(
            !steps.contains("/accept"),
            "still tells the buyer to accept: {steps}"
        );
    }

    /// Anonymous callers get told what they lack, not steps that need a
    /// contract id nobody has drafted for them.
    #[test]
    fn the_anonymous_challenge_asks_for_identity_first() {
        let c = route().challenge("https://node/x402/acme/s", "", "did:gap:node");
        let steps = c["gap"]["how_to_pay"].to_string();
        assert!(steps.contains("Bearer"), "{steps}");
        assert!(!steps.contains("/v1/escrow/park"), "{steps}");
        assert_eq!(c["gap"]["contract_id"], "");
        // The price is still readable: deciding whether to bother
        // minting an identity needs it.
        assert_eq!(c["accepts"][0]["maxAmountRequired"], "0.010000");
    }
}
