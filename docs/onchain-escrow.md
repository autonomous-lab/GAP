# GAP — On-Chain Escrow

> *The payment layer in production: funds held by code, not by a node.*

**Author:** Celene Jimari
**Date:** 2026-08-08

## 1. Why on-chain

The off-chain escrow in `src/payment.rs` is the protocol twin: same
state machine, same authorization rules. In production, the escrow is
enforced by a **smart contract** instead of a trusted node. Nobody —
including the node operator — can move funds outside the protocol's
state machine. Trust becomes math.

## 2. The contract

`contracts/GapEscrow.sol` — one instance serves many GAP contracts,
keyed by a contract hash (SHA-256 of the signed GAP contract id):

| Function | Authorized caller | Effect |
|----------|-------------------|--------|
| `park(hash, provider, arbitrator, amount)` | client | pulls stablecoin from client, locks it |
| `release(hash)` | client | sends locked funds to provider (after `exe.accept`) |
| `refund(hash)` | client | sends funds back to client (cancel / ruling) |
| `dispute(hash)` | client | locks funds until arbitrator rules |
| `rule(hash, clientBasisPoints)` | arbitrator | splits funds per ruling |

Currency: a stablecoin (USDC/EURC) held by the contract. The
arbitrator is registered per GAP contract at park time.

## 3. How the node relays

The GAP node acts as a **relayer**: it encodes the GapEscrow calls
(ABI), signs them with agent EVM keys (key custody — `EvmKey` in
`src/relayer.rs`), and submits transactions through a JSON-RPC chain
(`eth_call` for reads, `eth_sendTransaction` for writes). The flow:

```
agent → node HTTP API → relayer → GapEscrow contract
              │
              └─ off-chain: contract signed, deliverable hashed,
                 acceptance recorded (the audit spine)

on-chain only: park → release (or dispute → rule)
```

Enable in the node: `GAP_ESCROW_ADDRESS` + `GAP_RPC_URL` env vars.
Without them, the node uses the off-chain reference escrow.

The off-chain acceptance still carries the proof bundle hash; the
on-chain `release` is the settlement. Both are linked by the contract
hash in the event log (`pay.parked.onchain`, `pay.released.onchain`).

### The relayer module (`src/relayer.rs`)

- `AbiEncoder` — minimal ABI encoding for the GapEscrow functions
  (selectors via keccak256, 32-byte words, address padding).
- `Chain` trait — `JsonRpcChain` (real EVM) and `MockChain` (tests).
- `EvmKey` — secp256k1 keys (k256), EVM address derivation.
- `Relayer` — park/release/refund/dispute/rule/state_of calls.

The server test `onchain_escrow_flow_via_relayer` runs the full HTTP
flow (announce → propose → accept → park → deliver → accept-delivery)
with the mock chain, asserting on-chain settlement events in the
audit spine.

## 4. Verification

```bash
cd contracts
npm install          # solc
node test-escrow.js  # 14 tests: lifecycle, authorization, arbitration
```

The harness compiles both contracts with solc and simulates the EVM
flows (park/release, unauthorized rejection, dispute/rule split,
refund). For production testing, deploy on a testnet with Foundry or
Hardhat — the harness validates semantics; the bytecode is compiled by
solc 0.8.26.

## 5. Deployment checklist

- [ ] Deploy stablecoin (USDC mainnet address or test token)
- [ ] Deploy `GapEscrow` with the token address in the constructor
- [ ] Wire the node's relayer: `GAP_ESCROW_ADDRESS` + EVM key custody
- [ ] Agents approve the escrow contract (one-time per stablecoin)
- [ ] Map contract hash on-chain ↔ off-chain contract id in the audit
      spine (the node records both)
- [ ] Testnet: deploy on Sepolia, run the flow end-to-end

## 6. Gas notes

- `park` + `release` ≈ 2 token transfers per contract — cheap for
  meaningful amounts; micro-payments stay off-chain (ledger + net
  settlement), on-chain is for the settlement layer (see
  `docs/scaling.md` and `BUSINESS.md` §3).

## 7. Security considerations

- **No admin keys**: the contract has no owner; funds are only moved
  by the protocol's state machine.
- **Reentrancy**: transfers are the last statement after state
  transitions (checks-effects-interactions); the reference uses
  ERC-20 (no callbacks).
- **Arbitrator compromise**: the arbitrator can only split *disputed*
  funds between the two recorded parties — never divert them.
- **Contract hash collisions**: SHA-256 of the GAP contract id; ids are
  node-generated unique values (collision negligible).

---
*Celene Jimari — GAP on-chain escrow.*
