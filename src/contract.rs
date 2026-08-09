//! Contract layer (GAP spec part 03).
//!
//! A Contract is the signed, machine-readable agreement that makes
//! agent-to-agent commerce safe. GAP's rule: no work happens without a
//! signed contract.

use crate::error::{Error, Result};
use crate::identity::{AgentIdentity, Did};
use serde::{Deserialize, Serialize};

/// Contract negotiation state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContractState {
    #[default]
    Draft,
    Signed,
    Executing,
    Delivered,
    Accepted,
    Disputed,
    Ruled,
    Cancelled,
}

impl ContractState {
    /// Lowercase wire representation, consistent across all routes.
    pub fn wire_name(&self) -> &'static str {
        match self {
            ContractState::Draft => "draft",
            ContractState::Signed => "signed",
            ContractState::Executing => "executing",
            ContractState::Delivered => "delivered",
            ContractState::Accepted => "accepted",
            ContractState::Disputed => "disputed",
            ContractState::Ruled => "ruled",
            ContractState::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "draft" => ContractState::Draft,
            "signed" => ContractState::Signed,
            "executing" => ContractState::Executing,
            "delivered" => ContractState::Delivered,
            "accepted" => ContractState::Accepted,
            "disputed" => ContractState::Disputed,
            "ruled" => ContractState::Ruled,
            "cancelled" => ContractState::Cancelled,
            _ => return Err(Error::Other(format!("unknown contract state: {s}"))),
        })
    }
}

/// Price terms.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Price {
    #[serde(deserialize_with = "de_f64_from_string")]
    pub amount: f64,
    pub currency: String,
    /// fixed | per-unit | subscription | commission
    pub model: String,
    /// Maximum total the client will pay.
    #[serde(default, deserialize_with = "de_opt_f64_from_string")]
    pub cap: Option<f64>,
}

/// The terms of a contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Terms {
    /// Free-form input specification.
    pub input: serde_json::Value,
    /// Free-form deliverable specification.
    pub deliverable: serde_json::Value,
    /// Machine-checkable acceptance criteria.
    pub acceptance_criteria: Vec<String>,
    /// UNIX timestamp deadline.
    pub deadline: u64,
    pub price: Price,
    /// One of: propose, execute-notify, execute-certified.
    pub autonomy: String,
    #[serde(default)]
    pub confidentiality: Option<String>,
}

/// A signed contract between a client and a provider.
///
/// # State model
///
/// `state` is deliberately NOT serialized (`#[serde(skip)]`): signatures
/// cover only the immutable terms, and the state is re-derived by each
/// runtime from the message log (event sourcing). This keeps signatures
/// valid across transitions and makes the contract portable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contract {
    pub contract_id: String,
    pub client: Did,
    pub provider: Did,
    pub capability_id: String,
    pub terms: Terms,
    #[serde(default)]
    pub escrow: bool,
    #[serde(default)]
    pub client_sig: Option<String>,
    #[serde(default)]
    pub provider_sig: Option<String>,
    pub created_at: u64,
    #[serde(skip)]
    pub state: ContractState,
}

impl Contract {
    /// Start a negotiation: the client proposes terms.
    pub fn propose(
        client: &AgentIdentity,
        provider: Did,
        capability_id: impl Into<String>,
        terms: Terms,
        escrow: bool,
    ) -> Self {
        let mut c = Self {
            contract_id: crate::new_id("ctr"),
            client: client.did().clone(),
            provider,
            capability_id: capability_id.into(),
            terms,
            escrow,
            client_sig: None,
            provider_sig: None,
            created_at: crate::message::now_unix(),
            state: ContractState::Draft,
        };
        let canonical = c.canonical_bytes();
        c.client_sig = Some(client.sign(&canonical).to_hex());
        c
    }

