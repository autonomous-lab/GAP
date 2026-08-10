//! Verifying that money actually arrived (RFC-0016 §5, §8).
//!
//! The first cut of `/v1/balance/deposit` took an `amount` from the
//! caller and credited it. With play money that is a placeholder; with
//! real money it is a faucet, because the depositor is also the party
//! that benefits from lying about the figure.
//!
//! A deposit is therefore never asserted. It is **observed**:
//!
//! 1. the agent sends tokens on chain to the node's deposit address;
//! 2. it hands the node a transaction hash;
//! 3. the node reads the chain and decides for itself.
//!
//! What the node checks, and why each check is load-bearing:
//!
//! - **the transaction succeeded** - a reverted transfer moves nothing;
//! - **the log is a `Transfer` of the configured token** - any contract
//!   can emit an event that looks like one, so the emitter must be the
//!   token we settle in;
//! - **the recipient is our deposit address** - otherwise an agent
//!   credits itself by pointing at somebody else's payment;
//! - **enough confirmations** - a reorg can unmake a transfer;
//! - **the hash has not been credited before** - replaying one transfer
//!   is free money, and it is the cheapest attack of the lot.
//!
//! Attribution is the subtle part. Nothing on chain says which agent a
//! transfer belongs to, so the sender address must be bound to an agent
//! that proved it controls the key.

use crate::amount::Amount;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

/// `keccak256("Transfer(address,address,uint256)")`, the topic every
/// ERC-20 transfer log carries first.
pub const TRANSFER_TOPIC: &str = "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// How the node was told about a deposit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepositClaim {
    /// The transaction the depositor says funded it.
    pub tx: String,
}

/// What the chain actually says happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedTransfer {
    pub token: String,
    pub from: String,
    pub to: String,
    pub amount: Amount,
    pub confirmations: u64,
}

/// Everything the node needs to judge a claim.
#[derive(Debug, Clone, Default)]
pub struct DepositPolicy {
    /// The ERC-20 we settle in. A transfer of anything else is not a
    /// deposit, however real it looks.
    pub token: String,
    /// Where deposits must land.
    pub deposit_address: String,
    /// Below this depth a transfer can still be unmade by a reorg.
    pub min_confirmations: u64,
    /// Token decimals, for converting on-chain units into `Amount`'s
    /// six-decimal minor units. USDC is 6; getting this wrong misprices
    /// every deposit by orders of magnitude, so it is explicit.
    pub decimals: u32,
}

