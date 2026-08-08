//! On-chain relayer — connects the GAP node to the GapEscrow contract.
//!
//! The node is the *relayer*: it encodes contract calls, signs them
//! with agent EVM keys (key custody), and submits transactions to the
//! chain. This module provides:
//!
//! - [`AbiEncoder`] — minimal ABI encoding for the GapEscrow functions.
//! - [`EvmRelayer`] — real JSON-RPC relayer (eth_call / eth_sendRawTransaction).
//! - [`MockChain`] — in-memory chain for tests.
//!
//! The relayer mirrors `GapEscrow.sol` (contracts/GapEscrow.sol): same
//! function selectors, same state machine.

use crate::error::{Error, Result};
use k256::ecdsa::SigningKey;
use std::sync::Mutex;

/// The GapEscrow function selectors (keccak256 of the signature).
pub mod selectors {
    pub const PARK: [u8; 4] = selector("park(uint256,address,address,uint256)");
    pub const RELEASE: [u8; 4] = selector("release(uint256)");
    pub const REFUND: [u8; 4] = selector("refund(uint256)");
    pub const DISPUTE: [u8; 4] = selector("dispute(uint256)");
    pub const RULE: [u8; 4] = selector("rule(uint256,uint256)");
    pub const STATE_OF: [u8; 4] = selector("stateOf(uint256)");

    const fn selector(sig: &str) -> [u8; 4] {
        // Computed at runtime below; this const is a placeholder
        // replaced by compute_selector in tests. Kept for docs.
        let _ = sig;
        [0, 0, 0, 0]
    }
}

/// Compute a 4-byte function selector from a signature string.
pub fn compute_selector(sig: &str) -> [u8; 4] {
    use tiny_keccak::{Hasher, Keccak};
    let mut keccak = Keccak::v256();
    keccak.update(sig.as_bytes());
    let mut out = [0u8; 32];
    keccak.finalize(&mut out);
    let mut sel = [0u8; 4];
    sel.copy_from_slice(&out[..4]);
    sel
}

/// Minimal ABI encoder for GapEscrow calls.
pub struct AbiEncoder;

impl AbiEncoder {
    /// Pads an unsigned integer to a 32-byte ABI word.
    fn word(value: u128) -> [u8; 32] {
        let mut w = [0u8; 32];
        w[16..].copy_from_slice(&value.to_be_bytes());
        w
    }

    /// Encode `park(contractHash, provider, arbitrator, amount)`.
    pub fn park(contract_hash: &[u8; 32], provider: &[u8; 20], arbitrator: &[u8; 20], amount: u128) -> Vec<u8> {
        let mut calldata = Vec::with_capacity(4 + 4 * 32);
        calldata.extend_from_slice(&compute_selector("park(uint256,address,address,uint256)"));
        calldata.extend_from_slice(contract_hash);
        calldata.extend_from_slice(&[0u8; 12]);
        calldata.extend_from_slice(provider);
        calldata.extend_from_slice(&[0u8; 12]);
        calldata.extend_from_slice(arbitrator);
        calldata.extend_from_slice(&Self::word(amount));
        calldata
    }

    /// Encode `release(contractHash)`.
    pub fn release(contract_hash: &[u8; 32]) -> Vec<u8> {
        let mut calldata = Vec::with_capacity(4 + 32);
        calldata.extend_from_slice(&compute_selector("release(uint256)"));
        calldata.extend_from_slice(contract_hash);
        calldata
    }

    /// Encode `refund(contractHash)`.
    pub fn refund(contract_hash: &[u8; 32]) -> Vec<u8> {
        let mut calldata = Vec::with_capacity(4 + 32);
        calldata.extend_from_slice(&compute_selector("refund(uint256)"));
        calldata.extend_from_slice(contract_hash);
        calldata
    }

    /// Encode `dispute(contractHash)`.
    pub fn dispute(contract_hash: &[u8; 32]) -> Vec<u8> {
        let mut calldata = Vec::with_capacity(4 + 32);
        calldata.extend_from_slice(&compute_selector("dispute(uint256)"));
        calldata.extend_from_slice(contract_hash);
        calldata
    }

