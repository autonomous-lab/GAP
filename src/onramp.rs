//! Fiat on-ramp links (RFC-0016 §5.3).
//!
//! An on-ramp lets a buyer who holds no crypto fund an agent's balance:
//! it takes a card or a bank transfer and delivers stablecoin to an
//! address. The node builds the link with the agent's own derived
//! deposit address already in it, so the buyer pays and the funds land
//! where the deposit rail is watching. No transaction on the buyer's
//! side, no address to copy, nothing to paste wrong.
//!
//! # Why this is not the Stripe mistake again
//!
//! A card processor sits in the money path: it holds funds, and closing
//! an account freezes what is in flight. An on-ramp never holds
//! anything of ours - it delivers to an address and leaves. If a
//! provider drops us we lose a funnel, not a balance, and the other
//! provider still works. That is why two are supported rather than one:
//! neither is allowed to be load-bearing.
//!
//! # What is verified and what is not
//!
//! Parameter names come from each provider's published widget
//! documentation. **MoonPay's URL-signing scheme could not be verified
//! from public docs** at the time of writing: the signature is
//! implemented as HMAC-SHA256 over the query string, base64-encoded,
//! which is the documented-in-general form, and it MUST be checked
//! against a live sandbox before real money depends on it. Signing is
//! only applied when a secret is configured, so an unconfigured node
//! produces an unsigned link rather than a wrong one.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

/// Which on-ramp to send the buyer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Cheaper on card (roughly 2-5% all-in), white-label, 64+
    /// countries. The default for that reason.
    Transak,
    /// Wider reach (160+ countries) and cross-app identity, at a higher
    /// card cost. The fallback where Transak does not operate.
    Moonpay,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Transak => "transak",
            Provider::Moonpay => "moonpay",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim().to_lowercase().as_str() {
            "transak" => Provider::Transak,
            "moonpay" => Provider::Moonpay,
            other => return Err(Error::Other(format!("unknown on-ramp: {other}"))),
        })
    }
}

/// What the node needs to build a link.
#[derive(Debug, Clone, Default)]
pub struct OnrampConfig {
    pub transak_api_key: String,
    /// Transak runs separate staging and production hosts; pointing a
    /// live node at staging produces links that take real intent and
    /// deliver nothing.
    pub transak_staging: bool,
    pub moonpay_api_key: String,
    /// Signing secret. Absent means unsigned links, which providers
    /// reject once a wallet address is pre-filled.
    pub moonpay_secret: String,
    /// What the buyer ends up holding.
    pub crypto_code: String,
    /// The chain it lands on. Wrong here means funds arrive somewhere
    /// the deposit rail is not watching.
    pub network: String,
}

impl OnrampConfig {
    pub fn from_env() -> Self {
        Self {
            transak_api_key: std::env::var("GAP_TRANSAK_API_KEY").unwrap_or_default(),
            transak_staging: matches!(
                std::env::var("GAP_TRANSAK_STAGING")
                    .unwrap_or_default()
                    .trim(),
                "1" | "true" | "yes"
            ),
            moonpay_api_key: std::env::var("GAP_MOONPAY_API_KEY").unwrap_or_default(),
            moonpay_secret: std::env::var("GAP_MOONPAY_SECRET").unwrap_or_default(),
            crypto_code: std::env::var("GAP_ONRAMP_CRYPTO").unwrap_or_else(|_| "USDC".into()),
            network: std::env::var("GAP_ONRAMP_NETWORK").unwrap_or_else(|_| "base".into()),
        }
    }

    /// Which providers this node can actually offer.
    pub fn available(&self) -> Vec<Provider> {
        let mut out = Vec::new();
        if !self.transak_api_key.trim().is_empty() {
            out.push(Provider::Transak);
        }
        if !self.moonpay_api_key.trim().is_empty() {
            out.push(Provider::Moonpay);
        }
        out
    }
}

/// What the buyer is being sent to do.
#[derive(Debug, Clone)]
pub struct OnrampRequest {
    /// The agent's derived deposit address; where the stablecoin lands.
    pub deposit_address: String,
    /// Fiat the buyer spends, e.g. `EUR`.
    pub fiat_currency: String,
    /// Optional suggested amount. Left changeable on purpose: a fixed
    /// amount the buyer cannot edit turns a funding page into a
    /// checkout, and they are not buying anything here.
    pub fiat_amount: Option<String>,
    /// Echoed back by the provider's webhooks, so a support question
    /// about one payment can be tied to one agent.
    pub reference: String,
}

