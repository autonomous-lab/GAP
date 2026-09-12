//! Public VM access policy is checked before nginx opens any upstream stream.
use super::*;

#[derive(Clone,serde::Serialize,serde::Deserialize)]
pub(super) struct Record {
    pub vm_id:String, pub project_id:String, pub owner_did:String, pub route_key:String,
    pub username:Option<String>, pub password_hash:Option<String>,
    #[serde(default)] pub password_sealed:Option<String>,
}
impl Record {
    fn public(&self)->Value {json!({"vm_id":self.vm_id,"basic_auth_required":true,"configured":self.password_hash.is_some(),"username":self.username,"password_recoverable":self.password_sealed.is_some()})}
}
fn valid_vm(v:&str)->bool {v.strip_prefix("vm_").is_some_and(|v|v.len()==32 && v.bytes().all(|b|b.is_ascii_hexdigit()))}
fn save_record(g:&mut NodeState,r:&Record)->Result<()> {
    let conflicts:Vec<_>=g.vm_http.values().filter(|old|old.route_key==r.route_key && old.vm_id!=r.vm_id).cloned().collect();
    for old in conflicts {
        if old.project_id!=r.project_id || old.owner_did!=r.owner_did {return Err(denied("route ownership conflict"))}
        g.storage.delete_state("cloud_vm_http",&old.vm_id)?;g.vm_http.remove(&old.vm_id);
    }
    g.storage.upsert_state(&crate::storage::StateRecord{scope:"cloud_vm_http".into(),key:r.vm_id.clone(),value:serde_json::to_string(r).map_err(|e|Error::Other(e.to_string()))?,updated_at:now_unix()})?;
    g.vm_http.insert(r.vm_id.clone(),r.clone());Ok(())
}
fn denied(message:&str)->Error {Error::Other(message.into())}

