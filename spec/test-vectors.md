# GAP — Known-Answer Test Vectors

**Status:** Normative for v0.1.0
**Verified by:** `tests/test_vectors.rs` in the reference implementation
(the CI fails if the implementation drifts from these bytes).

Any implementation that produces different bytes for these inputs is
not interoperable with GAP v0.1.0. Changing a vector is a breaking
protocol change: bump the protocol version and regenerate this file.

## 1. Key material

Seeds are 32 bytes, shown as a repeated byte value.

| Party | Ed25519 seed | DID |
|-------|--------------|-----|
| Alice | `0x01` × 32 | `did:gap:8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c` |
| Bob   | `0x02` × 32 | `did:gap:8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394` |

The DID body is the lowercase hex of the Ed25519 public key derived
from the seed (spec 01 §1.1).

## 2. Raw signature

Alice signs the ASCII bytes `gap-test-vector`:

```
f5101f5b5c02f944f115fcd805115517db1f5dd04ffdf6a4a2b6934ab872e2e8df99a07c20b8ee01cae0db1b3257cff4e9f411250f88e0eec681b4902d6de004
```

## 3. Envelope canonicalization & signature

Input envelope (before signing):

| Field | Value |
|-------|-------|
| `protocol` | `gap` |
| `version` | `0.1.0` |
| `message_id` | `urn:gap:msg:00000000000000000000000000000001` |
| `from` | Alice's DID |
| `to` | Bob's DID |
| `contract_id` | `urn:gap:ctr:00000000000000000000000000000002` |
| `kind` | `ctr.propose` |
| `timestamp` | `1754000000` |
| `payload` | `{ "note": "test-vector", "n": 1 }` |

Canonical form rules (spec 00 §0.7): UTF-8, **object keys sorted
lexicographically by Unicode code point at every nesting level**, no
insignificant whitespace, `signature` field absent. The payload above
therefore canonicalizes with `"n"` before `"note"`.

Alice's Ed25519 signature over the canonical bytes:

```
a5e73faad727b2600a2ffeaa0ebaa0a1a67cc29825fcca241072f116bd725bd7ffb8db7cfc27d19d63bbf24a8e42584296218fab2aa99f11d1531cda0184ff0d
```

## 4. Signed wire form

The complete signed envelope on the wire (one line):

```json
{"protocol":"gap","version":"0.1.0","message_id":"urn:gap:msg:00000000000000000000000000000001","from":"did:gap:8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c","to":"did:gap:8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394","contract_id":"urn:gap:ctr:00000000000000000000000000000002","kind":"ctr.propose","timestamp":1754000000,"payload":{"n":1,"note":"test-vector"},"signature":"a5e73faad727b2600a2ffeaa0ebaa0a1a67cc29825fcca241072f116bd725bd7ffb8db7cfc27d19d63bbf24a8e42584296218fab2aa99f11d1531cda0184ff0d"}
```

Note the `kind` field carries the **dotted taxonomy** (`ctr.propose`).
A v0.1.0 pre-release of the reference implementation serialized a
collapsed variant name (`ctrpropose`); that form is invalid.

## 5. Regenerating

The vectors are produced by the reference implementation itself; see
`tests/test_vectors.rs`. To regenerate after an intentional breaking
change, update the constants there, run the test to confirm, and mirror
the values into this file.