    /// The provider accepts and signs the contract.
    ///
    /// The provider MUST verify the client's signature before signing:
    /// accepting a contract with a forged client signature would bind the
    /// provider to terms the client never agreed to.
    pub fn accept_by_provider(mut self, provider: &AgentIdentity) -> Result<Self> {
        if self.provider != *provider.did() {
            return Err(Error::KeyMismatch);
        }
        // Verify the client's signature first (fail closed).
        let client_sig = self.client_sig.as_ref().ok_or(Error::UnsignedContract)?;
        crate::identity::verify_signature(
            &self.client,
            &self.canonical_bytes(),
            &decode_sig(client_sig)?,
        )
        .map_err(|_| Error::UnverifiedSignature("client signature invalid".into()))?;
        let canonical = self.canonical_bytes();
        self.provider_sig = Some(provider.sign(&canonical).to_hex());
        self.state = ContractState::Signed;
        Ok(self)
    }

    /// Verify both signatures and the state machine preconditions.
    pub fn verify_signed(&self) -> Result<()> {
        let client_sig = self.client_sig.as_ref().ok_or(Error::UnsignedContract)?;
        let provider_sig = self.provider_sig.as_ref().ok_or(Error::UnsignedContract)?;
        crate::identity::verify_signature(
            &self.client,
            &self.canonical_bytes(),
            &decode_sig(client_sig)?,
        )
        .map_err(|_| Error::UnverifiedSignature("client signature invalid".into()))?;
        crate::identity::verify_signature(
            &self.provider,
            &self.canonical_bytes(),
            &decode_sig(provider_sig)?,
        )
        .map_err(|_| Error::UnverifiedSignature("provider signature invalid".into()))
    }

    /// Check whether the deadline has passed (used by executors and
    /// meta-agents to detect late deliveries).
    pub fn is_expired(&self) -> bool {
        crate::message::now_unix() > self.terms.deadline
    }

    /// Transition the contract state (execution/payment layers call this).
    pub fn transition(&mut self, to: ContractState) -> Result<()> {
        let ok = match (self.state, to) {
            (ContractState::Draft, ContractState::Signed) => true,
            (ContractState::Draft, ContractState::Cancelled) => true,
            (ContractState::Signed, ContractState::Executing) => true,
            (ContractState::Signed, ContractState::Cancelled) => true,
            (ContractState::Executing, ContractState::Delivered) => true,
            (ContractState::Executing, ContractState::Cancelled) => true,
            (ContractState::Delivered, ContractState::Accepted) => true,
            (ContractState::Delivered, ContractState::Disputed) => true,
            (ContractState::Disputed, ContractState::Ruled) => true,
            (ContractState::Disputed, ContractState::Delivered) => true, // remedy rework
            _ => false,
        };
        if ok {
            self.state = to;
            Ok(())
        } else {
            Err(Error::InvalidTransition {
                from: format!("{:?}", self.state),
                to: format!("{to:?}"),
            })
        }
    }

    /// The canonical bytes over which signatures are computed.
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.client_sig = None;
        clone.provider_sig = None;
        let v = serde_json::to_value(&clone).expect("contract serializes");
        serde_json::to_vec(&v).expect("contract serializes")
    }
}

fn decode_sig(hex_sig: &str) -> Result<crate::identity::Signature> {
    let bytes: [u8; 64] = hex::decode(hex_sig)
        .map_err(|_| Error::BadSignature)?
        .try_into()
        .map_err(|_| Error::BadSignature)?;
    Ok(crate::identity::Signature(bytes))
}

pub(crate) fn de_f64_from_string<'de, D>(deserializer: D) -> std::result::Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| serde::de::Error::custom("invalid numeric value")),
        serde_json::Value::String(s) => s.parse::<f64>().map_err(serde::de::Error::custom),
        _ => Err(serde::de::Error::custom("value must be number or string")),
    }
}