/// Build a widget URL with the deposit address already in it.
pub fn build_url(
    provider: Provider,
    config: &OnrampConfig,
    req: &OnrampRequest,
) -> Result<String> {
    if req.deposit_address.trim().is_empty() {
        return Err(Error::Other("no deposit address to send funds to".into()));
    }
    match provider {
        Provider::Transak => build_transak(config, req),
        Provider::Moonpay => build_moonpay(config, req),
    }
}

fn build_transak(config: &OnrampConfig, req: &OnrampRequest) -> Result<String> {
    if config.transak_api_key.trim().is_empty() {
        return Err(Error::Other("no Transak API key configured".into()));
    }
    let host = if config.transak_staging {
        "https://global-stg.transak.com"
    } else {
        "https://global.transak.com"
    };
    let mut q = vec![
        ("apiKey", config.transak_api_key.clone()),
        ("walletAddress", req.deposit_address.clone()),
        ("cryptoCurrencyCode", config.crypto_code.clone()),
        ("network", config.network.clone()),
        ("fiatCurrency", req.fiat_currency.clone()),
        ("productsAvailed", "BUY".to_string()),
        ("partnerOrderId", req.reference.clone()),
        // The address is ours to choose, not the buyer's to edit:
        // an editable one would deliver the funds somewhere the deposit
        // rail never looks.
        ("disableWalletAddressForm", "true".to_string()),
    ];
    if let Some(amount) = &req.fiat_amount {
        // `defaultFiatAmount`, not `fiatAmount`: the latter locks the
        // figure, and a buyer topping up a balance should be able to
        // decide how much.
        q.push(("defaultFiatAmount", amount.clone()));
    }
    Ok(format!("{host}?{}", encode_query(&q)))
}

fn build_moonpay(config: &OnrampConfig, req: &OnrampRequest) -> Result<String> {
    if config.moonpay_api_key.trim().is_empty() {
        return Err(Error::Other("no MoonPay API key configured".into()));
    }
    let mut q = vec![
        ("apiKey", config.moonpay_api_key.clone()),
        ("walletAddress", req.deposit_address.clone()),
        // MoonPay's docs are explicit that pre-filling a wallet address
        // requires currencyCode alongside it.
        ("currencyCode", moonpay_currency(config)),
        ("baseCurrencyCode", req.fiat_currency.to_lowercase()),
        ("externalTransactionId", req.reference.clone()),
    ];
    if let Some(amount) = &req.fiat_amount {
        q.push(("baseCurrencyAmount", amount.clone()));
    }
    let query = encode_query(&q);

    if config.moonpay_secret.trim().is_empty() {
        // Unsigned rather than wrongly signed. MoonPay rejects an
        // unsigned URL that pre-fills an address, so this fails
        // visibly at their end instead of silently misrouting funds.
        return Ok(format!("https://buy.moonpay.com?{query}"));
    }
    let signature = sign_moonpay(&format!("?{query}"), &config.moonpay_secret)?;
    Ok(format!(
        "https://buy.moonpay.com?{query}&signature={}",
        urlencode(&signature)
    ))
}

/// MoonPay names a currency by asset and chain together, e.g.
/// `usdc_base`. Sending `usdc` alone lands the funds on Ethereum
/// mainnet, where the deposit rail is not watching.
fn moonpay_currency(config: &OnrampConfig) -> String {
    let asset = config.crypto_code.to_lowercase();
    let chain = config.network.to_lowercase();
    if chain.is_empty() || chain == "ethereum" {
        asset
    } else {
        format!("{asset}_{chain}")
    }
}

/// HMAC-SHA256 over the query string, base64-encoded.
///
/// **Unverified against MoonPay's current documentation.** Validate in
/// their sandbox before real money depends on it; an incorrect
/// signature is rejected at their end, which is the safe failure, but
/// it is still a failure.
fn sign_moonpay(query_with_question_mark: &str, secret: &str) -> Result<String> {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| Error::Other("invalid MoonPay secret".into()))?;
    mac.update(query_with_question_mark.as_bytes());
    Ok(base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
}

