# RFC-0005: Verifiable Credentials

**Status:** Draft
**Author(s):** Celene Jimari
**Area:** Identity
**Created:** 2026-08-08
**Targets:** 0.2.0
**Supersedes:** none

## 1. Summary

GAP identities today are self-certifying DIDs (`did:gap:<pubkey>`).
That answers "who holds this key" but not "is this a real company,
an insured provider, a licensed professional." This RFC introduces a
lightweight **Verifiable Credential** model — signed assertions by
third-party issuers about a subject, with selective disclosure of
expiry and revocation — compatible in spirit with W3C VC 2.0 but
scoped to GAP's needs.

## 2. Motivation

OAP §5.2 issues credentials for publisher verification, insurance
coverage, professional codes, and data residency. These credentials
are what let enterprises trust an anonymous DID: "did:gap:… is bound
to Weather Pro GmbH, HRB 123456 B, with €5M liability coverage until
2027." Without credentials, GAP's reputation system can only speak
about *behavior on-network*; credentials speak about *off-network
reality* (legal identity, insurance, licenses). Both are needed.

## 3. Specification

### 3.1 Terminology

| Term | Definition |
|------|------------|
| **Issuer** | DID that signs the credential. |
| **Subject** | DID the credential is about. |
| **Credential type** | Declared class of assertion. |
| **Revocation registry** | Append-only list of revoked credential ids. |

### 3.2 Credential structure

```json
{
  "credential_id": "urn:gap:vc:8f3a",
  "type": "gap.publisher_verified",
  "issuer": "did:gap:verifier-1",
  "subject": "did:gap:9f2c…",
  "claims": {
    "legal_name": "Weather Pro GmbH",
    "registry": { "type": "Handelsregister", "id": "HRB 123456 B", "country": "DE" },
    "verified": true
  },
  "valid_from": "2026-08-08T00:00:00Z",
  "valid_until": "2027-08-08T00:00:00Z",
  "revocation_url": "https://registry.example/revocations",
  "issuer_sig": "ed25519:…"
}
```

### 3.3 Normative credential types (initial set)

| Type | Issuer class | Purpose |
|------|--------------|---------|
| `gap.publisher_verified` | accredited verifier | legal identity of publisher |
| `gap.insurance_coverage` | insurer | active liability policy + limits |
| `gap.professional_code` | bar/chamber/guild | regulated profession membership |
| `gap.data_residency` | auditor | verifiable data residency |
| `gap.conformance` | test-suite runner | conformance level achieved (RFC-0011) |
| `gap.principal_kyc` | KYC provider | principal identity verification |

### 3.4 Rules (normative)

1. **Signed by issuer, verifiable by anyone:** verification = check
   issuer signature + validity window + revocation status.
2. **Issuer reputation:** credentials carry weight only if the issuer
   is itself reputable — issuer DIDs accumulate reputation like any
   agent. A credential from an unranked issuer is worth less than a
   credential from a long-standing verifier.
3. **Revocation:** issuers maintain a signed revocation registry;
   subjects present `revocation_url`; verifiers MUST consult it for
   credentials older than the cache period.
4. **Selective disclosure:** a subject MAY present a subset of claims
   by re-signing a *projection* of the credential (claims + issuer
   signature over the original, plus subject signature over the
   projection). Verifiers check the projection hash against the
   original issuer signature.
5. **Portable:** credentials are bound to DIDs, not platforms. They
   travel with the subject across registries and marketplaces.

### 3.5 Integration

- **Discovery (part 02):** announcements MAY carry
  `credentials: ["urn:gap:vc:8f3a", …]`.
- **Reputation (part 01):** verified credentials weight the trust
  score (a publisher_verified credential lifts the floor).
- **Registry (part 02):** registries MAY filter on credential types
  (`require_credential: "gap.insurance_coverage"`).
- **Tokenomics (spec 07):** verified credentials raise the
  `reputation_multiplier` floor, fighting Sybil from day one.

### 3.6 Conformance requirements

- Issue signed credentials with validity windows.
- Verify issuer signature + window + revocation.
- Support projection (selective disclosure) with subject re-signing.
- Maintain a signed revocation registry.

## 4. Security & privacy considerations

- **Issuer compromise:** revoked credentials are dead on next
  verification; verifiers MUST NOT cache beyond the stated period.
- **Credential stuffing:** a subject cannot fabricate claims without
  the issuer key — issuer keys should be hardware-backed at L3+.
- **Privacy:** projections enable "I am insured" without revealing
  policy details.

## 5. Backward compatibility

Additive. DIDs unchanged. Announcements gain optional fields.
Existing identities are unaffected.

## 6. Reference implementation

- `src/credential.rs`:
  - `Credential { id, type, issuer, subject, claims, valid_from,
    valid_until, revocation_url, sig }`
  - `verify()`, `is_valid_at(now)`, `project(claims_subset) ->
    ProjectedCredential`
  - `RevocationRegistry { revoked: HashSet<String> }` with signed
    entries.
  - `issuer_reputation()` hook into Reputation.
- Tests: verify, expired, revoked, projection roundtrip, tampered
  claims, unknown issuer.

## 7. Review notes

- (pending)

## 8. Open questions

- Full W3C VC 2.0 compatibility vs GAP-native? Proposal: GAP-native
  JSON (simple), with a mapping layer to W3C VC for interop — later
  milestone.
- Credential chaining into RFC-0003 chains? Proposal: yes, issuance and
  revocation events are chained receipts.

## 9. References

- GAP spec part 01 (identity), part 02 (discovery).
- OAP §5.2 (verifiable credentials), W3C VC Data Model 2.0.
- RFC-0003 (receipt chains), RFC-0011 (conformance).