pub(crate) fn de_opt_f64_from_string<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => n
            .as_f64()
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("invalid numeric value")),
        Some(serde_json::Value::String(s)) => {
            s.parse::<f64>().map(Some).map_err(serde::de::Error::custom)
        }
        _ => Err(serde::de::Error::custom("value must be number or string")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms() -> Terms {
        Terms {
            input: serde_json::json!({}),
            deliverable: serde_json::json!({}),
            acceptance_criteria: vec!["each lead has verified email".into()],
            deadline: crate::message::now_unix() + 86400,
            price: Price {
                amount: 0.05,
                currency: "EUR".into(),
                model: "per-unit".into(),
                cap: Some(100.0),
            },
            autonomy: "execute-notify".into(),
            confidentiality: None,
        }
    }

    #[test]
    fn contract_requires_both_signatures() {
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let c = Contract::propose(
            &client,
            provider.did().clone(),
            "cap:lead-gen",
            terms(),
            true,
        );
        // not yet signed by provider
        assert!(c.verify_signed().is_err());
        let signed = c.accept_by_provider(&provider).unwrap();
        assert!(signed.verify_signed().is_ok());
        assert_eq!(signed.state, ContractState::Signed);
    }

    #[test]
    fn provider_rejects_forged_client_signature() {
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let attacker = AgentIdentity::generate();
        let mut c = Contract::propose(
            &client,
            provider.did().clone(),
            "cap:lead-gen",
            terms(),
            true,
        );
        // Attacker replaces the client signature with their own — the
        // provider must refuse to accept.
        let canonical = c.canonical_bytes();
        c.client_sig = Some(attacker.sign(&canonical).to_hex());
        assert!(c.accept_by_provider(&provider).is_err());
    }

    #[test]
    fn contract_wrong_provider_identity_rejected() {
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let wrong = AgentIdentity::generate();
        let c = Contract::propose(
            &client,
            provider.did().clone(),
            "cap:lead-gen",
            terms(),
            true,
        );
        // A different agent tries to accept as the provider.
        assert!(c.accept_by_provider(&wrong).is_err());
    }

    #[test]
    fn contract_serialization_roundtrip_preserves_terms() {
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let c = Contract::propose(
            &client,
            provider.did().clone(),
            "cap:lead-gen",
            terms(),
            true,
        )
        .accept_by_provider(&provider)
        .unwrap();
        let wire = serde_json::to_string(&c).unwrap();
        let back: Contract = serde_json::from_str(&wire).unwrap();
        // State is not serialized (event sourcing) — but terms are intact
        // and signatures still verify.
        assert_eq!(back.terms.price.amount, 0.05);
        assert_eq!(back.terms.acceptance_criteria.len(), 1);
        assert!(back.verify_signed().is_ok());
        // State re-derives to Draft after transport (default) — the
        // runtime must re-derive it from the message log.
        assert_eq!(back.state, ContractState::Draft);
    }

    #[test]
    fn contract_is_expired_after_deadline() {
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        // Deadline already in the past (deterministic, no clock advance).
        let mut t = terms();
        t.deadline = crate::message::now_unix().saturating_sub(60);
        let c = Contract::propose(&client, provider.did().clone(), "cap:lead-gen", t, true);
        assert!(c.is_expired());

        // Deadline in the future.
        let mut t2 = terms();
        t2.deadline = crate::message::now_unix() + 3600;
        let c2 = Contract::propose(&client, provider.did().clone(), "cap:lead-gen", t2, true);
        assert!(!c2.is_expired());
    }

    #[test]
    fn transition_state_machine() {
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let mut c = Contract::propose(
            &client,
            provider.did().clone(),
            "cap:lead-gen",
            terms(),
            true,
        )
        .accept_by_provider(&provider)
        .unwrap();
        assert!(c.transition(ContractState::Executing).is_ok());
        assert!(c.transition(ContractState::Delivered).is_ok());
        assert!(c.transition(ContractState::Accepted).is_ok());
        // invalid: accepted -> executing
        assert!(c.transition(ContractState::Executing).is_err());
        // valid: signed -> cancelled (before execution)
        let mut c2 = Contract::propose(
            &client,
            provider.did().clone(),
            "cap:lead-gen",
            terms(),
            true,
        )
        .accept_by_provider(&provider)
        .unwrap();
        assert!(c2.transition(ContractState::Cancelled).is_ok());
        // invalid: accepted -> cancelled
        assert!(c.transition(ContractState::Cancelled).is_err());
        // dispute path
        let mut c3 = Contract::propose(
            &client,
            provider.did().clone(),
            "cap:lead-gen",
            terms(),
            true,
        )
        .accept_by_provider(&provider)
        .unwrap();
        c3.transition(ContractState::Executing).unwrap();
        c3.transition(ContractState::Delivered).unwrap();
        c3.transition(ContractState::Disputed).unwrap();
        c3.transition(ContractState::Ruled).unwrap();
        assert_eq!(c3.state, ContractState::Ruled);
    }
}
