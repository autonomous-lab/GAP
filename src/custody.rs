//! Custody modes and prefunded balances (RFC-0016).
//!
//! The protocol always said *when* money moves. It never said who holds
//! it in between, and gave a buyer no way to find out before committing.
//!
//! Two things live here:
//!
//! - **A declared custody mode**, published in the AgentCard so agents
//!   can filter on it the way they already filter on reputation. The
//!   goal is not to declare custody safe; it is to make it legible.
//! - **A prefunded balance ledger.** Settling a five-cent contract on
//!   chain costs about what the contract is worth, so between deposits
//!   a settlement is a ledger entry rather than a transaction. Gas is
//!   paid twice per agent lifetime instead of twice per contract.
//!
//! Every movement is an event on the audit spine, which is the
//! load-bearing property: the operator's liabilities are recomputable
//! by anyone from signed history, so a balance is a fold over a log
//! rather than a figure the operator asserts.

use crate::amount::Amount;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

/// Who holds funds between park and release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CustodyMode {
    /// An escrow contract holds them; the node relays and cannot move
    /// funds on its own.
    #[default]
    NonCustodial,
    /// The operator holds them, against a ledger.
    Custodial,
    /// The operator below a declared threshold, the contract above it.
    Hybrid,
}

impl CustodyMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            CustodyMode::NonCustodial => "non-custodial",
            CustodyMode::Custodial => "custodial",
            CustodyMode::Hybrid => "hybrid",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim().to_lowercase().as_str() {
            "non-custodial" | "noncustodial" => CustodyMode::NonCustodial,
            "custodial" => CustodyMode::Custodial,
            "hybrid" => CustodyMode::Hybrid,
            other => return Err(Error::Other(format!("unknown custody mode: {other}"))),
        })
    }

    /// Can this node hold user funds at all?
    pub fn holds_funds(&self) -> bool {
        matches!(self, CustodyMode::Custodial | CustodyMode::Hybrid)
    }
}

/// The legal person behind a custodial node. An anonymous custodian is
/// not a custodian anyone should use, so this is required whenever the
/// operator can hold funds.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Operator {
    pub legal_name: String,
    pub jurisdiction: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
}

/// What a node declares about custody (RFC-0016 §4).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CustodyPolicy {
    pub mode: CustodyMode,
    /// Above this, settlement goes on chain. Only meaningful for
    /// `hybrid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<Amount>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub currency: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<Operator>,
    /// How long a withdrawal may take. Breaching it is an incident
    /// under RFC-0012, not a private inconvenience.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawal_sla_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub settlement_contract: String,
}

impl CustodyPolicy {
    /// Read the policy from the environment. Absent configuration means
    /// non-custodial, which is the only mode that is safe to assume.
    pub fn from_env() -> Self {
        let mode = std::env::var("GAP_CUSTODY_MODE")
            .ok()
            .and_then(|m| CustodyMode::parse(&m).ok())
            .unwrap_or_default();
        let threshold = std::env::var("GAP_CUSTODY_THRESHOLD")
            .ok()
            .and_then(|t| Amount::parse(&t).ok());
        let operator = match (
            std::env::var("GAP_OPERATOR_NAME").ok(),
            std::env::var("GAP_OPERATOR_JURISDICTION").ok(),
        ) {
            (Some(legal_name), Some(jurisdiction)) if !legal_name.trim().is_empty() => {
                Some(Operator {
                    legal_name,
                    jurisdiction,
                    id: std::env::var("GAP_OPERATOR_ID").unwrap_or_default(),
                })
            }
            _ => None,
        };
        Self {
            mode,
            threshold,
            currency: std::env::var("GAP_CUSTODY_CURRENCY").unwrap_or_else(|_| "USDC".into()),
            operator,
            withdrawal_sla_seconds: std::env::var("GAP_WITHDRAWAL_SLA_SECS")
                .ok()
                .and_then(|v| v.parse().ok()),
            settlement_contract: std::env::var("GAP_ESCROW_ADDRESS").unwrap_or_default(),
        }
    }