pub(super) fn manage(state:&Arc<Mutex<NodeState>>,method:&str,raw:&str,body:&Value,auth:Option<&str>)->Option<(u16,Value)> {
    let path=raw.split('?').next()?;
    let (project,tail)=path.strip_prefix("/v1/cloud/projects/")?.split_once("/vm/")?;
    if tail!="http-access" && tail!="http-access/reveal" && tail!="domains" && !tail.starts_with("domains/") {return None}
    let result=(|| -> Result<Value> {
        let params=parse_url_params(raw);
        let vm=body["vm_id"].as_str().or_else(||params.get("vm_id").map(String::as_str)).unwrap_or("");
        if !valid_vm(vm) {return Err(denied("vm_id required"))}
        let token=auth.and_then(|a|a.strip_prefix("Bearer ")).ok_or_else(||Error::Unauthorized("missing bearer token".into()))?;
        let mut g=state.lock().map_err(|_|denied("state unavailable"))?;
        let owner=g.cloud_owned_project(token,project)?.owner_did;
        let policy=g.private_node.as_ref().ok_or_else(||denied("microVM hosting not configured"))?;
        policy.authorize_compose(&owner)?;
        let runner=policy.runner.clone().ok_or_else(||denied("microVM hosting not configured"))?;
        if method=="POST" && tail=="http-access/reveal" {
            let r=g.vm_http.get(vm).filter(|r|r.project_id==project && r.owner_did==owner).ok_or_else(||denied("visitor credentials not configured"))?;
            let sealed=r.password_sealed.as_deref().filter(|s|crate::vault::Vault::is_sealed(s)).ok_or_else(||denied("password unavailable: save visitor credentials once to enable reveal"))?;
            let vault=g.vault.as_ref().ok_or_else(||denied("credential vault unavailable"))?;
            let data:Value=serde_json::from_str(&vault.open(sealed)?).map_err(|_|denied("invalid encrypted credentials"))?;
            if data["purpose"]!="vm-http-password-v1" || data["vm_id"]!=vm || data["project_id"]!=project || data["username"].as_str()!=r.username.as_deref() {return Err(denied("credential binding mismatch"))}
            return Ok(json!({"vm_id":vm,"username":r.username,"password":data["password"]}))
        }
        if method=="GET" && tail=="http-access" {
            return Ok(g.vm_http.get(vm).filter(|r|r.project_id==project && r.owner_did==owner).map(Record::public)
                .unwrap_or(json!({"vm_id":vm,"basic_auth_required":true,"configured":false,"username":null})))
        }
        if method=="GET" && tail=="domains" {
            let domains:Vec<_>=g.custom_domains.values().filter(|d|d.project_id==project && d.vm_id.as_deref()==Some(vm)).cloned().collect();
            return Ok(json!({"domains":domains,"target":g.custom_domain_target,"limit":crate::cloud::MAX_SITE_CUSTOM_DOMAINS}))
        }
        if tail=="http-access" && method=="PUT" || tail=="domains" && method=="POST" {
            drop(g);
            let hash=if tail=="http-access" {
                let username=body["username"].as_str().unwrap_or("");
                if username.is_empty()||username.len()>64||!username.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'_'||b==b'-'){return Err(denied("username must contain 1-64 letters, digits, underscores or hyphens"))}
                Some(crate::cloud::hash_site_password(body["password"].as_str().unwrap_or(""))?)
            } else {None};
            let (status,ingress)=crate::private_node::forward(&runner,project,&owner,"GET","ingress",json!({"vm_id":vm}));
            if status!=200 || ingress["vm_id"].as_str()!=Some(vm) {return Err(denied("microVM unavailable or deleted"))}
            let route=ingress["base_path"].as_str().and_then(|p|p.strip_prefix("/apps/")).and_then(|p|p.strip_suffix('/')).ok_or_else(||denied("invalid VM route"))?;
            if route!=vm && route!=project {return Err(denied("VM route identity mismatch"))}
            g=state.lock().map_err(|_|denied("state unavailable"))?;
            g.cloud_owned_project(token,project)?;g.private_node.as_ref().ok_or_else(||denied("hosting disabled"))?.authorize_compose(&owner)?;
            let mut record=g.vm_http.get(vm).cloned().unwrap_or(Record{vm_id:vm.into(),project_id:project.into(),owner_did:owner.clone(),route_key:route.into(),username:None,password_hash:None,password_sealed:None});
            if record.project_id!=project || record.owner_did!=owner {return Err(denied("VM owner mismatch"))}
            record.route_key=route.into();
            if let Some(hash)=hash {record.username=body["username"].as_str().map(str::to_owned);record.password_hash=Some(hash);
                let vault=g.vault.as_ref().ok_or_else(||denied("GAP_MASTER_KEY is required to save recoverable visitor credentials"))?;
                record.password_sealed=Some(vault.seal(&json!({"purpose":"vm-http-password-v1","vm_id":vm,"project_id":project,"username":record.username,"password":body["password"]}).to_string()));
            }
            save_record(&mut g,&record)?;
            if tail=="http-access" {return Ok(record.public())}
            let hostname=body["hostname"].as_str().unwrap_or("");
            for env in ["GAP_PUBLIC_URL","GAP_ADMIN_ORIGIN"] {
                if std::env::var(env).ok().is_some_and(|v|v.trim_end_matches('/').split("://").nth(1).is_some_and(|h|h.eq_ignore_ascii_case(hostname.trim_end_matches('.')))) {return Err(denied("reserved node hostname"))}
            }
            let domain=g.cloud_create_domain(token,project,hostname,"public",Some(vm))?;
            return Ok(json!({"domain":domain,"dns":{"txt_name":domain.verification_name,"txt_value":domain.verification_value,"target":g.custom_domain_target}}))
        }
        let rest=tail.strip_prefix("domains/").ok_or_else(||denied("unsupported VM HTTP operation"))?;
        let (hostname,verify)=rest.strip_suffix("/verify").map(|h|(h,true)).unwrap_or((rest,false));
        let hostname=normalize_custom_hostname(hostname)?;
        let pending=g.custom_domains.get(&hostname).filter(|d|d.project_id==project && d.vm_id.as_deref()==Some(vm)).cloned().ok_or_else(||denied("unknown VM domain"))?;
        if method=="DELETE" && !verify {
            g.storage.delete_state("cloud_domains",&hostname)?;
            g.custom_domains.remove(&hostname);
            return Ok(json!({"deleted":true}))
        }
        if method!="POST" || !verify {return Err(denied("unsupported VM domain operation"))}
        drop(g);verify_domain_txt(&pending)?;
        g=state.lock().map_err(|_|denied("state unavailable"))?;
        g.cloud_owned_project(token,project)?;
        g.private_node.as_ref().ok_or_else(||denied("hosting disabled"))?.authorize_compose(&owner)?;
        let current=g.custom_domains.get(&hostname).filter(|d|d.vm_id.as_deref()==Some(vm) && d.project_id==project && d.verification_value==pending.verification_value).ok_or_else(||denied("domain changed during verification"))?;
        let mut active=current.clone();active.status="active".into();active.updated_at=now_unix();active.verified_at=Some(active.updated_at);
        g.storage.upsert_state(&crate::storage::StateRecord{scope:"cloud_domains".into(),key:hostname.clone(),value:serde_json::to_string(&active).map_err(|e|denied(&e.to_string()))?,updated_at:active.updated_at})?;
        g.custom_domains.insert(hostname,active.clone());Ok(json!({"domain":active}))
    })();
    Some(match result {Ok(v)=>(200,v),Err(e)=>error_response(&e)})
}

pub struct Admission {pub status:u16,pub uri:Option<String>,pub vm_id:Option<String>,pub strip_authorization:bool}
fn answer(status:u16)->Admission {Admission{status,uri:None,vm_id:None,strip_authorization:false}}
fn encode_path(path:&str)->String {
    let mut out=String::new();for b in path.bytes(){if b.is_ascii_alphanumeric()||b"/-._~".contains(&b){out.push(b as char)}else{out.push_str(&format!("%{b:02X}"))}}out
}

