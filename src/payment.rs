//! Payment layer (GAP spec part 05).
//!
//! Payment turns the protocol into an economy: atomic, escrowed, and
//! auditable. The escrow agent only acts on signed instructions that
//! reference a signed contract.

use crate::amount::Amount;
use crate::error::{Error, Result};
use crate::identity::AgentIdentity;
use serde::{Deserialize, Serialize};

/// Escrow settlement state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EscrowState {
    Empty,
    Parked,
    Released,
    Refunded,
    Disputed,
    Ruled,
}

impl EscrowState {
    pub fn wire_name(&self) -> &'static str {
        match self {
            EscrowState::Empty => "empty",
            EscrowState::Parked => "parked",
            EscrowState::Released => "released",
            EscrowState::Refunded => "refunded",
            EscrowState::Disputed => "disputed",
            EscrowState::Ruled => "ruled",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "empty" => EscrowState::Empty,
            "parked" => EscrowState::Parked,
            "released" => EscrowState::Released,
            "refunded" => EscrowState::Refunded,
            "disputed" => EscrowState::Disputed,
            "ruled" => EscrowState::Ruled,
            _ => return Err(Error::UnknownState(format!("escrow: {s}"))),
        })
    }
}

/// A signed receipt for every escrow transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub receipt_id: String,
    pub contract_id: String,
    pub event: String,
    pub amount: Amount,
    pub currency: String,
    pub from: String,
    pub to: String,
    pub at: u64,
    #[serde(default)]
    pub escrow_sig: Option<String>,
}

impl Receipt {
    pub fn signed(
        escrow: &AgentIdentity,
        contract_id: &str,
        event: &str,
        amount: Amount,
        currency: &str,
        from: &str,
        to: &str,
    ) -> Self {
        let mut r = Self {
            receipt_id: crate::new_id("rcpt"),
            contract_id: contract_id.to_string(),
            event: event.to_string(),
            amount,
            currency: currency.to_string(),
            from: from.to_string(),
            to: to.to_string(),
            at: crate::message::now_unix(),
            escrow_sig: None,
        };
        r.escrow_sig = Some(escrow.sign(&r.canonical_bytes()).to_hex());
        r
    }

    pub fn verify(&self, escrow: &AgentIdentity) -> Result<()> {
        self.verify_against(escrow.did())
    }

    /// Verify the escrow signature against a DID only (distributed form).
    pub fn verify_against(&self, escrow_did: &crate::identity::Did) -> Result<()> {
        let sig_hex = self.escrow_sig.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            escrow_did,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.escrow_sig = None;
        let v = serde_json::to_value(&clone).expect("receipt serializes");
        serde_json::to_vec(&v).expect("receipt serializes")
    }
}

/// An escrow agent holding funds for **one** contract.
///
/// **Security model:** every transition (`park`, `release`, `refund`,
/// `dispute`) is triggered by a signed [`Envelope`](crate::message::Envelope)
/// from the authorized party, referencing the contract this escrow was
/// created for — verified as signed by both parties at construction.
/// The escrow NEVER acts on raw strings or unsigned instructions, and
/// its [`ReplayGuard`](crate::message::ReplayGuard) rejects any
/// instruction envelope presented twice (or outside the freshness
/// window).
///
/// One escrow instance per contract: the previous multi-contract map
/// shared a single `(state, held)` pair, so registering a second
/// contract silently made it unusable ("funds already parked").
pub struct Escrow {
    identity: AgentIdentity,
    state: EscrowState,
    held: Amount,
    currency: String,
    /// The signed contract this escrow settles (verified at construction).
    contract: crate::contract::Contract,
    /// Append-only receipt log (the audit trail).
    log: Vec<Receipt>,
    /// Instruction replay protection (freshness + message_id dedup).
    guard: crate::message::ReplayGuard,
    /// Freshness window applied to instruction envelopes.
    max_age_secs: u64,
}

impl Escrow {
    /// Create an escrow for a signed contract. Rejects unsigned contracts.
    pub fn for_contract(
        identity: AgentIdentity,
        contract: crate::contract::Contract,
    ) -> Result<Self> {
        contract.verify_signed()?;
        // The currency is the one the parties signed, not a house
        // default. Hardcoding EUR here meant a contract priced in USDC
        // settled with a receipt that said EUR - the amount was right
        // and the unit was a lie, which is the kind of discrepancy an
        // accounting system inherits and never questions.
        let currency = contract.terms.price.currency.clone();
        Ok(Self {
            identity,
            state: EscrowState::Empty,
            held: Amount::ZERO,
            currency,
            contract,
            log: vec![],
            guard: crate::message::ReplayGuard::new(),
            max_age_secs: crate::message::RECOMMENDED_MAX_AGE_SECS,
        })
    }