    /// Should this amount settle from the ledger rather than on chain?
    pub fn settles_from_balance(&self, amount: Amount) -> bool {
        match self.mode {
            CustodyMode::NonCustodial => false,
            CustodyMode::Custodial => true,
            // At or below the threshold the ledger wins; above it the
            // amount justifies the gas.
            CustodyMode::Hybrid => self.threshold.map(|t| amount <= t).unwrap_or(false),
        }
    }

    /// Everything RFC-0016 requires before this node should be trusted
    /// with value. Returns the reasons it should not be.
    pub fn declaration_gaps(&self) -> Vec<String> {
        let mut gaps = Vec::new();
        if !self.mode.holds_funds() {
            return gaps;
        }
        if self.operator.is_none() {
            gaps.push("custodial node with no declared operator".into());
        }
        if self.withdrawal_sla_seconds.is_none() {
            gaps.push("custodial node with no declared withdrawal SLA".into());
        }
        if self.mode == CustodyMode::Hybrid && self.threshold.is_none() {
            gaps.push("hybrid custody with no declared threshold".into());
        }
        gaps
    }
}

/// One agent's prefunded balance.
///
/// `held` is the part committed to open contracts. It is tracked
/// separately from `available` so that parking twice against the same
/// funds is impossible without inspecting every open contract.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Balance {
    pub available: Amount,
    pub held: Amount,
    /// Requested for withdrawal but not yet paid out.
    ///
    /// A third bucket exists because the second one lied. `withdraw`
    /// used to take funds straight out of `available` and emit a
    /// receipt, while nothing anywhere sent anything: the agent's
    /// balance fell, the money stayed, and the node's own liabilities
    /// figure said it owed less than it did. Funds sitting here have
    /// left the agent's spendable balance and have NOT left the node,
    /// which is the truth in between, and `total` counts them because
    /// they are still owed.
    #[serde(default)]
    pub withdrawing: Amount,
    pub currency: String,
}

impl Balance {
    pub fn total(&self) -> Amount {
        Amount::from_minor(
            self.available.minor_units() + self.held.minor_units() + self.withdrawing.minor_units(),
        )
    }

    /// Credit a deposit.
    pub fn credit(&mut self, amount: Amount) {
        self.available = Amount::from_minor(self.available.minor_units() + amount.minor_units());
    }

    /// Move funds from available to held, for a contract about to run.
    ///
    /// Refuses rather than going negative: a custodian that lets a
    /// balance go below zero has extended credit, which is a different
    /// regulated activity from holding deposits.
    pub fn hold(&mut self, amount: Amount) -> Result<()> {
        if amount.minor_units() > self.available.minor_units() {
            return Err(Error::EscrowViolation(format!(
                "insufficient balance: {} available, {} required",
                self.available.to_string_decimal(),
                amount.to_string_decimal()
            )));
        }
        self.available = Amount::from_minor(self.available.minor_units() - amount.minor_units());
        self.held = Amount::from_minor(self.held.minor_units() + amount.minor_units());
        Ok(())
    }

    /// Release held funds out of this balance (they go to the provider).
    pub fn settle_held(&mut self, amount: Amount) -> Result<()> {
        if amount.minor_units() > self.held.minor_units() {
            return Err(Error::EscrowViolation("more held than exists".into()));
        }
        self.held = Amount::from_minor(self.held.minor_units() - amount.minor_units());
        Ok(())
    }

    /// Return held funds to available (a refund).
    pub fn unhold(&mut self, amount: Amount) -> Result<()> {
        self.settle_held(amount)?;
        self.credit(amount);
        Ok(())
    }

    /// Earmark funds for a payout that has not happened yet.
    ///
    /// Deliberately not a debit: until an operator has actually sent
    /// the money and said so, the node still owes it.
    pub fn start_withdrawal(&mut self, amount: Amount) -> Result<()> {
        self.debit(amount)?;
        self.withdrawing =
            Amount::from_minor(self.withdrawing.minor_units() + amount.minor_units());
        Ok(())
    }