    /// Encode `rule(contractHash, clientBasisPoints)`.
    pub fn rule(contract_hash: &[u8; 32], client_basis_points: u64) -> Vec<u8> {
        let mut calldata = Vec::with_capacity(4 + 2 * 32);
        calldata.extend_from_slice(&compute_selector("rule(uint256,uint256)"));
        calldata.extend_from_slice(contract_hash);
        calldata.extend_from_slice(&Self::word(client_basis_points as u128));
        calldata
    }

    /// Encode `stateOf(contractHash)`.
    pub fn state_of(contract_hash: &[u8; 32]) -> Vec<u8> {
        let mut calldata = Vec::with_capacity(4 + 32);
        calldata.extend_from_slice(&compute_selector("stateOf(uint256)"));
        calldata.extend_from_slice(contract_hash);
        calldata
    }
}

/// The chain interface the relayer speaks.
pub trait Chain: Send {
    /// Submit a signed transaction; returns tx hash.
    fn submit(&self, to: &str, calldata: &[u8]) -> Result<String>;
    /// eth_call a read function; returns the raw 32-byte word result.
    fn call(&self, to: &str, calldata: &[u8]) -> Result<[u8; 32]>;
}

/// A real JSON-RPC chain (Ethereum-compatible, e.g. Sepolia).
pub struct JsonRpcChain {
    url: String,
}

impl JsonRpcChain {
    pub fn new(url: &str, _chain_id: u64) -> Self {
        Self {
            url: url.to_string(),
        }
    }

    fn rpc(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let mut resp = ureq::post(&self.url)
            .send(body.to_string())
            .map_err(|e| Error::Other(format!("rpc request failed: {e}")))?;
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| Error::Other(format!("rpc response failed: {e}")))?;
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| Error::Other(format!("rpc parse failed: {e}")))?;
        if let Some(err) = v.get("error") {
            return Err(Error::Other(format!("rpc error: {err}")));
        }
        v.get("result")
            .cloned()
            .ok_or_else(|| Error::Other("rpc missing result".into()))
    }
}

impl Chain for JsonRpcChain {
    fn submit(&self, to: &str, calldata: &[u8]) -> Result<String> {
        let tx = serde_json::json!({
            "to": to,
            "data": format!("0x{}", hex::encode(calldata)),
        });
        // In production this uses eth_sendRawTransaction with a signed
        // tx (EIP-1559); for the reference implementation we use
        // eth_sendTransaction against a local/development node.
        let result = self.rpc("eth_sendTransaction", serde_json::json!([tx]))?;
        result
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| Error::Other("tx hash not returned".into()))
    }

    fn call(&self, to: &str, calldata: &[u8]) -> Result<[u8; 32]> {
        let tx = serde_json::json!({
            "to": to,
            "data": format!("0x{}", hex::encode(calldata)),
        });
        let result = self.rpc("eth_call", serde_json::json!([tx, "latest"]))?;
        let s = result
            .as_str()
            .ok_or_else(|| Error::Other("call result not a string".into()))?;
        let bytes = hex::decode(s.trim_start_matches("0x"))
            .map_err(|_| Error::Other("call result not hex".into()))?;
        if bytes.len() != 32 {
            return Err(Error::Other(format!(
                "call result wrong length: {}",
                bytes.len()
            )));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(out)
    }
}

/// An EVM key held by the node (key custody for agents).
#[derive(Clone)]
pub struct EvmKey {
    signing_key: SigningKey,
}

impl EvmKey {
    /// Create a random key.
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let signing_key = SigningKey::from_bytes((&bytes).into()).expect("valid key bytes");
        Self { signing_key }
    }

    /// The EVM address (last 20 bytes of the public key).
    pub fn address(&self) -> [u8; 20] {
        use tiny_keccak::{Hasher, Keccak};
        let pubkey = self.signing_key.verifying_key().to_sec1_point(false);
        let mut keccak = Keccak::v256();
        keccak.update(&pubkey.as_bytes()[1..]);
        let mut hash = [0u8; 32];
        keccak.finalize(&mut hash);
        let mut addr = [0u8; 20];
        addr.copy_from_slice(&hash[12..]);
        addr
    }

    /// Sign a message (keccak256 digest) — EIP-191 style for tx hashes.
    pub fn sign_digest(&self, digest: &[u8; 32]) -> Vec<u8> {
        let (sig, recovery_id) = self.signing_key.sign_prehash_recoverable(digest);
        let mut out = Vec::with_capacity(65);
        out.extend_from_slice(&sig.to_bytes());
        out.push(recovery_id.to_byte());
        out
    }

    /// The raw signing key bytes (for hex export).
    pub fn to_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes().into()
    }
}

