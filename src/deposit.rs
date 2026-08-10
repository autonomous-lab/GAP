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