    /// The payout went out. Now, and only now, the node owes less.
    pub fn finish_withdrawal(&mut self, amount: Amount) -> Result<()> {
        if amount.minor_units() > self.withdrawing.minor_units() {
            return Err(Error::EscrowViolation(
                "more withdrawing than exists".into(),
            ));
        }
        self.withdrawing =
            Amount::from_minor(self.withdrawing.minor_units() - amount.minor_units());
        Ok(())
    }

    /// The payout could not be made: give it back rather than strand it.
    pub fn cancel_withdrawal(&mut self, amount: Amount) -> Result<()> {
        self.finish_withdrawal(amount)?;
        self.credit(amount);
        Ok(())
    }

    /// Take funds out. Only `available` may leave.
    pub fn debit(&mut self, amount: Amount) -> Result<()> {
        if amount.minor_units() > self.available.minor_units() {
            return Err(Error::EscrowViolation(format!(
                "insufficient balance: {} available, {} requested",
                self.available.to_string_decimal(),
                amount.to_string_decimal()
            )));
        }
        self.available = Amount::from_minor(self.available.minor_units() - amount.minor_units());
        Ok(())
    }
}

/// A signed statement that holdings cover liabilities (RFC-0016 §6).
///
/// `spine_seq` is what makes it checkable: a verifier replays the audit
/// spine up to that sequence and recomputes `liabilities` itself. An
/// attestation nobody can recompute is decoration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReserveAttestation {
    pub at: u64,
    pub spine_seq: u64,
    pub liabilities: String,
    pub holdings: String,
    pub currency: String,
    pub accounts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl ReserveAttestation {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.signature = None;
        // Same canonical form as every other signed GAP artifact: the
        // signature field is absent from the bytes it covers.
        serde_json::to_vec(&serde_json::to_value(&clone).unwrap_or_default()).unwrap_or_default()
    }

    pub fn sign(&mut self, node: &crate::identity::AgentIdentity) {
        self.signature = None;
        let sig = node.sign(&self.canonical_bytes());
        self.signature = Some(format!("ed25519:{}", sig.to_hex()));
    }

    /// Do the declared holdings cover the declared liabilities?
    ///
    /// A node that answers no is insolvent and should stop accepting
    /// deposits. Reporting it honestly is the whole point of publishing.
    pub fn is_solvent(&self) -> bool {
        match (
            Amount::parse(&self.holdings),
            Amount::parse(&self.liabilities),
        ) {
            (Ok(h), Ok(l)) => h.minor_units() >= l.minor_units(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hold_cannot_push_a_balance_negative() {
        // A custodian whose ledger goes below zero has extended credit,
        // which is a different regulated activity from holding deposits.
        let mut b = Balance {
            available: Amount::parse("1.00").unwrap(),
            ..Default::default()
        };
        assert!(b.hold(Amount::parse("2.00").unwrap()).is_err());
        assert_eq!(b.available.to_string_decimal(), "1.000000");
        assert_eq!(b.held.to_string_decimal(), "0.000000");
    }

    #[test]
    fn held_funds_cannot_be_spent_twice() {
        let mut b = Balance {
            available: Amount::parse("1.00").unwrap(),
            ..Default::default()
        };
        b.hold(Amount::parse("0.60").unwrap()).unwrap();
        // Only 0.40 is left available, even though the total is 1.00.
        assert!(b.hold(Amount::parse("0.60").unwrap()).is_err());
        assert!(b.debit(Amount::parse("0.60").unwrap()).is_err());
        assert_eq!(b.total().to_string_decimal(), "1.000000");
    }

    #[test]
    fn a_refund_returns_held_funds_to_available() {
        let mut b = Balance {
            available: Amount::parse("1.00").unwrap(),
            ..Default::default()
        };
        b.hold(Amount::parse("0.25").unwrap()).unwrap();
        b.unhold(Amount::parse("0.25").unwrap()).unwrap();
        assert_eq!(b.available.to_string_decimal(), "1.000000");
        assert_eq!(b.held.to_string_decimal(), "0.000000");
    }

    #[test]
    fn a_settlement_removes_the_funds_entirely() {
        let mut b = Balance {
            available: Amount::parse("1.00").unwrap(),
            ..Default::default()
        };
        b.hold(Amount::parse("0.25").unwrap()).unwrap();
        b.settle_held(Amount::parse("0.25").unwrap()).unwrap();
        assert_eq!(b.total().to_string_decimal(), "0.750000");
    }

    #[test]
    fn the_threshold_decides_which_rail_a_contract_uses() {
        let policy = CustodyPolicy {
            mode: CustodyMode::Hybrid,
            threshold: Amount::parse("25.00").ok(),
            ..Default::default()
        };
        assert!(policy.settles_from_balance(Amount::parse("0.05").unwrap()));
        assert!(policy.settles_from_balance(Amount::parse("25.00").unwrap()));
        assert!(!policy.settles_from_balance(Amount::parse("25.000001").unwrap()));
    }

    #[test]
    fn a_non_custodial_node_never_settles_from_a_ledger() {
        let policy = CustodyPolicy::default();
        assert_eq!(policy.mode, CustodyMode::NonCustodial);
        assert!(!policy.settles_from_balance(Amount::parse("0.01").unwrap()));
        assert!(!policy.mode.holds_funds());
        // ...and has nothing to declare, because it holds nothing.
        assert!(policy.declaration_gaps().is_empty());
    }

    #[test]
    fn a_custodial_node_that_declares_nothing_is_reported_as_such() {
        // RFC-0016 §4: an anonymous custodian is not a custodian anyone
        // should use. The node says so about itself rather than waiting
        // for a buyer to discover it.
        let policy = CustodyPolicy {
            mode: CustodyMode::Custodial,
            ..Default::default()
        };
        let gaps = policy.declaration_gaps();
        assert!(gaps.iter().any(|g| g.contains("operator")));
        assert!(gaps.iter().any(|g| g.contains("withdrawal SLA")));
    }

    #[test]
    fn hybrid_without_a_threshold_is_an_incomplete_declaration() {
        let policy = CustodyPolicy {
            mode: CustodyMode::Hybrid,
            operator: Some(Operator::default()),
            withdrawal_sla_seconds: Some(86_400),
            ..Default::default()
        };
        assert!(policy
            .declaration_gaps()
            .iter()
            .any(|g| g.contains("threshold")));
    }

    #[test]
    fn an_attestation_is_signed_over_its_own_content() {
        let node = crate::identity::AgentIdentity::generate();
        let mut a = ReserveAttestation {
            at: 1,
            spine_seq: 42,
            liabilities: "10.000000".into(),
            holdings: "12.000000".into(),
            currency: "USDC".into(),
            accounts: vec!["0xabc".into()],
            signature: None,
        };
        a.sign(&node);
        assert!(a.signature.is_some());
        assert!(a.is_solvent());

        // Understating liabilities after signing must not go unnoticed.
        let signed = a.canonical_bytes();
        a.liabilities = "1.000000".into();
        assert_ne!(signed, a.canonical_bytes());
    }

    #[test]
    fn a_shortfall_is_reported_rather_than_rounded_away() {
        let a = ReserveAttestation {
            at: 1,
            spine_seq: 1,
            liabilities: "10.000001".into(),
            holdings: "10.000000".into(),
            currency: "USDC".into(),
            accounts: vec![],
            signature: None,
        };
        assert!(!a.is_solvent(), "one minor unit short is still short");
    }

    #[test]
    fn modes_round_trip_through_their_wire_names() {
        for m in [
            CustodyMode::NonCustodial,
            CustodyMode::Custodial,
            CustodyMode::Hybrid,
        ] {
            assert_eq!(CustodyMode::parse(m.as_str()).unwrap(), m);
        }
        assert!(CustodyMode::parse("sort-of").is_err());
    }
}