    /// Rebuild an escrow from persisted materialized state.
    pub fn restore(
        identity: AgentIdentity,
        contract: crate::contract::Contract,
        state: EscrowState,
        held: Amount,
        currency: String,
    ) -> Result<Self> {
        contract.verify_signed()?;
        Ok(Self {
            identity,
            state,
            held,
            currency,
            contract,
            log: vec![],
            guard: crate::message::ReplayGuard::new(),
            max_age_secs: crate::message::RECOMMENDED_MAX_AGE_SECS,
        })
    }

    /// Check that an instruction targets this escrow's contract, then
    /// run full envelope validation + replay rejection.
    fn admit(&mut self, instruction: &crate::message::Envelope, what: &str) -> Result<()> {
        let cid = instruction
            .contract_id
            .as_deref()
            .ok_or_else(|| Error::EscrowViolation(format!("{what} missing contract_id")))?;
        if cid != self.contract.contract_id {
            return Err(Error::EscrowViolation(format!(
                "{what} for different contract {cid}"
            )));
        }
        self.guard.check(instruction, self.max_age_secs)
    }

    /// `pay.park` — the CLIENT instructs escrow to hold funds.
    ///
    /// The instruction must be a signed envelope from the contract's
    /// client, for `Kind::PayPark`, and the amount must not exceed the
    /// contract's price cap.
    pub fn park(&mut self, instruction: &crate::message::Envelope) -> Result<Receipt> {
        let contract = self.contract.clone();
        let cid = contract.contract_id.clone();
        // Only the client may park.
        if instruction.from != contract.client {
            return Err(Error::Unauthorized("only the client may park funds".into()));
        }
        if instruction.kind != crate::message::Kind::PayPark {
            return Err(Error::EscrowViolation(
                "expected pay.park instruction".into(),
            ));
        }
        self.admit(instruction, "park instruction")?;

        let payload = instruction.decode::<serde_json::Value>()?;
        let amount = parse_amount_value(payload.get("amount"))?;
        // Enforce the price cap from the contract.
        if let Some(cap) = contract.terms.price.cap {
            let cap_amount = Amount::from_f64_rounding(cap);
            if amount > cap_amount {
                return Err(Error::EscrowViolation(format!(
                    "park amount {amount} exceeds contract cap {cap_amount}"
                )));
            }
        }
        if self.state != EscrowState::Empty {
            return Err(Error::EscrowViolation("funds already parked".into()));
        }
        self.state = EscrowState::Parked;
        self.held = amount;
        let client = contract.client.to_string();
        let provider = contract.provider.to_string();
        let r = Receipt::signed(
            &self.identity,
            &cid,
            "pay.parked",
            amount,
            &self.currency,
            &client,
            &provider,
        );
        self.log.push(r.clone());
        Ok(r)
    }

    /// `pay.release` — release funds to the provider after `exe.accept`.
    ///
    /// Requires a signed `pay.release` instruction AND a verified
    /// `exe.accept` envelope referencing the same contract.
    pub fn release(
        &mut self,
        instruction: &crate::message::Envelope,
        acceptance: &crate::message::Envelope,
    ) -> Result<Receipt> {
        let contract = self.contract.clone();
        let cid = contract.contract_id.clone();
        if instruction.from != contract.client {
            return Err(Error::Unauthorized(
                "only the client may release funds".into(),
            ));
        }
        if instruction.kind != crate::message::Kind::PayRelease {
            return Err(Error::EscrowViolation(
                "expected pay.release instruction".into(),
            ));
        }
        self.admit(instruction, "release instruction")?;
        // The acceptance must be a valid exe.accept from the client for
        // the same contract.
        if acceptance.contract_id.as_deref() != Some(cid.as_str()) {
            return Err(Error::EscrowViolation(
                "acceptance for different contract".into(),
            ));
        }
        if acceptance.kind != crate::message::Kind::ExeAccept {
            return Err(Error::EscrowViolation(
                "expected exe.accept envelope".into(),
            ));
        }
        if acceptance.from != contract.client {
            return Err(Error::Unauthorized("only the client may accept".into()));
        }
        acceptance.verify()?;

        if self.state != EscrowState::Parked {
            return Err(Error::EscrowViolation("funds not parked".into()));
        }
        let amount = self.held;
        self.state = EscrowState::Released;
        self.held = Amount::ZERO;
        let client = contract.client.to_string();
        let provider = contract.provider.to_string();
        let r = Receipt::signed(
            &self.identity,
            &cid,
            "pay.released",
            amount,
            &self.currency,
            &client,
            &provider,
        );
        self.log.push(r.clone());
        Ok(r)
    }

