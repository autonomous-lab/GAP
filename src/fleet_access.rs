//! Local verification of operator-signed, project-scoped capabilities.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone)]
pub struct Access {
    pub operator: String,
    pub node: String,
    pub key: VerifyingKey,
    pub identity_url: String,
    pub identity_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claims {
    pub version: u8,
    pub operator_id: String,
    pub node_id: String,
    pub customer_id: String,
    pub project_id: String,
    pub owner_did: String,
    pub agent_did: Option<String>,
    pub scope: String,
    pub issued_at: u64,
    pub expires_at: u64,
    pub nonce: String,
}

fn identifier(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
}
fn hex_id(s: &str, prefix: &str, len: usize) -> bool {
    s.strip_prefix(prefix).is_some_and(|v| v.len() == len && v.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

impl Access {
    pub fn from_env() -> Result<Option<Self>, String> {
        match std::env::var("GAP_FLEET_ACCESS_ENABLED").as_deref() {
            Err(_) | Ok("") | Ok("0") => return Ok(None),
            Ok("1") => (),
            _ => return Err("GAP_FLEET_ACCESS_ENABLED must be 0 or 1".into()),
        }
        let get = |k| std::env::var(k).map_err(|_| format!("missing {k}"));
        let operator = get("GAP_FLEET_OPERATOR_ID")?;
        let node = get("GAP_FLEET_NODE_ID")?;
        let raw: [u8; 32] = hex::decode(get("GAP_FLEET_PUBLIC_KEY")?).map_err(|_| "invalid fleet public key")?
            .try_into().map_err(|_| "invalid fleet public key")?;
        let key = VerifyingKey::from_bytes(&raw).map_err(|_| "invalid fleet public key")?;
        let identity_url = get("GAP_FLEET_IDENTITY_URL")?;
        let uri: ureq::http::Uri = identity_url.parse().map_err(|_| "invalid fleet identity URL")?;
        let local = uri.scheme_str() == Some("http") && matches!(uri.host(), Some("127.0.0.1" | "localhost" | "172.17.0.1"));
        if (!local && uri.scheme_str() != Some("https")) || uri.authority().is_none()
            || uri.authority().is_some_and(|a| a.as_str().contains('@')) || uri.query().is_some()
            || !matches!(uri.path(), "/identity" | "/v1/fleet/identity") || identity_url.contains('#') {
            return Err("fleet identity URL requires verified HTTPS or local bridge".into());
        }
        let identity_token = get("GAP_FLEET_IDENTITY_TOKEN")?;
        if !identifier(&operator) || !identifier(&node) || !(43..=128).contains(&identity_token.len())
            || !identity_token.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)) {
            return Err("invalid fleet identity configuration".into());
        }
        Ok(Some(Self { operator, node, key, identity_url, identity_token }))
    }

    pub fn verify(&self, token: &str, project: &str, now: u64) -> Option<Claims> {
        if token.len() > 2048 { return None; }
        let rest = token.strip_prefix("gapf1.")?;
        let (payload, signature) = rest.split_once('.')?;
        let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(signature).ok()?).ok()?;
        self.key.verify_strict(format!("gapf1.{payload}").as_bytes(), &signature).ok()?;
        let claims: Claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
        if claims.version != 1 || claims.operator_id != self.operator || claims.node_id != self.node
            || claims.project_id != project || !hex_id(project, "prj_", 24)
            || !hex_id(&claims.customer_id, "cus_", 32) || !hex_id(&claims.owner_did, "did:gap:", 64)
            || claims.agent_did.as_ref().is_some_and(|a| !hex_id(a, "did:gap:", 64))
            || !hex_id(&claims.nonce, "", 32) || claims.scope != "project.manage"
            || claims.issued_at > now || claims.expires_at <= now
            || claims.expires_at.checked_sub(claims.issued_at).is_none_or(|ttl| !(30..=300).contains(&ttl)) {
            return None;
        }
        Some(claims)
    }

    pub fn connect(&self, body: &Value) -> (u16, Value) {
        crate::fleet_relay::forward(&self.identity_url, "POST", Some(&format!("Bearer {}", self.identity_token)),
            &serde_json::to_vec(body).unwrap_or_default())
    }
}

/// The public relay is an exact allowlist; operator APIs never pass through.
pub fn relay_path(method: &str, path: &str) -> Option<String> {
    match (method, path) {
        ("POST", "/v1/fleet/identity") => Some("/identity".into()),
        ("GET", "/v1/fleet/account" | "/v1/fleet/projects" | "/v1/fleet/wallet" | "/v1/fleet/quotas" | "/v1/fleet/members")
        | ("POST", "/v1/fleet/project-token" | "/v1/fleet/logout" | "/v1/fleet/members") => Some(path.replacen("/v1/fleet/", "/v1/", 1)),
        _ => None,
    }
}

pub fn denied() -> crate::Error { crate::Error::Unauthorized("invalid fleet project credential".into()) }

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use ed25519_dalek::{Signer, SigningKey};
    fn fixture() -> (Access, SigningKey, Value) {
        let key = SigningKey::from_bytes(&[7;32]);
        (Access { operator:"one".into(),node:"node-one".into(),key:key.verifying_key(),identity_url:String::new(),identity_token:String::new() }, key,
         json!({"version":1,"operator_id":"one","node_id":"node-one","customer_id":format!("cus_{}","1".repeat(32)),
            "project_id":format!("prj_{}","2".repeat(24)),"owner_did":format!("did:gap:{}","3".repeat(64)),
            "agent_did":null,"scope":"project.manage","issued_at":100,"expires_at":220,"nonce":"4".repeat(32)}))
    }
    fn sign(key:&SigningKey, value:&Value)->String {
        let message=format!("gapf1.{}",URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap()));
        format!("{message}.{}",URL_SAFE_NO_PAD.encode(key.sign(message.as_bytes()).to_bytes()))
    }
    #[test] fn rejects_wrong_audience_scope_project_times_and_signature() {
        let (a,k,v)=fixture(); let project=v["project_id"].as_str().unwrap();
        let token=sign(&k,&v);assert!(a.verify(&token,project,100).is_some());
        assert!(a.verify(&token,project,220).is_none());assert!(a.verify(&token,project,99).is_none());
        assert!(a.verify(&token,"prj_other",120).is_none());
        assert!(a.verify(&sign(&SigningKey::from_bytes(&[8;32]),&v),project,120).is_none());
        for (field,value) in [("operator_id",json!("two")),("node_id",json!("node-two")),("scope",json!("operator")),
            ("expires_at",json!(401)),("owner_did",json!("root")),("version",json!(2)),("extra",json!(true))] {
            let mut bad=v.clone();bad[field]=value;assert!(a.verify(&sign(&k,&bad),project,120).is_none(),"{field}");
        }
        assert!(a.verify(&format!("{token}.extra"),project,120).is_none());
    }
    #[test] fn relay_never_accepts_administration_or_path_tricks() {
        for path in ["/v1/fleet/operator","/v1/fleet/../operator","/v1/fleet/projects/../operator","/v1/fleet/account/","/v1/fleet/account?url=/operator"] {
            assert!(relay_path("GET",path).is_none());
        }
        assert_eq!(relay_path("GET","/v1/fleet/account").as_deref(),Some("/v1/account"));
        assert!(relay_path("GET","/v1/fleet/identity").is_none());
    }
}
