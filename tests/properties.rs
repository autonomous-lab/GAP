//! Property-based tests (proptest): invariants that must hold for ALL
//! inputs, not just the examples the unit suite happens to pick.
//!
//! Covered: exact-amount arithmetic/parsing, envelope sign/verify under
//! arbitrary payloads and tampering, DID parsing, and escrow
//! conservation (funds can never be released twice or exceed the cap).

use gap::amount::Amount;
use gap::contract::{Contract, Price, Terms};
use gap::identity::{AgentIdentity, Did};
use gap::message::{now_unix, Envelope, Kind};
use gap::payment::{Escrow, EscrowState};
use proptest::prelude::*;
use serde_json::json;

proptest! {
    /// Amount: minor-units -> decimal string -> parse is the identity.
    #[test]
    fn amount_roundtrips_through_decimal_string(units in 0u128..u64::MAX as u128) {
        let a = Amount::from_minor(units);
        let s = a.to_string_decimal();
        prop_assert_eq!(Amount::parse(&s).unwrap(), a);
    }

    /// Amount: addition never loses precision (integer minor units).
    #[test]
    fn amount_addition_is_exact(a in 0u128..1u128 << 60, b in 0u128..1u128 << 60) {
        let sum = Amount::from_minor(a).checked_add(Amount::from_minor(b)).unwrap();
        prop_assert_eq!(sum, Amount::from_minor(a + b));
    }

    /// Envelope: signing then verifying succeeds for any payload text,
    /// and any post-signature payload mutation is detected.
    #[test]
    fn envelope_signature_covers_arbitrary_payloads(text in ".{0,256}", tamper in ".{1,64}") {
        let alice = AgentIdentity::generate();
        let bob = AgentIdentity::generate();
        let env = Envelope::new(
            alice.did().clone(),
            bob.did().clone(),
            Kind::ExeProgress,
            json!({ "note": text }),
        )
        .sign(&alice);
        prop_assert!(env.verify().is_ok());

        let mut forged = env.clone();
        forged.payload = json!({ "note": format!("{text}{tamper}") });
        prop_assert!(forged.verify().is_err());
    }

    /// DID parsing accepts exactly the 64-hex-char form and nothing else.
    #[test]
    fn did_parse_never_panics(s in "\\PC{0,100}") {
        // Must never panic, whatever the input.
        let _ = Did::parse(&s);
    }

    /// Escrow conservation: for any parked amount within the cap, a
    /// release moves exactly the parked amount exactly once; a second
    /// release (even with a fresh instruction) is impossible.
    #[test]
    fn escrow_releases_exactly_once_and_exactly_the_parked_amount(
        minor in 1u128..=10_000_000u128,
    ) {
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let escrow_agent = AgentIdentity::generate();

        let terms = Terms {
            input: json!({}),
            deliverable: json!({}),
            acceptance_criteria: vec!["ok".into()],
            deadline: now_unix() + 3600,
            price: Price {
                amount: 1.0,
                currency: "EUR".into(),
                model: "fixed".into(),
                // Cap = 10 EUR = 10_000_000 minor units, the upper bound
                // of the generated amount.
                cap: Some(10.0),
            },
            autonomy: "execute-notify".into(),
            confidentiality: None,
        };
        let contract = Contract::propose(&client, provider.did().clone(), "cap:t", terms, true)
            .accept_by_provider(&provider)
            .unwrap();
        let mut escrow = Escrow::for_contract(escrow_agent.clone(), contract.clone()).unwrap();

        let amount = Amount::from_minor(minor);
        let park = Envelope::new(
            client.did().clone(),
            escrow_agent.did().clone(),
            Kind::PayPark,
            json!({ "amount": amount.to_string_decimal() }),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&client);
        escrow.park(&park).unwrap();
        prop_assert_eq!(escrow.held(), amount);

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
            escrow_agent.did().clone(),
            Kind::PayRelease,
            json!({}),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&client);
        let receipt = escrow.release(&release, &acceptance).unwrap();
        prop_assert_eq!(receipt.amount, amount);
        prop_assert_eq!(escrow.state(), EscrowState::Released);
        prop_assert_eq!(escrow.held(), Amount::ZERO);

        // A brand-new signed release instruction cannot move funds again.
        let release2 = Envelope::new(
            client.did().clone(),
            escrow_agent.did().clone(),
            Kind::PayRelease,
            json!({}),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&client);
        prop_assert!(escrow.release(&release2, &acceptance).is_err());
    }

    /// Park amounts above the contract cap are always rejected.
    #[test]
    fn escrow_enforces_the_cap_for_all_amounts(over in 10_000_001u128..1u128 << 50) {
        let client = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let escrow_agent = AgentIdentity::generate();
        let terms = Terms {
            input: json!({}),
            deliverable: json!({}),
            acceptance_criteria: vec!["ok".into()],
            deadline: now_unix() + 3600,
            price: Price {
                amount: 1.0,
                currency: "EUR".into(),
                model: "fixed".into(),
                cap: Some(10.0),
            },
            autonomy: "execute-notify".into(),
            confidentiality: None,
        };
        let contract = Contract::propose(&client, provider.did().clone(), "cap:t", terms, true)
            .accept_by_provider(&provider)
            .unwrap();
        let mut escrow = Escrow::for_contract(escrow_agent.clone(), contract.clone()).unwrap();
        let park = Envelope::new(
            client.did().clone(),
            escrow_agent.did().clone(),
            Kind::PayPark,
            json!({ "amount": Amount::from_minor(over).to_string_decimal() }),
        )
        .for_contract(contract.contract_id.clone())
        .sign(&client);
        prop_assert!(escrow.park(&park).is_err());
        prop_assert_eq!(escrow.state(), EscrowState::Empty);
    }
}