    /// `pay.refund` — full refund to the client (after cancellation or a
    /// ruling against the provider). Requires a signed instruction from
    /// the client.
    pub fn refund(&mut self, instruction: &crate::message::Envelope) -> Result<Receipt> {
        let contract = self.contract.clone();
        let cid = contract.contract_id.clone();
        if instruction.from != contract.client {
            return Err(Error::Unauthorized(
                "only the client may request a refund".into(),
            ));
        }
        if instruction.kind != crate::message::Kind::PayRefund {
            return Err(Error::EscrowViolation(
                "expected pay.refund instruction".into(),
            ));
        }
        self.admit(instruction, "refund instruction")?;

        if self.state != EscrowState::Parked {
            return Err(Error::EscrowViolation("funds not parked".into()));
        }
        let amount = self.held;
        self.state = EscrowState::Refunded;
        self.held = Amount::ZERO;
        let client = contract.client.to_string();
        let r = Receipt::signed(
            &self.identity,
            &cid,
            "pay.refunded",
            amount,
            &self.currency,
            &client,
            "escrow",
        );
        self.log.push(r.clone());
        Ok(r)
    }

    /// `pay.dispute` — hold funds during arbitration.
    pub fn dispute(&mut self, instruction: &crate::message::Envelope) -> Result<Receipt> {
        let contract = self.contract.clone();
        let cid = contract.contract_id.clone();
        if instruction.from != contract.client {
            return Err(Error::Unauthorized("only the client may dispute".into()));
        }
        if instruction.kind != crate::message::Kind::PayDispute {
            return Err(Error::EscrowViolation(
                "expected pay.dispute instruction".into(),
            ));
        }
        self.admit(instruction, "dispute instruction")?;

        if self.state != EscrowState::Parked {
            return Err(Error::EscrowViolation("funds not parked".into()));
        }
        self.state = EscrowState::Disputed;
        let amount = self.held;
        let client = contract.client.to_string();
        let provider = contract.provider.to_string();
        let r = Receipt::signed(
            &self.identity,
            &cid,
            "pay.disputed",
            amount,
            &self.currency,
            &client,
            &provider,
        );
        self.log.push(r.clone());
        Ok(r)
    }

    /// `pay.ruled` — execute an arbitrator's signed ruling on disputed
    /// funds. The ruling is an envelope from the agreed arbitrator DID,
    /// carrying a `split` between client and provider (fractions in
    /// [0.0, 1.0] that sum to 1.0).
    pub fn rule(
        &mut self,
        ruling: &crate::message::Envelope,
        arbitrator_did: &crate::identity::Did,
    ) -> Result<Receipt> {
        let cid = self.contract.contract_id.clone();
        if ruling.kind != crate::message::Kind::CtrRuling {
            return Err(Error::EscrowViolation(
                "expected ctr.ruling envelope".into(),
            ));
        }
        if &ruling.from != arbitrator_did {
            return Err(Error::Unauthorized(
                "ruling from unapproved arbitrator".into(),
            ));
        }
        self.admit(ruling, "ruling")?;
        if self.state != EscrowState::Disputed {
            return Err(Error::EscrowViolation("funds not in dispute".into()));
        }

        let v: serde_json::Value = ruling.decode()?;
        let split = v
            .get("split")
            .ok_or_else(|| Error::EscrowViolation("ruling missing split".into()))?;
        let client_share = split
            .get("client")
            .and_then(|x| x.as_f64())
            .ok_or_else(|| Error::EscrowViolation("split.client missing".into()))?;
        let provider_share = split
            .get("provider")
            .and_then(|x| x.as_f64())
            .ok_or_else(|| Error::EscrowViolation("split.provider missing".into()))?;
        if (client_share + provider_share - 1.0).abs() > 1e-9 {
            return Err(Error::EscrowViolation("split must sum to 1.0".into()));
        }

        let amount = self.held;
        self.state = EscrowState::Ruled;
        self.held = Amount::ZERO;
        let client = self.contract.client.to_string();
        let provider = self.contract.provider.to_string();
        let r = Receipt::signed(
            &self.identity,
            &cid,
            "pay.ruled",
            amount,
            &self.currency,
            &client,
            &provider,
        );
        self.log.push(r.clone());
        Ok(r)
    }

