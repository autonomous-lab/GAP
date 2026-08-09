//! Known-answer test vectors (spec/test-vectors.md).
//!
//! These pin the wire format and the signing scheme: any implementation
//! (this one included) that produces different bytes for these inputs
//! is NOT interoperable. If a change here is intentional, it is a
//! breaking protocol change — bump the protocol version and regenerate
//! spec/test-vectors.md.

use gap::identity::AgentIdentity;
use gap::message::{Envelope, Kind};
use serde_json::json;

const ALICE_DID: &str = "did:gap:8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c";
const BOB_DID: &str = "did:gap:8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394";
const ENVELOPE_SIG: &str = "a5e73faad727b2600a2ffeaa0ebaa0a1a67cc29825fcca241072f116bd725bd7ffb8db7cfc27d19d63bbf24a8e42584296218fab2aa99f11d1531cda0184ff0d";
const RAW_SIG: &str = "f5101f5b5c02f944f115fcd805115517db1f5dd04ffdf6a4a2b6934ab872e2e8df99a07c20b8ee01cae0db1b3257cff4e9f411250f88e0eec681b4902d6de004";
const WIRE: &str = r#"{"protocol":"gap","version":"0.1.0","message_id":"urn:gap:msg:00000000000000000000000000000001","from":"did:gap:8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c","to":"did:gap:8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394","contract_id":"urn:gap:ctr:00000000000000000000000000000002","kind":"ctr.propose","timestamp":1754000000,"payload":{"n":1,"note":"test-vector"},"signature":"a5e73faad727b2600a2ffeaa0ebaa0a1a67cc29825fcca241072f116bd725bd7ffb8db7cfc27d19d63bbf24a8e42584296218fab2aa99f11d1531cda0184ff0d"}"#;

fn alice() -> AgentIdentity {
    AgentIdentity::from_seed(&[1u8; 32])
}

fn bob() -> AgentIdentity {
    AgentIdentity::from_seed(&[2u8; 32])
}

#[test]
fn did_derivation_vectors() {
    assert_eq!(alice().did().to_string(), ALICE_DID);
    assert_eq!(bob().did().to_string(), BOB_DID);
}

#[test]
fn raw_signature_vector() {
    let sig = alice().sign(b"gap-test-vector");
    assert_eq!(sig.to_hex(), RAW_SIG);
    assert!(alice().verify(b"gap-test-vector", &sig));
}

fn vector_envelope() -> Envelope {
    let mut env = Envelope::new(
        alice().did().clone(),
        bob().did().clone(),
        Kind::CtrPropose,
        json!({ "note": "test-vector", "n": 1 }),
    );
    env.message_id = "urn:gap:msg:00000000000000000000000000000001".into();
    env.timestamp = 1_754_000_000;
    env.for_contract("urn:gap:ctr:00000000000000000000000000000002")
        .sign(&alice())
}

#[test]
fn envelope_signature_vector() {
    let env = vector_envelope();
    assert_eq!(env.signature.as_deref(), Some(ENVELOPE_SIG));
    assert!(env.verify().is_ok());
}

#[test]
fn envelope_wire_form_vector() {
    // The serialized wire form is byte-exact, including the dotted kind
    // taxonomy ("ctr.propose", not a collapsed variant name).
    let env = vector_envelope();
    assert_eq!(serde_json::to_string(&env).unwrap(), WIRE);

    // And it round-trips: parsing the wire form verifies.
    let parsed: Envelope = serde_json::from_str(WIRE).unwrap();
    assert!(parsed.verify().is_ok());
    assert_eq!(parsed.kind, Kind::CtrPropose);
}

#[test]
fn canonicalization_sorts_keys_and_strips_whitespace() {
    // The same envelope with payload keys given in a different order
    // must produce the same signature: canonical form sorts keys.
    let mut env = Envelope::new(
        alice().did().clone(),
        bob().did().clone(),
        Kind::CtrPropose,
        json!({ "n": 1, "note": "test-vector" }), // reversed insertion order
    );
    env.message_id = "urn:gap:msg:00000000000000000000000000000001".into();
    env.timestamp = 1_754_000_000;
    let env = env
        .for_contract("urn:gap:ctr:00000000000000000000000000000002")
        .sign(&alice());
    assert_eq!(env.signature.as_deref(), Some(ENVELOPE_SIG));
}