pub fn admit_vm_http(state:&Arc<Mutex<NodeState>>,secret:&str,host:&str,path:&str,raw:&str,authorization:Option<&str>,ip:Option<&str>)->Admission {
    let expected=std::env::var("GAP_VM_EDGE_TOKEN").unwrap_or_default();
    let mut g=match state.lock(){Ok(g)=>g,Err(_)=>return answer(503)};
    let domain=g.custom_domain(host).filter(|d|d.vm_id.is_some());
    let private=path.starts_with("/apps/");
    if expected.len()<32 {
        return answer(if domain.is_none()&&!private {200}else if private {401}else{403})
    }
    if crate::sha256_hex(secret.as_bytes())!=crate::sha256_hex(expected.as_bytes()){return answer(403)}
    if domain.is_none()&&!private {return answer(200)}
    if !path.starts_with('/')||path.len()>8192||raw.len()>16384||path.split('/').any(|p|p=="."||p=="..")||raw.bytes().any(|b|b<32||b==127){return answer(403)}
    let record=if let Some(domain)=&domain {
        g.vm_http.get(domain.vm_id.as_deref().unwrap()).filter(|r|r.project_id==domain.project_id).cloned()
    } else {
        let key=path.strip_prefix("/apps/").unwrap().split('/').next().unwrap_or("");
        g.vm_http.values().find(|r|r.route_key==key).cloned()
    };
    let Some(record)=record else{return answer(if private {401}else{403})};
    if !g.active_cloud_project(&record.project_id) {return answer(403)}
    let Some(policy)=g.private_node.as_ref() else{return answer(403)};
    if policy.authorize_compose(&record.owner_did).is_err(){return answer(403)}
    if policy.runner.is_none(){return answer(503)}
    if g.check_rate_limit(None,ip).is_err(){return answer(403)}
    drop(g);
    if domain.is_none() {
        let credentials=authorization.and_then(parse_basic_credentials);
        let allowed=credentials.is_some_and(|(u,p)|record.username.as_deref()==Some(&u) && record.password_hash.as_deref().is_some_and(|h|crate::cloud::verify_site_password(&p,h).unwrap_or(false)));
        if !allowed{return answer(401)}
    }
    // The private Caddy route independently matches both the edge secret and
    // this exact VM generation. Disabled/deleted/replaced routes fail closed.
    // Do not call the worker here: it calls the node back for authorization,
    // which could exhaust the HTTP worker pool under concurrent app requests.
    // Encode nginx's normalized path, not an independently normalized raw URI.
    // Encoded traversal must not escape the selected VM's prefix at Caddy.
    let target=if domain.is_some(){format!("/apps/{}{}",record.route_key,encode_path(path))}else{encode_path(path)};
    let uri=match raw.split_once('?'){Some((_,q))=>format!("{target}?{q}"),None=>target};
    Admission{status:200,uri:Some(uri),vm_id:Some(record.vm_id),strip_authorization:domain.is_none()}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_path_cannot_introduce_a_second_decode_or_header() {
        assert_eq!(encode_path("/hello world/%2e%2e/?#\r\n"),"/hello%20world/%252e%252e/%3F%23%0D%0A");
        assert_eq!(encode_path("/caf\u{e9}"),"/caf%C3%A9");
    }
    #[test]
    fn visitor_passwords_never_appear_in_public_settings() {
        let record=Record{vm_id:"vm_test".into(),project_id:"prj_test".into(),owner_did:"owner".into(),route_key:"prj_test".into(),username:Some("visitor".into()),password_hash:Some("sensitive-hash".into()),password_sealed:Some("sensitive-ciphertext".into())};
        assert_eq!(record.public()["configured"],true);
        assert!(!record.public().to_string().contains("sensitive"));
    }
    #[test]
    fn replaced_legacy_routes_are_removed_and_cross_owner_collisions_rejected() {
        let mut state=NodeState::new(Box::new(crate::storage::sqlite::SqliteStorage::open(":memory:").unwrap()));
        let original=Record{vm_id:"vm_old".into(),project_id:"prj_test".into(),owner_did:"owner".into(),route_key:"prj_test".into(),username:None,password_hash:None,password_sealed:None};
        save_record(&mut state,&original).unwrap();
        let mut replacement=original.clone();replacement.vm_id="vm_new".into();replacement.owner_did="intruder".into();
        assert!(save_record(&mut state,&replacement).is_err());
        assert!(state.vm_http.contains_key("vm_old"));
        replacement.owner_did="owner".into();save_record(&mut state,&replacement).unwrap();
        assert!(!state.vm_http.contains_key("vm_old"));
        assert!(state.storage.get_state("cloud_vm_http","vm_old").unwrap().is_none());
        assert!(state.storage.get_state("cloud_vm_http","vm_new").unwrap().is_some());
    }
}