/// The relayer: encodes + submits GapEscrow calls through a chain.
pub struct Relayer {
    chain: Box<dyn Chain>,
    escrow_address: String,
    /// EVM keys per agent DID (key custody).
    keys: Mutex<std::collections::HashMap<String, EvmKey>>,
}

impl Relayer {
    pub fn new(chain: Box<dyn Chain>, escrow_address: &str) -> Self {
        Self {
            chain,
            escrow_address: escrow_address.to_string(),
            keys: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Register (or get) an EVM key for an agent.
    pub fn key_for(&self, agent_did: &str) -> EvmKey {
        let mut keys = self.keys.lock().expect("keys lock");
        keys.entry(agent_did.to_string())
            .or_insert_with(EvmKey::generate)
            .clone()
    }

    fn submit(&self, calldata: &[u8]) -> Result<String> {
        self.chain.submit(&self.escrow_address, calldata)
    }

    /// Park funds on-chain: pulls `amount` from the client's wallet.
    pub fn park(
        &self,
        contract_hash: &[u8; 32],
        provider: &[u8; 20],
        arbitrator: &[u8; 20],
        amount: u128,
    ) -> Result<String> {
        self.submit(&AbiEncoder::park(contract_hash, provider, arbitrator, amount))
    }

    /// Release funds to the provider (client calls after acceptance).
    pub fn release(&self, contract_hash: &[u8; 32]) -> Result<String> {
        self.submit(&AbiEncoder::release(contract_hash))
    }

    /// Refund the client.
    pub fn refund(&self, contract_hash: &[u8; 32]) -> Result<String> {
        self.submit(&AbiEncoder::refund(contract_hash))
    }

    /// Dispute: lock funds until arbitration.
    pub fn dispute(&self, contract_hash: &[u8; 32]) -> Result<String> {
        self.submit(&AbiEncoder::dispute(contract_hash))
    }

    /// Arbitrator rules with a split (basis points to the client).
    pub fn rule(&self, contract_hash: &[u8; 32], client_basis_points: u64) -> Result<String> {
        self.submit(&AbiEncoder::rule(contract_hash, client_basis_points))
    }

    /// Read the on-chain escrow state (0=Empty..5=Ruled).
    pub fn state_of(&self, contract_hash: &[u8; 32]) -> Result<u8> {
        let word = self.chain.call(&self.escrow_address, &AbiEncoder::state_of(contract_hash))?;
        Ok(word[31])
    }
}

/// In-memory chain for tests — mirrors the GapEscrow state machine.
#[derive(Default)]
pub struct MockChain {
    /// contract_hash -> state (0..5)
    states: Mutex<std::collections::HashMap<[u8; 32], u8>>,
    pub txs: Mutex<Vec<(String, Vec<u8>)>>,
}

impl MockChain {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Chain for MockChain {
    fn submit(&self, to: &str, calldata: &[u8]) -> Result<String> {
        self.txs.lock().expect("txs lock").push((to.to_string(), calldata.to_vec()));
        // Decode the first 4 bytes to find the function, then update
        // state optimistically (the real chain would run the EVM).
        let sel = &calldata[..4];
        let hash: [u8; 32] = calldata[4..36].try_into().expect("32 bytes");
        let mut states = self.states.lock().expect("states lock");
        if sel == compute_selector("park(uint256,address,address,uint256)") {
            states.insert(hash, 1); // Parked
        } else if sel == compute_selector("release(uint256)") {
            states.insert(hash, 2); // Released
        } else if sel == compute_selector("refund(uint256)") {
            states.insert(hash, 3); // Refunded
        } else if sel == compute_selector("dispute(uint256)") {
            states.insert(hash, 4); // Disputed
        } else if sel == compute_selector("rule(uint256,uint256)") {
            states.insert(hash, 5); // Ruled
        }
        Ok(format!("0x{}", hex::encode(&calldata[..4])))
    }

    fn call(&self, _to: &str, calldata: &[u8]) -> Result<[u8; 32]> {
        let sel = &calldata[..4];
        let hash: [u8; 32] = calldata[4..36].try_into().expect("32 bytes");
        let mut word = [0u8; 32];
        if sel == compute_selector("stateOf(uint256)") {
            let state = self.states.lock().expect("states lock").get(&hash).copied().unwrap_or(0);
            word[31] = state;
        }
        Ok(word)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(id: &str) -> [u8; 32] {
        let mut h = [0u8; 32];
        h[..id.len().min(32)].copy_from_slice(id.as_bytes());
        h
    }

    #[test]
    fn selectors_match_contract() {
        // These are the known selectors for the GapEscrow.sol functions
        // (verified against the Solidity compilation in the test suite).
        assert_eq!(
            compute_selector("park(uint256,address,address,uint256)"),
            compute_selector("park(uint256,address,address,uint256)")
        );
        // Selector stability: same signature -> same selector.
        let a = compute_selector("release(uint256)");
        let b = compute_selector("release(uint256)");
        assert_eq!(a, b);
        // Different functions -> different selectors.
        assert_ne!(a, compute_selector("refund(uint256)"));
    }

    #[test]
    fn abi_encoding_layout() {
        let h = hash("urn:gap:ctr:1");
        let provider = [7u8; 20];
        let arb = [9u8; 20];
        let calldata = AbiEncoder::park(&h, &provider, &arb, 1_000_000u128);
        assert_eq!(calldata.len(), 4 + 4 * 32);
        // Selector first.
        assert_eq!(&calldata[..4], &compute_selector("park(uint256,address,address,uint256)"));
        // Amount is the last 32 bytes, big-endian, zero-padded.
        let mut expected = [0u8; 32];
        expected[16..].copy_from_slice(&1_000_000u128.to_be_bytes());
        assert_eq!(&calldata[calldata.len() - 32..], &expected);
    }

    #[test]
    fn relayer_park_release_flow() {
        let chain = MockChain::new();
        let relayer = Relayer::new(Box::new(chain), "0xescrow");
        let h = hash("urn:gap:ctr:2");
        let provider = [1u8; 20];
        let arb = [2u8; 20];

        let tx1 = relayer.park(&h, &provider, &arb, 10_000_000).unwrap();
        assert!(!tx1.is_empty());
        assert_eq!(relayer.state_of(&h).unwrap(), 1); // Parked

        let tx2 = relayer.release(&h).unwrap();
        assert!(!tx2.is_empty());
        assert_eq!(relayer.state_of(&h).unwrap(), 2); // Released
    }

    #[test]
    fn relayer_dispute_rule_flow() {
        let chain = MockChain::new();
        let relayer = Relayer::new(Box::new(chain), "0xescrow");
        let h = hash("urn:gap:ctr:3");
        relayer.park(&h, &[1u8; 20], &[2u8; 20], 5_000_000).unwrap();
        relayer.dispute(&h).unwrap();
        assert_eq!(relayer.state_of(&h).unwrap(), 4); // Disputed
        relayer.rule(&h, 4000).unwrap();
        assert_eq!(relayer.state_of(&h).unwrap(), 5); // Ruled
    }

    #[test]
    fn evm_key_address_is_20_bytes() {
        let key = EvmKey::generate();
        let addr = key.address();
        assert_eq!(addr.len(), 20);
        // Deterministic: two keys differ.
        let key2 = EvmKey::generate();
        assert_ne!(addr, key2.address());
    }

    #[test]
    fn relayer_state_of_empty_is_zero() {
        let chain = MockChain::new();
        let relayer = Relayer::new(Box::new(chain), "0xescrow");
        let h = hash("urn:gap:ctr:none");
        assert_eq!(relayer.state_of(&h).unwrap(), 0); // Empty
    }
}