fn encode_query(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .map(|(k, v)| format!("{}={}", k, urlencode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encode everything outside the unreserved set. Being strict
/// here matters: an address or a reference that slips through unescaped
/// silently changes the parameter the provider reads.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> OnrampConfig {
        OnrampConfig {
            transak_api_key: "tk_live_1".into(),
            transak_staging: false,
            moonpay_api_key: "pk_live_1".into(),
            moonpay_secret: "sk_live_secret".into(),
            crypto_code: "USDC".into(),
            network: "base".into(),
        }
    }

    fn request() -> OnrampRequest {
        OnrampRequest {
            deposit_address: "0x1111111111111111111111111111111111111111".into(),
            fiat_currency: "EUR".into(),
            fiat_amount: Some("50".into()),
            reference: "did:gap:aaa".into(),
        }
    }

    #[test]
    fn a_transak_link_carries_the_deposit_address_and_the_chain() {
        let url = build_url(Provider::Transak, &config(), &request()).unwrap();
        assert!(url.starts_with("https://global.transak.com?"));
        assert!(url.contains("walletAddress=0x1111111111111111111111111111111111111111"));
        assert!(url.contains("cryptoCurrencyCode=USDC"));
        assert!(url.contains("network=base"));
        assert!(url.contains("productsAvailed=BUY"));
    }

    #[test]
    fn the_buyer_cannot_redirect_the_funds_elsewhere() {
        // An editable address would deliver the money somewhere the
        // deposit rail never looks, and the agent would never be
        // credited.
        let url = build_url(Provider::Transak, &config(), &request()).unwrap();
        assert!(url.contains("disableWalletAddressForm=true"));
    }

    #[test]
    fn the_amount_is_a_suggestion_not_a_price() {
        // Topping up a balance is not a checkout: the buyer decides how
        // much. `fiatAmount` would lock it; `defaultFiatAmount` does not.
        let url = build_url(Provider::Transak, &config(), &request()).unwrap();
        assert!(url.contains("defaultFiatAmount=50"));
        assert!(!url.contains("fiatAmount=50"));
    }

    #[test]
    fn moonpay_names_the_chain_in_the_currency_code() {
        // `usdc` alone lands on Ethereum mainnet, where nothing is
        // watching for it.
        let url = build_url(Provider::Moonpay, &config(), &request()).unwrap();
        assert!(url.contains("currencyCode=usdc_base"), "{url}");
        assert!(url.contains("baseCurrencyCode=eur"));
        assert!(url.contains("baseCurrencyAmount=50"));
    }

    #[test]
    fn a_moonpay_link_is_signed_when_a_secret_exists() {
        let url = build_url(Provider::Moonpay, &config(), &request()).unwrap();
        assert!(url.contains("&signature="));
        // Deterministic: the same request signs identically, so a
        // mismatch means the query changed, not that signing is random.
        let again = build_url(Provider::Moonpay, &config(), &request()).unwrap();
        assert_eq!(url, again);
    }

    #[test]
    fn without_a_secret_the_link_is_unsigned_rather_than_wrongly_signed() {
        let mut c = config();
        c.moonpay_secret = String::new();
        let url = build_url(Provider::Moonpay, &c, &request()).unwrap();
        assert!(!url.contains("signature="));
        // MoonPay refuses an unsigned URL that pre-fills an address, so
        // this fails at their end instead of misrouting funds here.
    }

    #[test]
    fn a_provider_with_no_key_is_refused_rather_than_linked_to() {
        let c = OnrampConfig {
            crypto_code: "USDC".into(),
            network: "base".into(),
            ..Default::default()
        };
        assert!(build_url(Provider::Transak, &c, &request()).is_err());
        assert!(build_url(Provider::Moonpay, &c, &request()).is_err());
        assert!(c.available().is_empty());
    }

    #[test]
    fn staging_and_production_are_different_hosts() {
        // Pointing a live node at staging produces links that take real
        // intent and deliver nothing.
        let mut c = config();
        c.transak_staging = true;
        let url = build_url(Provider::Transak, &c, &request()).unwrap();
        assert!(url.starts_with("https://global-stg.transak.com?"), "{url}");
    }

    #[test]
    fn a_did_survives_url_encoding_intact() {
        // A DID has colons; unescaped they end the parameter early and
        // the reference arrives truncated.
        let url = build_url(Provider::Transak, &config(), &request()).unwrap();
        assert!(url.contains("partnerOrderId=did%3Agap%3Aaaa"), "{url}");
    }

    #[test]
    fn a_link_with_nowhere_to_send_the_money_is_refused() {
        let mut r = request();
        r.deposit_address = String::new();
        assert!(build_url(Provider::Transak, &config(), &r).is_err());
    }

    #[test]
    fn both_providers_are_offered_when_both_are_configured() {
        // Two providers on purpose: neither is allowed to be
        // load-bearing, so losing one costs a funnel and not a rail.
        assert_eq!(
            config().available(),
            vec![Provider::Transak, Provider::Moonpay]
        );
    }

    #[test]
    fn provider_names_round_trip() {
        for p in [Provider::Transak, Provider::Moonpay] {
            assert_eq!(Provider::parse(p.as_str()).unwrap(), p);
        }
        assert!(Provider::parse("ramp").is_err());
    }
}
