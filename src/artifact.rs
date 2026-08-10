//! Deliverable artifacts the node carries on the parties' behalf.
//!
//! The protocol's default is that the work travels out of band and the
//! node keeps only a digest. That is the right default - it keeps the
//! node out of the file-serving business and out of the parties'
//! content - but it assumes the two agents share a channel.
//!
//! When they do not, the default fails twice over: the buyer has no way
//! to fetch what it paid for, and the judge is asked whether the work
//! meets the acceptance criteria with no work to read. The only honest
//! answer to that is `inconclusive`, which releases nothing - so a
//! perfectly good delivery stalls.
//!
//! So an artifact small enough to pass inline may be handed to the node,
//! which holds it for the parties and hands it to the judge. The digest
//! stays authoritative throughout: it is checked against the bytes at
//! delivery, so what the node holds can never disagree with what the
//! provider committed to.

use crate::error::{Error, Result};
use base64::Engine;
use serde_json::Value;

/// The largest artifact the node will hold inline, independent of the
/// HTTP body cap. Beyond this a URI is the answer, not a bigger buffer.
pub const MAX_INLINE_BYTES: usize = 5 * 1024 * 1024;

/// An artifact supplied with a delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// As sent: base64 for binary, the text itself otherwise.
    pub content: String,
    /// `base64` or `utf8`.
    pub encoding: String,
    /// Provider-declared, advisory only (e.g. `image/png`).
    pub media_type: String,
}

impl Artifact {
    /// Read an artifact out of a delivery body, accepting the spellings
    /// agents actually use.
    ///
    /// Being liberal here is deliberate. An agent that sends
    /// `content_base64` and has it silently ignored believes it
    /// delivered something the client can never fetch - which is
    /// precisely the failure this module exists to prevent.
    pub fn parse(body: &Value) -> Option<Self> {
        // Nested form: {"deliverable": {"content_base64": "..."}}
        let scope = body.get("deliverable").filter(|v| v.is_object());
        let look = |key: &str| -> Option<&str> {
            scope
                .and_then(|s| s.get(key))
                .or_else(|| body.get(key))
                .and_then(|v| v.as_str())
        };

        let media_type = look("media_type")
            .or_else(|| look("content_type"))
            .or_else(|| look("mime_type"))
            .unwrap_or("")
            .to_string();

        if let Some(b64) = look("content_base64").or_else(|| look("artifact_base64")) {
            return Some(Self {
                content: b64.trim().to_string(),
                encoding: "base64".into(),
                media_type,
            });
        }
        // A plain string under "deliverable" is the text itself.
        let text = look("content")
            .or_else(|| look("text"))
            .or_else(|| body.get("deliverable").and_then(|v| v.as_str()))?;
        Some(Self {
            content: text.to_string(),
            encoding: "utf8".into(),
            media_type,
        })
    }

    /// The bytes the digest is computed over.
    pub fn bytes(&self) -> Result<Vec<u8>> {
        if self.encoding == "base64" {
            // Accept both alphabets and tolerate missing padding: agents
            // produce all four combinations and none of them is wrong.
            let cleaned: String = self
                .content
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            base64::engine::general_purpose::STANDARD
                .decode(&cleaned)
                .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&cleaned))
                .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(&cleaned))
                .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&cleaned))
                .map_err(|_| Error::Other("deliverable content is not valid base64".into()))
        } else {
            Ok(self.content.clone().into_bytes())
        }
    }

    /// `sha256:<hex>` over the decoded bytes.
    pub fn digest(&self) -> Result<String> {
        Ok(format!("sha256:{}", crate::sha256_hex(&self.bytes()?)))
    }

    pub fn byte_len(&self) -> usize {
        self.bytes().map(|b| b.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.content.trim().is_empty()
    }
}

/// Compare two digests written by different implementations.
///
/// `sha256:ABC…`, `ABC…` and `abc…` are the same commitment. Rejecting a
/// delivery over the prefix or the case would be a protocol failure
/// dressed up as an integrity failure.
pub fn digests_match(a: &str, b: &str) -> bool {
    normalize_digest(a) == normalize_digest(b) && !normalize_digest(a).is_empty()
}

