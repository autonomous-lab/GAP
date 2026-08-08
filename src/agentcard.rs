//! Well-known discovery / AgentCard (RFC-0010).
//!
//! Every agent publishes a signed AgentCard at
//! `/.well-known/gap-agent.json` on its own domain, so clients can
//! discover and verify an agent without a registry.

use crate::error::{Error, Result};
use crate::identity::{AgentIdentity, Did};
use serde::{Deserialize, Serialize};

/// The canonical well-known path (RFC 8615).
pub const WELL_KNOWN_PATH: &str = "/.well-known/gap-agent.json";

/// A capability entry in the card.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardCapability {
    pub id: String,
    pub name: String,
    /// Price as DECIMAL STRING (precision requirement, matching OAP).
    pub price: CardPrice,
    #[serde(default)]
    pub irreversibility_class: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardPrice {
    pub amount: String,
    pub currency: String,
    pub model: String,
}

/// The signed AgentCard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCard {
    pub gap_version: String,
    pub agent: CardAgent,
    pub capabilities: Vec<CardCapability>,
    pub endpoints: CardEndpoints,
    #[serde(default)]
    pub auth: Vec<String>,
    #[serde(default)]
    pub autonomy_levels: Vec<String>,
    #[serde(default)]
    pub jurisdictions: Vec<String>,
    #[serde(default)]
    pub credentials: Vec<String>,
    pub updated_at: u64,
    #[serde(default)]
    pub agent_sig: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardAgent {
    pub did: Did,
    pub name: String,
    pub description_for_agents: String,
    #[serde(default)]
    pub provider: Option<CardProvider>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardProvider {
    pub did: Did,
    pub legal_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardEndpoints {
    pub invoke: String,
    #[serde(default)]
    pub discover: Option<String>,
    #[serde(default)]
    pub billing: Option<String>,
}

impl AgentCard {
    /// Create and sign a card.
    pub fn create(
        agent: &AgentIdentity,
        name: &str,
        description: &str,
        capabilities: Vec<CardCapability>,
        endpoints: CardEndpoints,
        jurisdictions: Vec<String>,
    ) -> Self {
        let mut card = Self {
            gap_version: "0.2.0".into(),
            agent: CardAgent {
                did: agent.did().clone(),
                name: name.into(),
                description_for_agents: description.into(),
                provider: None,
            },
            capabilities,
            endpoints,
            auth: vec!["bearer".into()],
            autonomy_levels: vec!["propose".into(), "execute-notify".into()],
            jurisdictions,
            credentials: vec![],
            updated_at: crate::message::now_unix(),
            agent_sig: None,
        };
        card.resign(agent);
        card
    }

    /// Re-sign after mutation.
    pub fn resign(&mut self, agent: &AgentIdentity) {
        self.agent_sig = None;
        let canonical = self.canonical_bytes();
        self.agent_sig = Some(agent.sign(&canonical).to_hex());
    }

    /// Verify the card's signature against its claimed DID.
    pub fn verify(&self) -> Result<()> {
        let sig_hex = self.agent_sig.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            &self.agent.did,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )
    }

    /// Validate price decimal strings: non-negative, parseable.
    pub fn validate_prices(&self) -> Result<()> {
        for cap in &self.capabilities {
            let amount = cap.price.amount.parse::<f64>().map_err(|_| {
                Error::Other(format!("invalid decimal price: {}", cap.price.amount))
            })?;
            if amount < 0.0 {
                return Err(Error::Other("negative price".into()));
            }
        }
        Ok(())
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.agent_sig = None;
        let v = serde_json::to_value(&clone).expect("card serializes");
        serde_json::to_vec(&v).expect("card serializes")
    }
}

/// A fetch trait so tests can mock HTTP.
pub trait CardFetcher {
    fn fetch(&self, url: &str) -> Result<AgentCard>;
}

/// An in-memory fetcher for tests and local deployments.
pub struct MemoryFetcher {
    cards: std::collections::HashMap<String, AgentCard>,
}

impl Default for MemoryFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryFetcher {
    pub fn new() -> Self {
        Self {
            cards: std::collections::HashMap::new(),
        }
    }

    pub fn add(&mut self, url: &str, card: AgentCard) {
        self.cards.insert(url.to_string(), card);
    }
}

impl CardFetcher for MemoryFetcher {
    fn fetch(&self, url: &str) -> Result<AgentCard> {
        self.cards
            .get(url)
            .cloned()
            .ok_or_else(|| Error::Other(format!("card not found at {url}")))
    }
}

/// Discover + verify a card at a well-known URL.
pub fn discover(fetcher: &impl CardFetcher, domain: &str) -> Result<AgentCard> {
    let url = format!("https://{domain}{WELL_KNOWN_PATH}");
    let card = fetcher.fetch(&url)?;
    card.verify()?;
    card.validate_prices()?;
    Ok(card)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(agent: &AgentIdentity) -> AgentCard {
        AgentCard::create(
            agent,
            "Weather Pro Agent",
            "Provides weather forecasts. Deterministic, no marketing language.",
            vec![CardCapability {
                id: "cap:weather:forecast".into(),
                name: "weather-forecast".into(),
                price: CardPrice {
                    amount: "0.001".into(),
                    currency: "EUR".into(),
                    model: "per_call".into(),
                },
                irreversibility_class: "reversible".into(),
            }],
            CardEndpoints {
                invoke: "https://api.weatherpro.example/gap/invoke".into(),
                discover: None,
                billing: None,
            },
            vec!["FR".into(), "DE".into()],
        )
    }

    #[test]
    fn card_verifies_and_detects_tampering() {
        let agent = AgentIdentity::generate();
        let c = card(&agent);
        assert!(c.verify().is_ok());
        assert!(c.validate_prices().is_ok());

        let mut tampered = c.clone();
        tampered.agent.name = "EVIL Agent".into();
        assert!(tampered.verify().is_err());
    }

    #[test]
    fn invalid_price_rejected() {
        let agent = AgentIdentity::generate();
        let mut c = card(&agent);
        c.capabilities[0].price.amount = "not-a-number".into();
        assert!(c.validate_prices().is_err());

        let mut c2 = card(&agent);
        c2.capabilities[0].price.amount = "-5".into();
        assert!(c2.validate_prices().is_err());
    }

    #[test]
    fn discovery_fetches_and_verifies() {
        let agent = AgentIdentity::generate();
        let c = card(&agent);
        let mut fetcher = MemoryFetcher::new();
        fetcher.add("https://weatherpro.example/.well-known/gap-agent.json", c);
        let fetched = discover(&fetcher, "weatherpro.example").unwrap();
        assert_eq!(fetched.agent.did, *agent.did());
        assert_eq!(fetched.capabilities[0].price.amount, "0.001");
    }

    #[test]
    fn discovery_rejects_forged_card() {
        let agent = AgentIdentity::generate();
        let mut c = card(&agent);
        // Forge: claim a different DID after signing.
        c.agent.did = AgentIdentity::generate().did().clone();
        let mut fetcher = MemoryFetcher::new();
        fetcher.add("https://evil.example/.well-known/gap-agent.json", c);
        assert!(discover(&fetcher, "evil.example").is_err());
    }

    #[test]
    fn missing_card_errors() {
        let fetcher = MemoryFetcher::new();
        assert!(discover(&fetcher, "nobody.example").is_err());
    }
}