    pub fn state(&self) -> EscrowState {
        self.state
    }

    /// The contract this escrow settles.
    pub fn contract(&self) -> &crate::contract::Contract {
        &self.contract
    }

    /// The escrow agent's DID (public, for addressing instructions).
    pub fn did(&self) -> &crate::identity::Did {
        self.identity.did()
    }

    pub fn held(&self) -> Amount {
        self.held
    }

    pub fn currency(&self) -> &str {
        &self.currency
    }

    /// The full, append-only audit trail.
    pub fn audit_log(&self) -> &[Receipt] {
        &self.log
    }
}

fn parse_amount_value(value: Option<&serde_json::Value>) -> Result<Amount> {
    match value {
        Some(v) if v.is_string() => Amount::parse(v.as_str().unwrap_or("")),
        Some(v) if v.is_number() => Ok(Amount::from_f64_rounding(v.as_f64().unwrap_or(0.0))),
        _ => Err(Error::EscrowViolation("park amount missing".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{Contract, Price, Terms};
    use crate::message::{Envelope, Kind};
    use serde_json::json;

    fn signed_contract(client: &AgentIdentity, provider: &AgentIdentity) -> Contract {
        let terms = Terms {
            input: json!({}),
            deliverable: json!({}),
            acceptance_criteria: vec!["ok".into()],
            deadline: crate::message::now_unix() + 86400,
            price: Price {
                amount: 1.0,
                currency: "EUR".into(),
                model: "fixed".into(),
                cap: Some(10.0),
            },
            autonomy: "execute-notify".into(),
            confidentiality: None,
            human_review_above: None,
            cooling_off_seconds: None,
        };
        Contract::propose(client, provider.did().clone(), "cap:test", terms, true)
            .accept_by_provider(provider)
            .unwrap()
    }

    fn park_instruction(
        client: &AgentIdentity,
        escrow: &AgentIdentity,
        contract: &Contract,
        amount: f64,
    ) -> Envelope {
        Envelope::new(
            client.did().clone(),
            escrow.did().clone(),
            Kind::PayPark,
            json!({ "amount": amount }),
        )
        .for_contract(contract.contract_id.clone())
        .sign(client)
    }

    #[test]
    fn escrow_lifecycle_with_signed_instructions() {
        let escrow_id = AgentIdentity::generate();
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let contract = signed_contract(&client, &provider);
        let mut escrow = Escrow::for_contract(escrow_id.clone(), contract.clone()).unwrap();

        let park = park_instruction(&client, &escrow_id, &contract, 5.0);
        escrow.park(&park).unwrap();
        assert_eq!(escrow.state(), EscrowState::Parked);
        assert_eq!(escrow.held(), Amount::parse("5.0").unwrap());

        let acceptance = Envelope::new(
            client.did().clone(),
            provider.did().clone(),
            Kind::ExeAccept,
            json!({ "verdict": "accepted" }),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&client);
        let release = Envelope::new(
            client.did().clone(),
            escrow_id.did().clone(),
            Kind::PayRelease,
            json!({}),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&client);
        escrow.release(&release, &acceptance).unwrap();
        assert_eq!(escrow.state(), EscrowState::Released);
        // cannot release twice
        assert!(escrow.release(&release, &acceptance).is_err());
        // audit trail has both receipts
        assert_eq!(escrow.audit_log().len(), 2);
    }

    #[test]
    fn escrow_rejects_unsigned_instructions() {
        let escrow_id = AgentIdentity::generate();
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let contract = signed_contract(&client, &provider);
        let mut escrow = Escrow::for_contract(escrow_id.clone(), contract.clone()).unwrap();

        // Unsigned park instruction -> must fail
        let unsigned = Envelope::new(
            client.did().clone(),
            escrow_id.did().clone(),
            Kind::PayPark,
            json!({ "amount": 5.0 }),
        )
        .for_contract(contract.contract_id.clone());
        assert!(escrow.park(&unsigned).is_err());
        assert_eq!(escrow.state(), EscrowState::Empty);
    }

    #[test]
    fn escrow_rejects_instructions_from_wrong_party() {
        let escrow_id = AgentIdentity::generate();
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let attacker = AgentIdentity::generate();
        let contract = signed_contract(&client, &provider);
        let mut escrow = Escrow::for_contract(escrow_id.clone(), contract.clone()).unwrap();

        // The provider tries to park (only the client may park)
        let forged = Envelope::new(
            provider.did().clone(),
            escrow_id.did().clone(),
            Kind::PayPark,
            json!({ "amount": 5.0 }),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&provider);
        assert!(escrow.park(&forged).is_err());
        assert_eq!(escrow.state(), EscrowState::Empty);

        // An unrelated attacker tries to park
        let forged2 = Envelope::new(
            attacker.did().clone(),
            escrow_id.did().clone(),
            Kind::PayPark,
            json!({ "amount": 5.0 }),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&attacker);
        assert!(escrow.park(&forged2).is_err());
    }

    #[test]
    fn escrow_enforces_price_cap() {
        let escrow_id = AgentIdentity::generate();
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let contract = signed_contract(&client, &provider); // cap = 10.0
        let mut escrow = Escrow::for_contract(escrow_id.clone(), contract.clone()).unwrap();

        let over_cap = park_instruction(&client, &escrow_id, &contract, 999.0);
        assert!(escrow.park(&over_cap).is_err());
        assert_eq!(escrow.state(), EscrowState::Empty);
    }

    #[test]
    fn escrow_rejects_release_without_acceptance() {
        let escrow_id = AgentIdentity::generate();
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let contract = signed_contract(&client, &provider);
        let mut escrow = Escrow::for_contract(escrow_id.clone(), contract.clone()).unwrap();

        let park = park_instruction(&client, &escrow_id, &contract, 5.0);
        escrow.park(&park).unwrap();

        let release = Envelope::new(
            client.did().clone(),
            escrow_id.did().clone(),
            Kind::PayRelease,
            json!({}),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&client);
        // Missing acceptance envelope -> must fail
        let fake_acceptance = Envelope::new(
            client.did().clone(),
            provider.did().clone(),
            Kind::ExeReject,
            json!({}),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&client);
        assert!(escrow.release(&release, &fake_acceptance).is_err());
        assert_eq!(escrow.state(), EscrowState::Parked);
    }

    #[test]
    fn escrow_refund_flow() {
        let escrow_id = AgentIdentity::generate();
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let contract = signed_contract(&client, &provider);
        let mut escrow = Escrow::for_contract(escrow_id.clone(), contract.clone()).unwrap();

        let park = park_instruction(&client, &escrow_id, &contract, 5.0);
        escrow.park(&park).unwrap();

        let refund = Envelope::new(
            client.did().clone(),
            escrow_id.did().clone(),
            Kind::PayRefund,
            json!({}),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&client);
        let receipt = escrow.refund(&refund).unwrap();
        assert_eq!(escrow.state(), EscrowState::Refunded);
        assert_eq!(receipt.event, "pay.refunded");
        assert_eq!(escrow.held(), Amount::ZERO);
    }

    #[test]
    fn escrow_never_releases_more_than_held() {
        let escrow_id = AgentIdentity::generate();
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let contract = signed_contract(&client, &provider);
        let mut escrow = Escrow::for_contract(escrow_id.clone(), contract.clone()).unwrap();

        let park = park_instruction(&client, &escrow_id, &contract, 5.0);
        escrow.park(&park).unwrap();

        let acceptance = Envelope::new(
            client.did().clone(),
            provider.did().clone(),
            Kind::ExeAccept,
            json!({}),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&client);
        let release = Envelope::new(
            client.did().clone(),
            escrow_id.did().clone(),
            Kind::PayRelease,
            json!({}),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&client);
        escrow.release(&release, &acceptance).unwrap();
        assert_eq!(escrow.state(), EscrowState::Released);
        assert_eq!(escrow.held(), Amount::ZERO);
    }

    fn disputed_escrow(escrow_agent: &AgentIdentity) -> (Escrow, Contract) {
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let contract = signed_contract(&client, &provider);
        let mut escrow = Escrow::for_contract(escrow_agent.clone(), contract.clone()).unwrap();
        escrow
            .park(&park_instruction(&client, escrow_agent, &contract, 5.0))
            .unwrap();
        let dispute = Envelope::new(
            client.did().clone(),
            escrow_agent.did().clone(),
            Kind::PayDispute,
            json!({ "reason": "nonconforming" }),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&client);
        escrow.dispute(&dispute).unwrap();
        (escrow, contract)
    }

    #[test]
    fn escrow_ruling_split_must_sum_to_one() {
        let escrow_agent = AgentIdentity::generate();
        let arbitrator = AgentIdentity::generate();
        let (mut escrow, contract) = disputed_escrow(&escrow_agent);

        // Split sums to 1.5 -> rejected.
        let bad_ruling = Envelope::new(
            arbitrator.did().clone(),
            escrow_agent.did().clone(),
            Kind::CtrRuling,
            json!({ "split": { "client": 1.0, "provider": 0.5 } }),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&arbitrator);
        assert!(escrow.rule(&bad_ruling, arbitrator.did()).is_err());
        assert_eq!(escrow.state(), EscrowState::Disputed);
    }

    #[test]
    fn escrow_ruling_rejects_unauthorized_arbitrator() {
        let escrow_agent = AgentIdentity::generate();
        let impostor = AgentIdentity::generate();
        let (mut escrow, contract) = disputed_escrow(&escrow_agent);

        let ruling = Envelope::new(
            impostor.did().clone(),
            escrow_agent.did().clone(),
            Kind::CtrRuling,
            json!({ "split": { "client": 0.0, "provider": 1.0 } }),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&impostor);
        // The trusted arbitrator is a different DID.
        let trusted = AgentIdentity::generate();
        assert!(escrow.rule(&ruling, trusted.did()).is_err());
    }

    #[test]
    fn escrow_ruling_requires_disputed_state() {
        let escrow_agent = AgentIdentity::generate();
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let contract = signed_contract(&client, &provider);
        let mut escrow = Escrow::for_contract(escrow_agent.clone(), contract.clone()).unwrap();
        // Parked but NOT disputed.
        escrow
            .park(&park_instruction(&client, &escrow_agent, &contract, 5.0))
            .unwrap();

        let arbitrator = AgentIdentity::generate();
        let ruling = Envelope::new(
            arbitrator.did().clone(),
            escrow_agent.did().clone(),
            Kind::CtrRuling,
            json!({ "split": { "client": 0.0, "provider": 1.0 } }),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&arbitrator);
        assert!(escrow.rule(&ruling, arbitrator.did()).is_err());
    }

    #[test]
    fn escrow_refund_rejects_non_client() {
        let escrow_agent = AgentIdentity::generate();
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let contract = signed_contract(&client, &provider);
        let mut escrow = Escrow::for_contract(escrow_agent.clone(), contract.clone()).unwrap();
        escrow
            .park(&park_instruction(&client, &escrow_agent, &contract, 5.0))
            .unwrap();

        // Provider (not client) tries to refund -> unauthorized.
        let refund = Envelope::new(
            provider.did().clone(),
            escrow_agent.did().clone(),
            Kind::PayRefund,
            json!({}),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&provider);
        assert!(escrow.refund(&refund).is_err());
        assert_eq!(escrow.state(), EscrowState::Parked);
    }

    #[test]
    fn escrow_register_rejects_unsigned_contract() {
        let escrow_agent = AgentIdentity::generate();
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        // Only the client has signed.
        let contract = Contract::propose(
            &client,
            provider.did().clone(),
            "cap:test",
            crate::contract::Terms {
                input: json!({}),
                deliverable: json!({}),
                acceptance_criteria: vec!["ok".into()],
                deadline: crate::message::now_unix() + 86400,
                price: crate::contract::Price {
                    amount: 1.0,
                    currency: "EUR".into(),
                    model: "fixed".into(),
                    cap: Some(10.0),
                },
                autonomy: "execute-notify".into(),
                confidentiality: None,
                human_review_above: None,
                cooling_off_seconds: None,
            },
            true,
        );
        assert!(Escrow::for_contract(escrow_agent, contract).is_err());
    }

    #[test]
    fn receipt_verifies_by_did() {
        let escrow_agent = AgentIdentity::generate();
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let contract = signed_contract(&client, &provider);
        let mut escrow = Escrow::for_contract(escrow_agent.clone(), contract.clone()).unwrap();
        let park = escrow
            .park(&park_instruction(&client, &escrow_agent, &contract, 5.0))
            .unwrap();
        assert!(park.verify_against(escrow_agent.did()).is_ok());
        // Wrong DID fails.
        let stranger = AgentIdentity::generate();
        assert!(park.verify_against(stranger.did()).is_err());
    }
}
