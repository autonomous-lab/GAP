//! Time boundaries for the anonymous, one-hour MicroVM offer.
//!
//! This module deliberately contains no public reservation endpoint. A trial
//! must not become reachable until the SSH gateway, egress proxy and cleanup
//! worker can enforce the same deadlines end to end.

use super::*;
use base64::Engine;
use hmac::Mac;
use serde::{Deserialize, Serialize};

pub(super) const ACTIVE_SECONDS: u64 = 60 * 60;
pub(super) const CLAIM_SECONDS: u64 = 24 * 60 * 60;

fn same_digest(a:&str,b:&str)->bool {
    if a.len()!=64 || b.len()!=64 {return false}
    a.bytes().zip(b.bytes()).fold(0u8,|difference,(x,y)|difference|(x^y))==0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Trial {
    pub project_id: String,
    pub owner_did: String,
    pub created_at: u64,
    pub claimed_at: Option<u64>,
    pub ssh_key_hash: String,
    pub source_hash: String,
    #[serde(default)] pub additional_source_hashes: Vec<String>,
    pub claim_hash: String,
    pub claim_sealed: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Phase {
    Active,
    Claimable,
    Claimed,
    Expired,
}

impl Phase {
    fn holds_ip_reservation(self) -> bool {
        matches!(self, Self::Active | Self::Claimable)
    }
}

impl Trial {
    pub fn active_until(&self) -> u64 {
        self.created_at.saturating_add(ACTIVE_SECONDS)
    }

    pub fn claim_until(&self) -> u64 {
        self.active_until().saturating_add(CLAIM_SECONDS)
    }

    pub fn phase(&self, now: u64) -> Phase {
        if self.claimed_at.is_some() {
            Phase::Claimed
        } else if now < self.active_until() {
            Phase::Active
        } else if now < self.claim_until() {
            Phase::Claimable
        } else {
            Phase::Expired
        }
    }

    pub fn may_claim(&self, now: u64) -> bool {
        matches!(self.phase(now), Phase::Active | Phase::Claimable)
    }

    pub(super) fn occupies_ip(&self, source: &str) -> bool {
        self.source_hash == source || self.additional_source_hashes.iter().any(|old| old == source)
    }
}

pub(super) fn ssh_fingerprint(input: &str) -> Result<String> {
    let parts: Vec<_> = input.split_whitespace().collect();
    if parts.len() != 2 || !matches!(parts[0], "ssh-ed25519" | "ssh-rsa") || parts[1].len() > 2048 {
        return Err(Error::Other("an Ed25519 or RSA SSH public key is required".into()));
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(parts[1])
        .map_err(|_| Error::Other("invalid SSH public key".into()))?;
    let mut cursor=0usize;
    let field=|cursor:&mut usize| -> Option<&[u8]> {
        let end=cursor.checked_add(4)?;
        let count=u32::from_be_bytes(decoded.get(*cursor..end)?.try_into().ok()?) as usize;
        *cursor=end;
        let end=cursor.checked_add(count)?;
        let value=decoded.get(*cursor..end)?;
        *cursor=end;
        Some(value)
    };
    let valid=match parts[0] {
        "ssh-ed25519" => field(&mut cursor)==Some(b"ssh-ed25519".as_slice())
            && field(&mut cursor).is_some_and(|value|value.len()==32),
        "ssh-rsa" => {
            let algorithm=field(&mut cursor);
            let exponent=field(&mut cursor);
            let modulus=field(&mut cursor);
            let exponent=exponent.and_then(|bytes| {
                if bytes.is_empty() || bytes.len()>4 || bytes[0]==0 {return None}
                Some(bytes.iter().fold(0u32,|value,byte|(value<<8)|u32::from(*byte)))
            });
            let modulus=modulus.map(|bytes|if bytes.first()==Some(&0) {&bytes[1..]} else {bytes});
            let bits=modulus.and_then(|bytes|bytes.first().map(|first|
                bytes.len()*8-first.leading_zeros() as usize)).unwrap_or(0);
            algorithm==Some(b"ssh-rsa".as_slice()) && exponent.is_some_and(|value|value>=3 && value%2==1)
                && (2048..=8192).contains(&bits)
        },
        _ => false,
    };
    if !valid || cursor!=decoded.len() {
        return Err(Error::Other("invalid SSH public key".into()));
    }
    Ok(crate::sha256_hex(&decoded))
}

/// Read-only fleet directory lookup. An unknown key can start a trial locally;
/// a known key must always be sent to its original node.
pub(super) fn route(g: &NodeState, ssh_key: &str, relay_key: &str, source_ip: &str) -> Result<Value> {
    let access = g.fleet_access.as_ref()
        .ok_or_else(|| Error::Other("fleet route authority is unavailable".into()))?;
    let ssh_hash = ssh_fingerprint(ssh_key)?;
    let relay_hash = ssh_fingerprint(relay_key)?;
    let (status, response) = access.connect(&json!({"action":"route-free-vm",
        "ssh_key_hash":ssh_hash,"relay_key_hash":relay_hash,"source_ip":source_ip}));
    if status != 200 || !response["found"].is_boolean() {
        return Err(Error::Other("fleet route authority is unavailable".into()));
    }
    if response["found"] == true && (response["node_id"].as_str().is_none()
        || response["ticket"].as_str().is_none_or(|ticket| ticket.len() > 4096
            || !ticket.starts_with("gapr1."))) {
        return Err(Error::Other("invalid fleet route response".into()));
    }
    Ok(response)
}

pub(super) fn source_fingerprint(ip: &str, secret: &[u8]) -> Result<String> {
    let address: std::net::IpAddr = ip.parse().map_err(|_| Error::Other("invalid source IP".into()))?;
    let address=match address {
        std::net::IpAddr::V6(v6)=>v6.to_ipv4_mapped().map(std::net::IpAddr::V4)
            .unwrap_or(std::net::IpAddr::V6(v6)),
        other=>other,
    };
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret)
        .map_err(|_| Error::Other("invalid abuse-control key".into()))?;
    mac.update(b"gap-free-vm-ip-v1\0");
    mac.update(address.to_string().as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn reserve_on_control(g: &NodeState, project: &str, did: &str, key_hash: &str,
    source_ip: &str, reused: bool) -> Result<u64> {
    // The local-only unit fixture exercises persistence and limits. Production
    // builds always require a configured and reachable fleet authority.
    #[cfg(test)]
    if g.fleet_access.is_none() { return Ok(now_unix()); }
    let access = g.fleet_access.as_ref()
        .ok_or_else(|| Error::Other("fleet admission authority is unavailable".into()))?;
    let (status, response) = access.connect(&json!({"action":"reserve-free-vm",
        "project_id":project,"agent_did":did,"ssh_key_hash":key_hash,"source_ip":source_ip}));
    if status != 200 {
        return Err(Error::Other(format!("fleet admission rejected the anonymous VM: {}",
            response["error"]["code"].as_str().unwrap_or("authority_unavailable"))));
    }
    let active_until = response["active_until"].as_u64()
        .ok_or_else(|| Error::Other("invalid fleet admission response".into()))?;
    if response["project_id"].as_str() != Some(project) || response["owner_did"].as_str() != Some(did)
        || response["node_id"].as_str() != Some(access.node.as_str()) || response["reused"].as_bool() != Some(reused)
        || response["claim_until"].as_u64() != active_until.checked_add(CLAIM_SECONDS)
    {
        return Err(Error::Other("fleet admission response mismatch".into()));
    }
    active_until.checked_sub(ACTIVE_SECONDS)
        .ok_or_else(|| Error::Other("invalid fleet admission deadline".into()))
}

/// Runner-only reservation. The public SSH gateway must authenticate to the
/// runner; callers of the ordinary cloud API cannot reach this operation.
pub(super) fn reserve(g: &mut NodeState, ssh_key: &str, source_ip: &str,
    enabled: bool, abuse_key: &str, max_active: usize) -> Result<Value> {
    if !enabled {
        return Err(Error::Other("anonymous MicroVM trials are disabled".into()));
    }
    if g.private_node.as_ref().and_then(|p| p.runner.as_ref()).is_none() {
        return Err(Error::Other("MicroVM runner is unavailable".into()));
    }
    let key_hash = ssh_fingerprint(ssh_key)?;
    if abuse_key.len() < 32 {
        return Err(Error::Other("abuse-control key is too short".into()));
    }
    let source_hash = source_fingerprint(source_ip, abuse_key.as_bytes())?;
    let vault = g.vault.as_ref().ok_or_else(|| Error::Other("credential vault is required".into()))?;
    let now = now_unix();
    let claim_url=|token:&str| format!("{}/free-vm/claim#{token}",
        std::env::var("GAP_PUBLIC_URL").unwrap_or_default().trim_end_matches('/'));
    if let Some(previous) = g.free_vm_trials.values().find(|trial|
        trial.ssh_key_hash == key_hash && trial.phase(now) == Phase::Active).cloned() {
        if g.free_vm_trials.values().any(|other|other.project_id != previous.project_id
            && other.phase(now).holds_ip_reservation() && other.occupies_ip(&source_hash)) {
            return Err(Error::Other("one anonymous VM per IP".into()));
        }
        if !previous.occupies_ip(&source_hash) {
            if previous.additional_source_hashes.len() >= 8 {
                return Err(Error::Other("too many source IPs for one anonymous VM".into()));
            }
            let created=reserve_on_control(g,&previous.project_id,&previous.owner_did,
                &key_hash,source_ip,true)?;
            if created != previous.created_at {
                return Err(Error::Other("fleet admission deadline mismatch".into()));
            }
            let mut updated=previous.clone();
            updated.additional_source_hashes.push(source_hash.clone());
            g.storage.upsert_state(&crate::storage::StateRecord {
                scope:"cloud_free_vm_trials".into(),key:updated.project_id.clone(),
                value:serde_json::to_string(&updated)?,updated_at:now,
            })?;
            g.free_vm_trials.insert(updated.project_id.clone(),updated);
        } else {
            let created=reserve_on_control(g,&previous.project_id,&previous.owner_did,
                &key_hash,source_ip,true)?;
            if created != previous.created_at {
                return Err(Error::Other("fleet admission deadline mismatch".into()));
            }
        }
        let claim_token = vault.open(&previous.claim_sealed)?;
        return Ok(json!({"project_id":previous.project_id,"owner_did":previous.owner_did,
            "claim_token":claim_token,"claim_url":claim_url(&claim_token),"active_until":previous.active_until(),"claim_until":previous.claim_until(),"reused":true}));
    }
    // A stopped trial cannot be restarted, but proof of the original SSH key
    // may recover its one-use claim link until the claim deadline. This does
    // not reserve another IP or grant access to the stopped guest.
    if let Some(previous) = g.free_vm_trials.values().find(|trial|
        trial.ssh_key_hash == key_hash && trial.phase(now) == Phase::Claimable) {
        let claim_token = vault.open(&previous.claim_sealed)?;
        return Ok(json!({"status":"claimable","claim_url":claim_url(&claim_token),
            "claim_until":previous.claim_until()}));
    }
    if let Some(previous) = g.free_vm_trials.values().find(|trial|
        trial.ssh_key_hash == key_hash && trial.phase(now) == Phase::Claimed) {
        return Ok(json!({"status":"claimed","project_id":previous.project_id,
            "owner_did":previous.owner_did}));
    }
    if g.free_vm_trials.values().any(|trial| trial.phase(now).holds_ip_reservation()
        && (trial.ssh_key_hash == key_hash || trial.occupies_ip(&source_hash))) {
        return Err(Error::Other("anonymous trial limit reached".into()));
    }
    let active = g.free_vm_trials.values().filter(|trial| trial.phase(now) == Phase::Active).count();
    if max_active == 0 || active >= max_active {
        return Err(Error::Other("anonymous trial capacity reached".into()));
    }

    use rand::RngCore;
    let identity = AgentIdentity::generate();
    let did = identity.did().to_string();
    let token = g.issue_token();
    let mut project_random = [0u8; 12];
    let mut claim_random = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut project_random);
    rand::rngs::OsRng.fill_bytes(&mut claim_random);
    let project_id = format!("prj_{}", hex::encode(project_random));
    let claim_token = format!("fvc_{}", hex::encode(claim_random));
    let created_at=reserve_on_control(g,&project_id,&did,&key_hash,source_ip,false)?;
    crate::cloud::ProjectStore::open(&g.cloud_root, &project_id)?;
    let project = crate::cloud::ProjectRecord { project_id: project_id.clone(), owner_did: did.clone(),
        status: "active".into(), plan: "free".into(), created_at: created_at, updated_at: now };
    let trial = Trial { project_id:project_id.clone(), owner_did:did.clone(), created_at,
        claimed_at:None, ssh_key_hash:key_hash, source_hash, additional_source_hashes:Vec::new(),
        claim_hash:crate::sha256_hex(claim_token.as_bytes()), claim_sealed:vault.seal(&claim_token) };
    g.storage.upsert_identity(&IdentityRecord { token:token.clone(), did:did.clone(),
        seed_hex:vault.seal(&identity.seed_hex()), created_at:now })?;
    g.storage.upsert_state(&crate::storage::StateRecord {scope:"cloud_projects".into(),key:project_id.clone(),
        value:serde_json::to_string(&project)?,updated_at:now})?;
    g.storage.upsert_state(&crate::storage::StateRecord {scope:"cloud_free_vm_trials".into(),key:project_id.clone(),
        value:serde_json::to_string(&trial)?,updated_at:now})?;
    g.agents.insert(token.clone(), RegisteredAgent { identity, announcement:None });
    g.agents_by_did.insert(did.clone(), token);
    g.cloud_projects.insert(project_id.clone(), project);
    g.free_vm_trials.insert(project_id.clone(), trial);
    Ok(json!({"project_id":project_id,"owner_did":did,"claim_url":claim_url(&claim_token),"claim_token":claim_token,
        "active_until":created_at.saturating_add(ACTIVE_SECONDS),
        "claim_until":created_at.saturating_add(ACTIVE_SECONDS).saturating_add(CLAIM_SECONDS),"reused":false}))
}

/// A claim token lives only in a URL fragment and POST body, never in an HTTP
/// request path or referrer. The e-mail code is consumed before account binding.
pub(super) fn claim_route(state: &Arc<Mutex<NodeState>>, method: &str, path: &str,
    body: &Value, client_ip: Option<&str>) -> Option<(u16, Value)> {
    if !matches!(path, "/v1/free-vm/claim/request" | "/v1/free-vm/claim/verify") {
        return None;
    }
    if method != "POST" { return Some((405,json!({"error":{"code":"method_not_allowed"}}))); }
    let token = body["claim_token"].as_str().unwrap_or("");
    if token.len()!=68 || !token.starts_with("fvc_") || !token[4..].bytes().all(|b|b.is_ascii_hexdigit()) {
        return Some((403,json!({"error":{"code":"invalid_claim_token"}})));
    }
    let digest=crate::sha256_hex(token.as_bytes());
    let (trial,registration,access)={
        let mut g=match state.lock(){Ok(g)=>g,Err(_)=>return Some((503,json!({"error":{"code":"state_unavailable"}})))};
        if std::env::var("GAP_FREE_VM_ENABLED").as_deref()!=Ok("1") {
            return Some((404,json!({"error":{"code":"not_found"}})));
        }
        if let Err(error)=g.check_rate_limit(None,client_ip) {return Some(error_response(&error))}
        let trial=g.free_vm_trials.values().find(|trial|
            same_digest(&trial.claim_hash,&digest) && trial.may_claim(now_unix())).cloned();
        let Some(trial)=trial else {return Some((403,json!({"error":{"code":"claim_unavailable"}})))};
        let Some(registration)=g.registration.clone() else {return Some((503,json!({"error":{"code":"email_verification_unavailable"}})))};
        let Some(access)=g.fleet_access.clone() else {return Some((503,json!({"error":{"code":"fleet_authority_unavailable"}})))};
        (trial,registration,access)
    };
    let context=format!("free-vm-claim:{}",trial.project_id);
    if path=="/v1/free-vm/claim/request" {
        return Some(match registration.request_link(body["email"].as_str().unwrap_or(""),
            client_ip.unwrap_or("unknown"),now_unix(),&context) {
            Ok(result)=>(202,result),Err(error)=>error.response(),
        });
    }
    let email=match registration.verify_link(body["challenge_id"].as_str().unwrap_or(""),
        body["code"].as_str().unwrap_or(""),now_unix(),&context) {
        Ok(email)=>email,Err(error)=>return Some(error.response()),
    };
    let (status,result)=access.connect(&json!({"action":"claim-free-vm",
        "request_id":format!("claim-{}",trial.project_id),"email":email,
        "project_id":trial.project_id,"agent_did":trial.owner_did}));
    if status != 200 {return Some((status,result));}
    let claimed_at=match result["free_vm_claimed_at"].as_u64() {
        Some(t)=>t,None=>return Some((503,json!({"error":{"code":"invalid_claim_confirmation"}}))),
    };
    let mut g=match state.lock(){Ok(g)=>g,Err(_)=>return Some((503,json!({"error":{"code":"state_unavailable"}})))};
    let Some(current)=g.free_vm_trials.get(&trial.project_id).cloned() else {
        return Some((503,json!({"error":{"code":"trial_state_unavailable"}})));
    };
    if current.claim_hash != trial.claim_hash || current.owner_did != trial.owner_did {
        return Some((503,json!({"error":{"code":"trial_state_changed"}})));
    }
    let mut updated=current;
    updated.claimed_at=Some(claimed_at);
    if g.storage.upsert_state(&crate::storage::StateRecord{scope:"cloud_free_vm_trials".into(),
        key:updated.project_id.clone(),value:match serde_json::to_string(&updated){Ok(v)=>v,Err(_)=>return Some((503,json!({"error":{"code":"trial_state_unavailable"}})))},
        updated_at:now_unix()}).is_err() {
        return Some((503,json!({"error":{"code":"trial_state_unavailable"}})));
    }
    g.free_vm_trials.insert(updated.project_id.clone(),updated);
    Some((200,result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_claim_and_deletion_boundaries_are_exact() {
        let trial = Trial {
            project_id: "prj_test".into(),
            owner_did: "did:gap:test".into(),
            created_at: 100,
            claimed_at: None,
            ssh_key_hash: String::new(), source_hash: String::new(), additional_source_hashes:Vec::new(),
            claim_hash: String::new(), claim_sealed: String::new(),
        };
        assert_eq!(trial.phase(3699), Phase::Active);
        assert_eq!(trial.phase(3700), Phase::Claimable);
        assert!(trial.may_claim(90099));
        assert_eq!(trial.phase(90100), Phase::Expired);
        assert!(!trial.may_claim(90100));
    }

    #[test]
    fn claimed_trial_is_not_expired_by_anonymous_deadline() {
        let trial = Trial {
            project_id: "prj_test".into(),
            owner_did: "did:gap:test".into(),
            created_at: 100,
            claimed_at: Some(3700),
            ssh_key_hash: String::new(), source_hash: String::new(), additional_source_hashes:Vec::new(),
            claim_hash: String::new(), claim_sealed: String::new(),
        };
        assert_eq!(trial.phase(90100), Phase::Claimed);
        assert!(!trial.may_claim(90100));
    }

    #[test]
    fn deadline_math_cannot_wrap() {
        let trial = Trial {
            project_id: "prj_test".into(),
            owner_did: "did:gap:test".into(),
            created_at: u64::MAX - 10,
            claimed_at: None,
            ssh_key_hash: String::new(), source_hash: String::new(), additional_source_hashes:Vec::new(),
            claim_hash: String::new(), claim_sealed: String::new(),
        };
        assert_eq!(trial.active_until(), u64::MAX);
        assert_eq!(trial.claim_until(), u64::MAX);
    }

    #[test]
    fn only_well_formed_ed25519_or_rsa_keys_are_trial_identities() {
        let mut encoded = Vec::from(&b"\0\0\0\x0bssh-ed25519\0\0\0\x20"[..]);
        encoded.extend([7u8; 32]);
        let key = format!("ssh-ed25519 {}", base64::engine::general_purpose::STANDARD.encode(encoded));
        assert_eq!(ssh_fingerprint(&key).unwrap().len(), 64);
        assert!(ssh_fingerprint("ssh-rsa AAAA").is_err());
        assert!(ssh_fingerprint("ssh-ed25519 AAAA").is_err());
        assert!(ssh_fingerprint(&(key + " unexpected-comment")).is_err());
        let field=|value:&[u8]| {
            let mut result=(value.len() as u32).to_be_bytes().to_vec();
            result.extend_from_slice(value);
            result
        };
        let mut rsa=field(b"ssh-rsa");
        rsa.extend(field(&[1,0,1]));
        let mut modulus=vec![0,0x80];
        modulus.extend([0u8;255]);
        rsa.extend(field(&modulus));
        let rsa_key=format!("ssh-rsa {}",base64::engine::general_purpose::STANDARD.encode(&rsa));
        assert_eq!(ssh_fingerprint(&rsa_key).unwrap().len(),64);
        let mut short=field(b"ssh-rsa");
        short.extend(field(&[1,0,1]));
        let mut short_modulus=vec![0,0x80];
        short_modulus.extend([0u8;127]);
        short.extend(field(&short_modulus));
        assert!(ssh_fingerprint(&format!("ssh-rsa {}",base64::engine::general_purpose::STANDARD.encode(short))).is_err());
    }

    #[test]
    fn abuse_key_requires_a_real_ip() {
        let secret = b"a sufficiently long test-only abuse key";
        assert_eq!(source_fingerprint("2001:db8::1", secret).unwrap(),
            source_fingerprint("2001:0db8:0:0:0:0:0:1", secret).unwrap());
        assert_eq!(source_fingerprint("203.0.113.8", secret).unwrap(),
            source_fingerprint("::ffff:203.0.113.8", secret).unwrap());
        assert!(source_fingerprint("127.0.0.1:1234", secret).is_err());
    }

    #[test]
    fn reservation_reuses_the_ssh_key_without_email_or_a_second_vm() {
        use crate::storage::sqlite::SqliteStorage;
        let root = std::env::temp_dir().join(format!("gap-free-vm-{}", rand::random::<u64>()));
        let mut state = NodeState::new(Box::new(SqliteStorage::open(":memory:").unwrap()));
        state.cloud_root = root.clone();
        state.vault = Some(crate::vault::Vault::new(&[9u8; 32]));
        state.private_node = Some(crate::private_node::PrivateNode {
            private: false, approvals: root.join("unused-approvals"), compose_approvals:None,
            runner:Some(("http://127.0.0.1:9".into(),"test-runner-secret".into())),
        });
        let mut wire = Vec::from(&b"\0\0\0\x0bssh-ed25519\0\0\0\x20"[..]);
        wire.extend([8u8; 32]);
        let key = format!("ssh-ed25519 {}", base64::engine::general_purpose::STANDARD.encode(wire));
        let secret = "test-only-key-of-at-least-thirty-two-bytes";
        assert!(reserve(&mut state,&key,"203.0.113.8",false,secret,16).is_err());
        assert!(reserve(&mut state,&key,"203.0.113.8",true,secret,0).is_err());
        let first = reserve(&mut state,&key,"203.0.113.8",true,secret,16).unwrap();
        let second = reserve(&mut state,&key,"203.0.113.9",true,secret,16).unwrap();
        assert_eq!(first["project_id"],second["project_id"]);
        assert_eq!(first["claim_token"],second["claim_token"]);
        assert_eq!(second["reused"],true);
        assert_eq!(state.free_vm_trials.len(),1);
        let did=first["owner_did"].as_str().unwrap();
        let (quota,always_on,restricted,tier)=state.microvm_approval(did).unwrap();
        assert_eq!(quota.vcpus,1.0);
        assert_eq!(quota.memory_mib,1024);
        assert!(!always_on && restricted && tier=="anonymous");
        let mut other_wire = Vec::from(&b"\0\0\0\x0bssh-ed25519\0\0\0\x20"[..]);
        other_wire.extend([9u8;32]);
        let other_key=format!("ssh-ed25519 {}",base64::engine::general_purpose::STANDARD.encode(other_wire));
        assert!(reserve(&mut state,&other_key,"203.0.113.8",true,secret,16).is_err());
        assert!(reserve(&mut state,&other_key,"203.0.113.9",true,secret,16).is_err(),
            "a moved SSH session also reserves its new source IP");
        reserve(&mut state,&other_key,"203.0.113.10",true,secret,16).unwrap();
        assert!(reserve(&mut state,&key,"203.0.113.10",true,secret,16).is_err(),
            "an existing key cannot move onto an IP that owns another trial");
        let project=first["project_id"].as_str().unwrap();
        state.free_vm_trials.get_mut(project).unwrap().created_at=now_unix().saturating_sub(ACTIVE_SECONDS);
        assert!(state.microvm_approval(did).is_none(),"the runner must lose execution approval at one hour");
        let claimable=reserve(&mut state,&key,"203.0.113.200",true,secret,16).unwrap();
        assert_eq!(claimable["status"],"claimable");
        assert!(claimable["claim_url"].as_str().unwrap().ends_with(first["claim_token"].as_str().unwrap()));
        assert!(claimable["project_id"].is_null(),"claim recovery must not provision a new VM");
        assert_eq!(state.free_vm_trials.len(),2,"claim recovery must not reserve the new IP");
        assert!(reserve(&mut state,&other_key,"203.0.113.8",true,secret,16).is_err(),
            "another key on the original IP must not recover the claim link");
        state.free_vm_trials.get_mut(project).unwrap().claimed_at=Some(now_unix());
        let claimed=reserve(&mut state,&key,"203.0.113.200",true,secret,0).unwrap();
        assert_eq!(claimed["status"],"claimed");
        assert_eq!(claimed["project_id"],project);
        assert_eq!(claimed["owner_did"],did);
        assert!(claimed["claim_token"].is_null());
        assert_eq!(state.free_vm_trials.len(),2,"claimed reconnect must not create another trial");
        let mut next_wire=Vec::from(&b"\0\0\0\x0bssh-ed25519\0\0\0\x20"[..]);
        next_wire.extend([10u8;32]);
        let next_key=format!("ssh-ed25519 {}",base64::engine::general_purpose::STANDARD.encode(next_wire));
        let next=reserve(&mut state,&next_key,"203.0.113.8",true,secret,16).unwrap();
        assert_ne!(next["project_id"],first["project_id"],
            "claiming a VM releases its source IP for a new anonymous trial");
        assert!(reserve(&mut state,&key,"203.0.113.8",true,secret,16).is_ok(),
            "the claimed key must still reconnect even when another trial uses its old IP");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reservation_survives_control_node_restart() {
        use crate::storage::sqlite::SqliteStorage;
        let root=std::env::temp_dir().join(format!("gap-free-vm-restart-{}",rand::random::<u64>()));
        std::fs::create_dir_all(&root).unwrap();
        let db=root.join("state.sqlite");
        let mut wire=Vec::from(&b"\0\0\0\x0bssh-ed25519\0\0\0\x20"[..]);
        wire.extend([11u8;32]);
        let key=format!("ssh-ed25519 {}",base64::engine::general_purpose::STANDARD.encode(wire));
        let project;
        {
            let mut state=NodeState::new(Box::new(SqliteStorage::open(db.to_str().unwrap()).unwrap()));
            state.cloud_root=root.join("projects");
            state.vault=Some(crate::vault::Vault::new(&[4u8;32]));
            state.private_node=Some(crate::private_node::PrivateNode {
                private:false, approvals:root.join("unused"),compose_approvals:None,
                runner:Some(("http://127.0.0.1:9".into(),"test-runner-secret".into())),
            });
            let first=reserve(&mut state,&key,"203.0.113.10",true,
                "another-test-only-abuse-key-of-adequate-length",1).unwrap();
            project=first["project_id"].as_str().unwrap().to_string();
        }
        let restored=NodeState::new(Box::new(SqliteStorage::open(db.to_str().unwrap()).unwrap()));
        assert!(restored.free_vm_trials.contains_key(&project));
        assert!(restored.cloud_projects.contains_key(&project));
        std::fs::remove_dir_all(root).unwrap();
    }
}