fn normalize_digest(d: &str) -> String {
    d.trim()
        .trim_start_matches("sha256:")
        .trim_start_matches("SHA256:")
        .trim()
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_base64_artifact_hashes_over_its_decoded_bytes() {
        // "hello" base64 is aGVsbG8=; the digest must be of the five
        // bytes, not of the eight characters that encode them.
        let art = Artifact {
            content: "aGVsbG8=".into(),
            encoding: "base64".into(),
            media_type: "text/plain".into(),
        };
        assert_eq!(art.bytes().unwrap(), b"hello");
        assert_eq!(
            art.digest().unwrap(),
            format!("sha256:{}", crate::sha256_hex(b"hello"))
        );
    }

    #[test]
    fn base64_is_accepted_in_every_form_agents_actually_send() {
        for encoded in ["aGVsbG8=", "aGVsbG8", "aGVs bG8=", "aGVsbG8=\n"] {
            let art = Artifact {
                content: encoded.into(),
                encoding: "base64".into(),
                media_type: String::new(),
            };
            assert_eq!(art.bytes().unwrap(), b"hello", "failed on {encoded:?}");
        }
    }

    #[test]
    fn text_artifacts_hash_over_their_utf8_bytes() {
        let art = Artifact {
            content: "hello".into(),
            encoding: "utf8".into(),
            media_type: String::new(),
        };
        assert_eq!(
            art.digest().unwrap(),
            format!("sha256:{}", crate::sha256_hex(b"hello"))
        );
    }

    #[test]
    fn every_spelling_an_agent_might_use_is_recognised() {
        let cases = [
            json!({ "content_base64": "aGVsbG8=" }),
            json!({ "deliverable": { "content_base64": "aGVsbG8=" } }),
            json!({ "artifact_base64": "aGVsbG8=" }),
        ];
        for body in cases {
            let art = Artifact::parse(&body).expect("recognised");
            assert_eq!(art.encoding, "base64");
            assert_eq!(art.bytes().unwrap(), b"hello");
        }
        // ...and the text forms.
        for body in [
            json!({ "content": "hello" }),
            json!({ "deliverable": "hello" }),
            json!({ "deliverable": { "text": "hello" } }),
        ] {
            let art = Artifact::parse(&body).expect("recognised");
            assert_eq!(art.encoding, "utf8");
            assert_eq!(art.content, "hello");
        }
    }

    #[test]
    fn a_delivery_with_no_artifact_parses_to_nothing() {
        let body = json!({ "deliverable_hash": "sha256:abc" });
        assert!(Artifact::parse(&body).is_none());
    }

    #[test]
    fn media_type_is_picked_up_from_any_of_its_usual_names() {
        for key in ["media_type", "content_type", "mime_type"] {
            let body = json!({ "content_base64": "aGVsbG8=", key: "image/png" });
            assert_eq!(Artifact::parse(&body).unwrap().media_type, "image/png");
        }
    }

    #[test]
    fn digests_compare_equal_across_prefix_and_case() {
        let hex = crate::sha256_hex(b"x");
        assert!(digests_match(&format!("sha256:{hex}"), &hex));
        assert!(digests_match(&hex.to_uppercase(), &format!("sha256:{hex}")));
        assert!(!digests_match(&hex, "sha256:deadbeef"));
        // Two empty digests are not a match - that would make an
        // unstated commitment verify against anything.
        assert!(!digests_match("", ""));
        assert!(!digests_match("sha256:", ""));
    }

    #[test]
    fn invalid_base64_is_an_error_not_a_silent_empty_artifact() {
        let art = Artifact {
            content: "!!!not base64!!!".into(),
            encoding: "base64".into(),
            media_type: String::new(),
        };
        assert!(art.bytes().is_err());
        assert!(art.digest().is_err());
    }
}