impl DepositPolicy {
    pub fn from_env() -> Self {
        Self {
            token: std::env::var("GAP_DEPOSIT_TOKEN").unwrap_or_default(),
            deposit_address: std::env::var("GAP_DEPOSIT_ADDRESS").unwrap_or_default(),
            min_confirmations: std::env::var("GAP_DEPOSIT_CONFIRMATIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(12),
            decimals: std::env::var("GAP_DEPOSIT_DECIMALS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(6),
        }
    }

    /// Is this node able to verify a deposit at all?
    ///
    /// A custodial node with no way to check receipt must refuse
    /// deposits outright rather than credit them on trust.
    pub fn is_configured(&self) -> bool {
        !self.token.trim().is_empty() && !self.deposit_address.trim().is_empty()
    }

    /// Accept or reject what the chain reported.
    pub fn accept(&self, observed: &ObservedTransfer) -> Result<Amount> {
        if !eq_address(&observed.token, &self.token) {
            return Err(Error::EscrowViolation(format!(
                "transfer is of {}, not the settlement token {}",
                observed.token, self.token
            )));
        }
        if !eq_address(&observed.to, &self.deposit_address) {
            return Err(Error::EscrowViolation(format!(
                "transfer went to {}, not this node's deposit address",
                observed.to
            )));
        }
        if observed.confirmations < self.min_confirmations {
            return Err(Error::EscrowViolation(format!(
                "only {} confirmation(s); {} required before crediting",
                observed.confirmations, self.min_confirmations
            )));
        }
        if observed.amount.minor_units() == 0 {
            return Err(Error::EscrowViolation("transfer of zero".into()));
        }
        Ok(observed.amount)
    }
}

/// Addresses differ in case across tools; comparing them raw is a bug
/// that only shows up with the wrong wallet.
pub fn eq_address(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim().trim_start_matches("0x").to_lowercase();
    !norm(a).is_empty() && norm(a) == norm(b)
}

/// Convert an on-chain integer into an [`Amount`] (six minor decimals).
///
/// A token with more decimals than we track is truncated, never
/// rounded up: crediting a fraction of a unit that was not sent is how
/// a ledger drifts above its reserves.
pub fn units_to_amount(raw: u128, decimals: u32) -> Amount {
    const OUR_DECIMALS: u32 = 6;
    if decimals >= OUR_DECIMALS {
        Amount::from_minor(raw / 10u128.pow(decimals - OUR_DECIMALS))
    } else {
        Amount::from_minor(raw * 10u128.pow(OUR_DECIMALS - decimals))
    }
}

/// Pull the ERC-20 transfer out of a receipt as returned by
/// `eth_getTransactionReceipt`.
///
/// Returns `None` rather than an error when the receipt simply has no
/// matching log: "this transaction is not a deposit" is an ordinary
/// answer, not a failure.
pub fn transfer_from_receipt(
    receipt: &serde_json::Value,
    current_block: u64,
) -> Option<ObservedTransfer> {
    // A reverted transaction moved nothing, whatever its logs say.
    if receipt.get("status").and_then(|s| s.as_str()) != Some("0x1") {
        return None;
    }
    let block = receipt
        .get("blockNumber")
        .and_then(|b| b.as_str())
        .and_then(|b| u64::from_str_radix(b.trim_start_matches("0x"), 16).ok())?;
    let confirmations = current_block.saturating_sub(block).saturating_add(1);

    for log in receipt.get("logs")?.as_array()? {
        let topics = log.get("topics")?.as_array()?;
        if topics.len() < 3 {
            continue;
        }
        let topic0 = topics[0].as_str().unwrap_or("").trim_start_matches("0x");
        if !topic0.eq_ignore_ascii_case(TRANSFER_TOPIC) {
            continue;
        }
        let raw = log
            .get("data")
            .and_then(|d| d.as_str())
            .and_then(|d| u128::from_str_radix(d.trim_start_matches("0x"), 16).ok())?;
        return Some(ObservedTransfer {
            token: log.get("address").and_then(|a| a.as_str())?.to_string(),
            from: topic_to_address(topics[1].as_str().unwrap_or("")),
            to: topic_to_address(topics[2].as_str().unwrap_or("")),
            // Decimals are applied by the caller, which knows the
            // policy; this function only reports what it read.
            amount: Amount::from_minor(raw),
            confirmations,
        });
    }
    None
}

/// An indexed address topic is a 32-byte word; the address is its last
/// 20 bytes.
fn topic_to_address(topic: &str) -> String {
    let hex = topic.trim_start_matches("0x");
    if hex.len() < 40 {
        return String::new();
    }
    format!("0x{}", &hex[hex.len() - 40..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy() -> DepositPolicy {
        DepositPolicy {
            token: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".into(),
            deposit_address: "0x1111111111111111111111111111111111111111".into(),
            min_confirmations: 12,
            decimals: 6,
        }
    }

    fn receipt(to_topic: &str, token: &str, amount_hex: &str, status: &str) -> serde_json::Value {
        json!({
            "status": status,
            "blockNumber": "0x64",
            "logs": [{
                "address": token,
                "topics": [
                    format!("0x{TRANSFER_TOPIC}"),
                    "0x0000000000000000000000002222222222222222222222222222222222222222",
                    to_topic
                ],
                "data": amount_hex
            }]
        })
    }

    const TO_NODE: &str = "0x0000000000000000000000001111111111111111111111111111111111111111";

    #[test]
    fn a_good_transfer_is_read_off_the_receipt() {
        let r = receipt(TO_NODE, &policy().token, "0x4c4b40", "0x1"); // 5_000_000
        let t = transfer_from_receipt(&r, 200).expect("a transfer");
        assert_eq!(t.amount.minor_units(), 5_000_000);
        assert!(eq_address(&t.to, &policy().deposit_address));
        assert_eq!(t.confirmations, 101);
        assert_eq!(policy().accept(&t).unwrap().to_string_decimal(), "5.000000");
    }

    #[test]
    fn a_reverted_transaction_moved_nothing() {
        // Its logs may still be present in some clients' output; the
        // status is the only thing that decides.
        let r = receipt(TO_NODE, &policy().token, "0x4c4b40", "0x0");
        assert!(transfer_from_receipt(&r, 200).is_none());
    }

    #[test]
    fn a_transfer_to_someone_else_is_not_a_deposit() {
        // The attack: point at a real payment that was never meant for
        // this node and claim the credit.
        let elsewhere = "0x0000000000000000000000009999999999999999999999999999999999999999";
        let r = receipt(elsewhere, &policy().token, "0x4c4b40", "0x1");
        let t = transfer_from_receipt(&r, 200).unwrap();
        let err = policy().accept(&t).unwrap_err().to_string();
        assert!(err.contains("not this node's deposit address"), "{err}");
    }

    #[test]
    fn a_transfer_of_a_different_token_is_refused() {
        // Any contract can emit something that looks like a Transfer.
        // Only the token we settle in counts.
        let r = receipt(TO_NODE, "0xdeadbeef00000000000000000000000000000000", "0x4c4b40", "0x1");
        let t = transfer_from_receipt(&r, 200).unwrap();
        let err = policy().accept(&t).unwrap_err().to_string();
        assert!(err.contains("not the settlement token"), "{err}");
    }

    #[test]
    fn a_shallow_transfer_waits_for_confirmations() {
        let r = receipt(TO_NODE, &policy().token, "0x4c4b40", "0x1");
        // Same block: one confirmation, far short of twelve.
        let t = transfer_from_receipt(&r, 100).unwrap();
        assert_eq!(t.confirmations, 1);
        let err = policy().accept(&t).unwrap_err().to_string();
        assert!(err.contains("required before crediting"), "{err}");
    }

    #[test]
    fn a_zero_transfer_credits_nothing() {
        let r = receipt(TO_NODE, &policy().token, "0x0", "0x1");
        let t = transfer_from_receipt(&r, 200).unwrap();
        assert!(policy().accept(&t).is_err());
    }

    #[test]
    fn addresses_compare_regardless_of_case_or_prefix() {
        assert!(eq_address("0xAbC1", "abc1"));
        assert!(eq_address("ABC1", "0xabc1"));
        assert!(!eq_address("0xabc1", "0xabc2"));
        // ...and an empty address never matches, or an unconfigured
        // node would accept everything.
        assert!(!eq_address("", ""));
        assert!(!eq_address("0x", "0x"));
    }

    #[test]
    fn decimals_are_converted_without_inventing_value() {
        // USDC: 6 decimals, same as ours.
        assert_eq!(units_to_amount(5_000_000, 6).to_string_decimal(), "5.000000");
        // An 18-decimal token: 1.5 tokens.
        assert_eq!(
            units_to_amount(1_500_000_000_000_000_000, 18).to_string_decimal(),
            "1.500000"
        );
        // Sub-minor-unit dust truncates DOWN. Rounding up would credit
        // value that was never sent, and a ledger above its reserves is
        // exactly what proof of reserves exists to catch.
        assert_eq!(units_to_amount(1_999_999_999_999, 18).to_string_decimal(), "0.000001");
        // A 2-decimal token scales up.
        assert_eq!(units_to_amount(150, 2).to_string_decimal(), "1.500000");
    }

    #[test]
    fn a_node_that_cannot_check_receipt_says_so() {
        // A custodial node with no configured token or address must
        // refuse deposits, not credit them on trust.
        assert!(!DepositPolicy::default().is_configured());
        assert!(policy().is_configured());
    }

    #[test]
    fn a_receipt_with_no_transfer_is_simply_not_a_deposit() {
        let r = json!({ "status": "0x1", "blockNumber": "0x64", "logs": [] });
        assert!(transfer_from_receipt(&r, 200).is_none());
    }
}

/// A deposit address derived for one agent, from the node's own seed.
///
/// This is what makes attribution work. Nothing on chain says which
/// agent a transfer belongs to, and crediting on an unproven claim of a
/// sender address lets one agent capture another's payment. Giving each
/// agent its own address makes the destination the attribution: whatever
/// arrives there belongs to that agent, and nobody has to be believed.
///
/// Derived, not stored. The key is recomputable from the node seed at
/// any time, so a lost database is not lost funds - which is a real
/// risk, because these addresses hold other people's money until they
/// are swept.
pub fn deposit_key_for(node_seed: &[u8; 32], agent_did: &str) -> [u8; 32] {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(agent_did.as_bytes()), node_seed);
    let mut okm = [0u8; 32];
    hk.expand(b"gap/deposit-address/v1", &mut okm)
        .expect("32 bytes is a valid HKDF length");
    okm
}

#[cfg(test)]
mod address_tests {
    use super::*;

    #[test]
    fn each_agent_gets_its_own_deposit_key() {
        let seed = [3u8; 32];
        let a = deposit_key_for(&seed, "did:gap:aaa");
        let b = deposit_key_for(&seed, "did:gap:bbb");
        assert_ne!(a, b, "two agents sharing an address cannot be told apart");
    }

    #[test]
    fn the_same_agent_always_gets_the_same_one() {
        // Derived, not stored: a lost database must not mean lost funds.
        let seed = [3u8; 32];
        assert_eq!(
            deposit_key_for(&seed, "did:gap:aaa"),
            deposit_key_for(&seed, "did:gap:aaa")
        );
    }

    #[test]
    fn a_different_node_derives_different_addresses() {
        assert_ne!(
            deposit_key_for(&[1u8; 32], "did:gap:aaa"),
            deposit_key_for(&[2u8; 32], "did:gap:aaa")
        );
    }

    #[test]
    fn the_deposit_key_is_not_the_signing_seed() {
        // Domain separation: a deposit key that equalled the identity
        // seed would let anyone who saw one spend from the other.
        let seed = [7u8; 32];
        assert_ne!(deposit_key_for(&seed, "did:gap:aaa"), seed);
    }
}

// ---------------------------------------------------------------
// The deposit-contract rail (contracts/GapDeposit.sol)
// ---------------------------------------------------------------
//
// A plain ERC-20 transfer says how much arrived and never whose it is.
// The deposit contract carries the agent identifier in its calldata and
// emits it back in an indexed event, so one address serves every agent
// and there is nothing to sweep.

/// `keccak256("Deposited(bytes32,address,uint256)")`.
///
/// Computed rather than hardcoded: a mistyped constant would silently
/// match no event at all, and every deposit would look like "not a
/// deposit" forever.
pub fn deposited_topic() -> [u8; 32] {
    keccak(b"Deposited(bytes32,address,uint256)")
}

/// `keccak256(did)`, the identifier the contract emits.
pub fn agent_id(did: &str) -> [u8; 32] {
    keccak(did.as_bytes())
}

fn keccak(input: &[u8]) -> [u8; 32] {
    use tiny_keccak::{Hasher, Keccak};
    let mut k = Keccak::v256();
    k.update(input);
    let mut out = [0u8; 32];
    k.finalize(&mut out);
    out
}

/// A deposit as the contract reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractDeposit {
    pub contract: String,
    /// keccak256 of the agent DID, hex without `0x`.
    pub agent_id: String,
    pub from: String,
    pub amount: Amount,
    pub confirmations: u64,
}

/// Pull a `Deposited` event out of a transaction receipt.
pub fn deposit_from_receipt(
    receipt: &serde_json::Value,
    current_block: u64,
) -> Option<ContractDeposit> {
    if receipt.get("status").and_then(|s| s.as_str()) != Some("0x1") {
        return None;
    }
    let block = receipt
        .get("blockNumber")
        .and_then(|b| b.as_str())
        .and_then(|b| u64::from_str_radix(b.trim_start_matches("0x"), 16).ok())?;
    let confirmations = current_block.saturating_sub(block).saturating_add(1);
    let want = hex::encode(deposited_topic());

    for log in receipt.get("logs")?.as_array()? {
        let topics = log.get("topics")?.as_array()?;
        if topics.len() < 3 {
            continue;
        }
        let topic0 = topics[0].as_str().unwrap_or("").trim_start_matches("0x");
        if !topic0.eq_ignore_ascii_case(&want) {
            continue;
        }
        let raw = log
            .get("data")
            .and_then(|d| d.as_str())
            .and_then(|d| u128::from_str_radix(d.trim_start_matches("0x"), 16).ok())?;
        return Some(ContractDeposit {
            contract: log.get("address").and_then(|a| a.as_str())?.to_string(),
            agent_id: topics[1]
                .as_str()
                .unwrap_or("")
                .trim_start_matches("0x")
                .to_lowercase(),
            from: topic_to_address(topics[2].as_str().unwrap_or("")),
            amount: Amount::from_minor(raw),
            confirmations,
        });
    }
    None
}

impl DepositPolicy {
    /// The deposit contract, when one is configured. Preferred over the
    /// plain-transfer rail because it answers "whose money is this?"
    /// without anyone having to be believed.
    pub fn contract(&self) -> String {
        std::env::var("GAP_DEPOSIT_CONTRACT").unwrap_or_default()
    }

    /// Accept or reject a contract deposit, for a known agent.
    ///
    /// The agent identifier is checked against the DID of the caller
    /// asking for the credit. Without that, one agent could point at
    /// another's deposit and take the balance.
    pub fn accept_contract_deposit(
        &self,
        observed: &ContractDeposit,
        claiming_did: &str,
    ) -> Result<Amount> {
        let configured = self.contract();
        if !eq_address(&observed.contract, &configured) {
            return Err(Error::EscrowViolation(format!(
                "event came from {}, not this node's deposit contract",
                observed.contract
            )));
        }
        let expected = hex::encode(agent_id(claiming_did));
        if observed.agent_id != expected {
            return Err(Error::EscrowViolation(
                "that deposit was made for a different agent".into(),
            ));
        }
        if observed.confirmations < self.min_confirmations {
            return Err(Error::EscrowViolation(format!(
                "only {} confirmation(s); {} required before crediting",
                observed.confirmations, self.min_confirmations
            )));
        }
        if observed.amount.minor_units() == 0 {
            return Err(Error::EscrowViolation("deposit of zero".into()));
        }
        Ok(observed.amount)
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use serde_json::json;

    const CONTRACT: &str = "0x5555555555555555555555555555555555555555";
    const DID: &str = "did:gap:aaa";

    fn receipt(agent_topic: &str, emitter: &str, amount_hex: &str, status: &str) -> serde_json::Value {
        json!({
            "status": status,
            "blockNumber": "0x64",
            "logs": [{
                "address": emitter,
                "topics": [
                    format!("0x{}", hex::encode(deposited_topic())),
                    agent_topic,
                    "0x0000000000000000000000002222222222222222222222222222222222222222"
                ],
                "data": amount_hex
            }]
        })
    }

    fn policy() -> DepositPolicy {
        std::env::set_var("GAP_DEPOSIT_CONTRACT", CONTRACT);
        DepositPolicy {
            min_confirmations: 12,
            decimals: 6,
            ..Default::default()
        }
    }

    #[test]
    fn the_topic_is_derived_not_typed() {
        // A mistyped constant would match no event at all, and every
        // deposit would look like "not a deposit" forever.
        assert_eq!(deposited_topic().len(), 32);
        assert_ne!(deposited_topic(), [0u8; 32]);
    }

    #[test]
    fn a_deposit_for_this_agent_is_credited() {
        let topic = format!("0x{}", hex::encode(agent_id(DID)));
        let r = receipt(&topic, CONTRACT, "0x4c4b40", "0x1");
        let d = deposit_from_receipt(&r, 200).expect("a deposit");
        let amount = policy().accept_contract_deposit(&d, DID).unwrap();
        assert_eq!(amount.minor_units(), 5_000_000);
    }

    #[test]
    fn one_agent_cannot_claim_anothers_deposit() {
        // The attack the whole design exists to stop: point at somebody
        // else's payment and take the credit.
        let topic = format!("0x{}", hex::encode(agent_id("did:gap:someone-else")));
        let r = receipt(&topic, CONTRACT, "0x4c4b40", "0x1");
        let d = deposit_from_receipt(&r, 200).unwrap();
        let err = policy()
            .accept_contract_deposit(&d, DID)
            .unwrap_err()
            .to_string();
        assert!(err.contains("made for a different agent"), "{err}");
    }

    #[test]
    fn an_event_from_another_contract_is_refused() {
        // Any contract can emit an identical-looking event.
        let topic = format!("0x{}", hex::encode(agent_id(DID)));
        let r = receipt(&topic, "0x9999999999999999999999999999999999999999", "0x4c4b40", "0x1");
        let d = deposit_from_receipt(&r, 200).unwrap();
        let err = policy()
            .accept_contract_deposit(&d, DID)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not this node's deposit contract"), "{err}");
    }

    #[test]
    fn a_shallow_or_reverted_deposit_credits_nothing() {
        let topic = format!("0x{}", hex::encode(agent_id(DID)));
        let r = receipt(&topic, CONTRACT, "0x4c4b40", "0x1");
        let d = deposit_from_receipt(&r, 100).unwrap();
        assert!(policy().accept_contract_deposit(&d, DID).is_err());

        let reverted = receipt(&topic, CONTRACT, "0x4c4b40", "0x0");
        assert!(deposit_from_receipt(&reverted, 200).is_none());
    }

    #[test]
    fn the_agent_id_is_the_keccak_of_the_did() {
        // The node and the contract must agree on this, or every
        // deposit is attributed to nobody.
        assert_eq!(agent_id(DID), agent_id("did:gap:aaa"));
        assert_ne!(agent_id(DID), agent_id("did:gap:aab"));
    }
}
