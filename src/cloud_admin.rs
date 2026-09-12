//! Individual administrator passwords, email second factor and revocable sessions.
//! The infrastructure bearer is never issued to a browser.
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use rand::RngCore;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, path::Path, sync::{Arc, Mutex}, time::Duration};
use crate::registration::{Failure, Registration};

type Result<T> = std::result::Result<T, Failure>;
fn unavailable<E>(_: E) -> Failure { Failure(503,"admin_unavailable") }
fn denied() -> Failure { Failure(401,"invalid_admin_credentials") }
fn random() -> String { let mut bytes=[0;32];rand::rngs::OsRng.fill_bytes(&mut bytes);hex::encode(bytes) }
fn digest(value: &str) -> String { hex::encode(Sha256::digest(value.as_bytes())) }
fn valid_token(value: &str) -> bool { value.len()==64 && value.bytes().all(|b|b.is_ascii_hexdigit()) }

pub struct Admin {
    db: Mutex<Connection>,
    mailer: Option<Arc<Registration>>,
    allowed: HashSet<String>,
    pub origin: String,
}
pub struct Session { pub email: String, pub csrf: String, pub expires: i64 }
impl Admin {
    pub fn from_env() -> std::result::Result<Option<Self>,String> {
        let Some(mailer)=Registration::admin_from_env()? else { return Ok(None) };
        let origin=std::env::var("GAP_ADMIN_ORIGIN").map_err(|_|"GAP_ADMIN_ORIGIN required")?.trim_end_matches('/').to_string();
        let authority=origin.strip_prefix("https://").ok_or("administrator console requires HTTPS")?;
        if authority.is_empty() || !authority.bytes().all(|b|b.is_ascii_alphanumeric() || b==b'.' || b==b'-') { return Err("administrator origin must use a DNS hostname without a port".into()) }
        if std::env::var("GAP_PUBLIC_URL").is_ok_and(|v|v.trim_end_matches('/')==origin) {return Err("administrator origin must be separate from application origin".into())}
        let mut allowed=HashSet::new();
        for email in std::env::var("GAP_ADMIN_EMAILS").map_err(|_|"GAP_ADMIN_EMAILS required")?.split(',') {
            let email=email.trim().to_ascii_lowercase();
            if email.parse::<lettre::Address>().is_err() { return Err("invalid administrator email allowlist".into()) }
            allowed.insert(email);
        }
        if allowed.is_empty() { return Err("administrator allowlist cannot be empty".into()) }
        let path=std::env::var("GAP_ADMIN_DB").unwrap_or_else(|_|"/data/cloud-admin.sqlite".into());
        Self::open(Path::new(&path),origin,allowed,Some(Arc::new(mailer))).map(Some).map_err(|_|"cannot initialize administrator database".into())
    }
    fn open(path: &Path, origin: String, allowed: HashSet<String>, mailer: Option<Arc<Registration>>) -> Result<Self> {
        let db=Connection::open(path).map_err(unavailable)?;
        #[cfg(unix)] if path!=Path::new(":memory:") {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o600)).map_err(unavailable)?;
        }
        db.busy_timeout(Duration::from_secs(5)).map_err(unavailable)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
          CREATE TABLE IF NOT EXISTS administrators(email TEXT PRIMARY KEY,password_hash TEXT NOT NULL,enabled INTEGER NOT NULL DEFAULT 1,created INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS admin_pending(id TEXT PRIMARY KEY,email TEXT NOT NULL,new_hash TEXT,old_hash TEXT,expires INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS admin_sessions(token_hash TEXT PRIMARY KEY,email TEXT NOT NULL,csrf TEXT NOT NULL,password_hash TEXT NOT NULL,expires INTEGER NOT NULL,last_seen INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS admin_attempts(key TEXT NOT NULL,bucket INTEGER NOT NULL,count INTEGER NOT NULL,PRIMARY KEY(key,bucket));
          CREATE TABLE IF NOT EXISTS admin_audit(id INTEGER PRIMARY KEY AUTOINCREMENT,at INTEGER NOT NULL,actor TEXT NOT NULL,action TEXT NOT NULL,target TEXT NOT NULL,details TEXT NOT NULL);")
            .map_err(unavailable)?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS access_requests(id TEXT PRIMARY KEY,owner TEXT NOT NULL,project TEXT NOT NULL,request_id TEXT NOT NULL,payload TEXT NOT NULL,status TEXT NOT NULL,reviewer TEXT,created INTEGER NOT NULL,updated INTEGER NOT NULL,UNIQUE(owner,request_id)); CREATE INDEX IF NOT EXISTS access_owner_status ON access_requests(owner,status);").map_err(unavailable)?;
        Ok(Self {db:Mutex::new(db),origin,allowed,mailer})
    }
    fn begin(&self,email:&str,password:&str,ip:&str,now:u64) -> Result<(String,Option<String>,Option<String>)> {
        let now=i64::try_from(now).map_err(unavailable)?;
        if email.len()>254 || password.len()>128 || password.len()<12 { return Err(denied()) }
        let email=email.trim().to_ascii_lowercase();
        let found:Option<(String,bool)>={
            let mut db=self.db.lock().map_err(unavailable)?;
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(unavailable)?;
            let bucket=now/900;
            tx.execute("DELETE FROM admin_attempts WHERE bucket<?",[bucket.saturating_sub(1)]).map_err(unavailable)?;
            for (key,limit) in [("email:".to_string()+&digest(&email),10),("ip:".to_string()+&digest(ip),40),("global".into(),300)] {
                let used:u32=tx.query_row("SELECT coalesce(sum(count),0) FROM admin_attempts WHERE key=? AND bucket>=?",params![key,bucket.saturating_sub(1)],|r|r.get(0)).map_err(unavailable)?;
                if used>=limit { return Err(Failure(429,"admin_login_rate_limited")) }
                tx.execute("INSERT INTO admin_attempts VALUES(?,?,1) ON CONFLICT(key,bucket) DO UPDATE SET count=count+1",params![key,bucket]).map_err(unavailable)?;
            }
            let found=tx.query_row("SELECT password_hash,enabled FROM administrators WHERE email=?",[&email],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(unavailable)?;
            tx.commit().map_err(unavailable)?;
            found
        };
        if !self.allowed.contains(&email) { return Err(denied()) }
        match found {
            Some((hash,true)) => {
                let parsed=PasswordHash::new(&hash).map_err(unavailable)?;
                Argon2::default().verify_password(password.as_bytes(),&parsed).map_err(|_|denied())?;
                Ok((email,None,Some(hash)))
            }
            Some((_,false)) => Err(denied()),
            None => {
                let salt=SaltString::generate(&mut rand::rngs::OsRng);
                let hash=Argon2::default().hash_password(password.as_bytes(),&salt).map_err(unavailable)?.to_string();
                Ok((email,Some(hash),None))
            }
        }
    }
    pub fn request(&self,email:&str,password:&str,ip:&str,now:u64) -> Result<Value> {
        let (email,new_hash,old_hash)=self.begin(email,password,ip,now)?;
        let mailer=self.mailer.as_ref().ok_or_else(||unavailable(()))?;
        let challenge=mailer.request(&email,ip,now)?;
        let id=challenge["challenge_id"].as_str().ok_or_else(||unavailable(()))?;
        self.pending(id,&email,new_hash.as_deref(),old_hash.as_deref(),now)?;
        Ok(challenge)
    }
    fn pending(&self,id:&str,email:&str,new_hash:Option<&str>,old_hash:Option<&str>,now:u64) -> Result<()> {
        let now=i64::try_from(now).map_err(unavailable)?;
        let db=self.db.lock().map_err(unavailable)?;
        db.execute("DELETE FROM admin_pending WHERE expires<=? OR email=?",params![now,email]).map_err(unavailable)?;
        db.execute("INSERT INTO admin_pending VALUES(?,?,?,?,?)",params![id,email,new_hash,old_hash,now+600]).map_err(unavailable)?;
        Ok(())
    }
    pub fn verify(&self,id:&str,code:&str,now:u64) -> Result<(String,Session)> {
        let email=self.mailer.as_ref().ok_or_else(||unavailable(()))?.verify(id,code,now)?;
        self.complete(id,&email,now)
    }
    fn complete(&self,id:&str,email:&str,now:u64) -> Result<(String,Session)> {
        let now=i64::try_from(now).map_err(unavailable)?;
        if !self.allowed.contains(email) { return Err(denied()) }
        let mut db=self.db.lock().map_err(unavailable)?;
        let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(unavailable)?;
        let pending:Option<(Option<String>,Option<String>)>=tx.query_row("SELECT new_hash,old_hash FROM admin_pending WHERE id=? AND email=? AND expires>?",params![id,email,now],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(unavailable)?;
        let Some((new,old))=pending else {return Err(denied())};
        let current:Option<(String,bool)>=tx.query_row("SELECT password_hash,enabled FROM administrators WHERE email=?",[email],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(unavailable)?;
        let hash=match (current,new,old) {
            (None,Some(new),None) => {
                tx.execute("INSERT INTO administrators(email,password_hash,created) VALUES(?,?,?)",params![email,new,now]).map_err(unavailable)?;
                new
            }
            (Some((hash,true)),None,Some(old)) if hash==old => hash,
            _ => return Err(denied()),
        };
        tx.execute("DELETE FROM admin_pending WHERE id=?",[id]).map_err(unavailable)?;
        tx.execute("DELETE FROM admin_sessions WHERE expires<=? OR last_seen<=?",params![now,now.saturating_sub(1800)]).map_err(unavailable)?;
        let token=random();let csrf=random();let expires=now+8*3600;
        tx.execute("INSERT INTO admin_sessions VALUES(?,?,?,?,?,?)",params![digest(&token),email,csrf,hash,expires,now]).map_err(unavailable)?;
        tx.execute("INSERT INTO admin_audit(at,actor,action,target,details) VALUES(?,?,?,?,?)",params![now,email,"admin.login",email,"{}"] ).map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        Ok((token,Session{email:email.into(),csrf,expires}))
    }
    pub fn session(&self,token:&str,now:u64) -> Result<Session> {
        let now=i64::try_from(now).map_err(unavailable)?;
        if !valid_token(token) {return Err(denied())}
        let db=self.db.lock().map_err(unavailable)?;
        let found:Option<(String,String,i64)>=db.query_row("SELECT s.email,s.csrf,s.expires FROM admin_sessions s JOIN administrators a ON a.email=s.email AND a.password_hash=s.password_hash WHERE s.token_hash=? AND s.expires>? AND s.last_seen>? AND a.enabled=1",params![digest(token),now,now.saturating_sub(1800)],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(unavailable)?;
        let Some((email,csrf,expires))=found else {return Err(denied())};
        if !self.allowed.contains(&email) {return Err(denied())}
        db.execute("UPDATE admin_sessions SET last_seen=? WHERE token_hash=?",params![now,digest(token)]).map_err(unavailable)?;
        Ok(Session{email,csrf,expires})
    }
    pub fn logout(&self,token:&str) -> Result<()> {
        self.db.lock().map_err(unavailable)?.execute("DELETE FROM admin_sessions WHERE token_hash=?",[digest(token)]).map_err(unavailable)?;Ok(())
    }
    pub fn audit(&self,actor:&str,action:&str,target:&str,details:&Value,now:u64) -> Result<()> {
        let now=i64::try_from(now).map_err(unavailable)?;
        self.db.lock().map_err(unavailable)?.execute("INSERT INTO admin_audit(at,actor,action,target,details) VALUES(?,?,?,?,?)",params![now,actor,action,target,details.to_string()]).map_err(unavailable)?;Ok(())
    }
    pub fn audit_entries(&self,offset:u64) -> Result<Value> {
        let offset=i64::try_from(offset).map_err(unavailable)?;
        let db=self.db.lock().map_err(unavailable)?;
        let mut stmt=db.prepare("SELECT id,at,actor,action,target,details FROM admin_audit ORDER BY id DESC LIMIT 100 OFFSET ?").map_err(unavailable)?;
        let rows=stmt.query_map([offset],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"at":r.get::<_,i64>(1)?,"actor":r.get::<_,String>(2)?,"action":r.get::<_,String>(3)?,"target":r.get::<_,String>(4)?,"details":serde_json::from_str::<Value>(&r.get::<_,String>(5)?).unwrap_or(Value::Null)}))).map_err(unavailable)?.collect::<std::result::Result<Vec<_>,_>>().map_err(unavailable)?;
        Ok(json!({"entries":rows,"offset":offset,"limit":100}))
    }
    pub fn request_access(&self,owner:&str,project:&str,input:&Value,now:u64) -> Result<Value> {
        let now=i64::try_from(now).map_err(unavailable)?;
        let request=input["request_id"].as_str().unwrap_or("");
        if request.len()!=32 || !request.bytes().all(|b|b.is_ascii_hexdigit()) {return Err(Failure(400,"invalid_request_id"))}
        let quota:crate::private_node::MicroVMQuota=serde_json::from_value(input["quota"].clone()).map_err(|_|Failure(400,"invalid_quota"))?;
        if [quota.vcpus,quota.memory_mib,quota.max_vms].iter().any(|n|*n==0 || *n>=2_u32.pow(31)) || quota.disk_gib.is_some_and(|n|n==0 || n>=2_u32.pow(31)) {return Err(Failure(400,"invalid_quota"))}
        let reason=input["reason"].as_str().filter(|s|!s.trim().is_empty() && s.len()<=2000).ok_or(Failure(400,"reason_required"))?;
        let always=input["always_on"].as_bool().ok_or(Failure(400,"always_on_flag_required"))?;
        let payload=json!({"quota":quota,"reason":reason,"always_on":always}).to_string();
        let mut db=self.db.lock().map_err(unavailable)?;
        let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(unavailable)?;
        let old:Option<(String,String,String,String)>=tx.query_row("SELECT id,status,payload,project FROM access_requests WHERE owner=? AND request_id=?",params![owner,request],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(unavailable)?;
        if let Some((id,status,previous,old_project))=old {return if payload==previous && project==old_project {Ok(json!({"id":id,"status":status}))}else{Err(Failure(409,"request_id_conflict"))}}
        let pending:i64=tx.query_row("SELECT count(*) FROM access_requests WHERE owner=? AND (status IN ('pending','applying') OR created>?)",params![owner,now.saturating_sub(3600)],|r|r.get(0)).map_err(unavailable)?;
        if pending>=5 {return Err(Failure(429,"approval_requests_rate_limited"))}
        let active:i64=tx.query_row("SELECT count(*) FROM access_requests WHERE owner=? AND status IN ('pending','applying')",[owner],|r|r.get(0)).map_err(unavailable)?;
        if active>0 {return Err(Failure(409,"approval_request_already_pending"))}
        let id="req_".to_owned()+&random();
        tx.execute("INSERT INTO access_requests VALUES(?,?,?,?,?,'pending',NULL,?,?)",params![id,owner,project,request,payload,now,now]).map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        Ok(json!({"id":id,"status":"pending"}))
    }
    pub fn access_requests(&self,owner:Option<&str>,offset:u64) -> Result<Value> {
        let offset=i64::try_from(offset).map_err(unavailable)?;
        let db=self.db.lock().map_err(unavailable)?;
        let mut stmt=db.prepare("SELECT id,owner,project,payload,status,reviewer,created,updated FROM access_requests WHERE (?1 IS NULL OR owner=?1) ORDER BY created DESC,id DESC LIMIT 100 OFFSET ?2").map_err(unavailable)?;
        let rows=stmt.query_map(params![owner,offset],|r|Ok(json!({"id":r.get::<_,String>(0)?,"owner_did":r.get::<_,String>(1)?,"project_id":r.get::<_,String>(2)?,"request":serde_json::from_str::<Value>(&r.get::<_,String>(3)?).unwrap_or(Value::Null),"status":r.get::<_,String>(4)?,"reviewer":r.get::<_,Option<String>>(5)?,"created_at":r.get::<_,i64>(6)?,"updated_at":r.get::<_,i64>(7)?}))).map_err(unavailable)?.collect::<std::result::Result<Vec<_>,_>>().map_err(unavailable)?;
        Ok(json!({"requests":rows,"offset":offset,"limit":100}))
    }
    pub fn review_access(&self,id:&str,approve:bool,actor:&str,policy:&crate::private_node::PrivateNode,now:u64)->Result<Value> {
        let now=i64::try_from(now).map_err(unavailable)?;
        let (owner,payload,status)={
            let mut db=self.db.lock().map_err(unavailable)?;
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(unavailable)?;
            let found:Option<(String,String,String)>=tx.query_row("SELECT owner,payload,status FROM access_requests WHERE id=?",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(unavailable)?;
            let Some((owner,payload,status))=found else{return Err(Failure(404,"approval_request_not_found"))};
            if !matches!(status.as_str(),"pending"|"applying") {return Ok(json!({"id":id,"status":status}))}
            if status=="applying" && !approve {return Err(Failure(409,"approval_is_being_applied"))}
            let next=if approve {"applying"} else {"rejected"};
            tx.execute("UPDATE access_requests SET status=?,reviewer=?,updated=? WHERE id=?",params![next,actor,now,id]).map_err(unavailable)?;
            tx.execute("INSERT INTO admin_audit(at,actor,action,target,details) VALUES(?,?,?,?,?)",params![now,actor,if approve {"approval.accepted"} else {"approval.rejected"},id,payload]).map_err(unavailable)?;
            tx.commit().map_err(unavailable)?;(owner,payload,next.to_string())
        };
        if status=="rejected" {return Ok(json!({"id":id,"status":status}))}
        let value:Value=serde_json::from_str(&payload).map_err(unavailable)?;
        let quota=serde_json::from_value(value["quota"].clone()).map_err(unavailable)?;
        policy.grant_microvm(&owner,quota,value["always_on"].as_bool().unwrap_or(false)).map_err(|_|Failure(503,"approval_apply_pending_retry"))?;
        self.db.lock().map_err(unavailable)?.execute("UPDATE access_requests SET status='approved',updated=? WHERE id=? AND status='applying'",params![now,id]).map_err(unavailable)?;
        Ok(json!({"id":id,"status":"approved"}))
    }
}

pub struct HttpResponse {pub status:u16,pub body:Value,pub cookie:Option<String>}
fn response(result:Result<Value>) -> HttpResponse {
    let (status,body)=match result {Ok(v)=>(200,v),Err(e)=>e.response()};
    HttpResponse{status,body,cookie:None}
}
pub fn handle(state:&Arc<Mutex<crate::server::NodeState>>,method:&str,path:&str,body:&[u8],cookie:&str,csrf:&str,origin:&str,ip:&str) -> HttpResponse {
    let Some(admin)=state.lock().ok().and_then(|s|s.cloud_admin.clone()) else {return response(Err(Failure(503,"administrator_console_not_configured")))};
    let clean=path.split('?').next().unwrap_or(path);
    let now=crate::message::now_unix();
    if method!="GET" && origin!=admin.origin {return response(Err(Failure(403,"admin_origin_rejected")))}
    if body.len()>16384 {return response(Err(Failure(413,"admin_body_too_large")))}
    let input=if body.is_empty(){json!({})}else{match serde_json::from_slice::<Value>(body){Ok(v)=>v,Err(_)=>return response(Err(Failure(400,"invalid_json")))}};
    if method=="POST" && clean=="/v1/admin/console/login" {
        let result=admin.request(input["email"].as_str().unwrap_or(""),input["password"].as_str().unwrap_or(""),ip,now);
        let mut out=response(result);if out.status==200 {out.status=202}return out;
    }
    if method=="POST" && clean=="/v1/admin/console/verify" {
        return match admin.verify(input["challenge_id"].as_str().unwrap_or(""),input["code"].as_str().unwrap_or(""),now) {
            Ok((token,s))=>HttpResponse{status:200,body:json!({"email":s.email,"csrf":s.csrf,"expires_at":s.expires}),cookie:Some(format!("__Host-gap_admin={token}; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=28800"))},
            Err(e)=>response(Err(e)),
        };
    }
    let tokens:Vec<_>=cookie.split(';').filter_map(|p|p.trim().strip_prefix("__Host-gap_admin=")).collect();
    if tokens.len()!=1 {return response(Err(denied()))}
    let token=tokens[0];let session=match admin.session(token,now){Ok(v)=>v,Err(e)=>return response(Err(e))};
    if method!="GET" && (!valid_token(csrf) || digest(csrf)!=digest(&session.csrf)) {return response(Err(Failure(403,"admin_csrf_rejected")))}
    if clean=="/v1/admin/console/session" && method=="GET" {return response(Ok(json!({"email":session.email,"csrf":session.csrf,"expires_at":session.expires})))}
    if clean=="/v1/admin/console/logout" && method=="POST" {
        let mut out=response(admin.logout(token).map(|_|json!({"ok":true})));
        if out.status==200 {out.cookie=Some("__Host-gap_admin=; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=0".into())}return out;
    }
    let offset=path.split_once('?').and_then(|(_,q)|q.split('&').find_map(|p|p.strip_prefix("offset="))).unwrap_or("0").parse::<u64>();
    let offset=match offset {Ok(n) if n<=1_000_000=>n,_=>return response(Err(Failure(400,"invalid_offset")))};
    if clean=="/v1/admin/console/audit" && method=="GET" {return response(admin.audit_entries(offset))}
    if clean=="/v1/admin/console/approvals" && method=="GET" {return response(admin.access_requests(None,offset))}
    if let Some(id)=clean.strip_prefix("/v1/admin/console/approvals/") {
        if method=="POST" {
            let Some(decision)=input["approve"].as_bool() else {return response(Err(Failure(400,"approval_decision_required")))};
            let policy=state.lock().ok().and_then(|s|s.private_node.clone());
            return response(policy.ok_or(Failure(409,"microvm_hosting_not_configured")).and_then(|p|admin.review_access(id,decision,&session.email,&p,now)));
        }
    }
    let suspension_target=clean.strip_prefix("/v1/admin/console/agents/").and_then(|p|p.strip_suffix("/suspension")).map(|id|("agent",id))
        .or_else(||clean.strip_prefix("/v1/admin/console/projects/").and_then(|p|p.strip_suffix("/suspension")).map(|id|("project",id)));
    if let Some((scope,did))=suspension_target {
        if method=="POST" {
            let (Some(active),Some(generation),Some(reason))=(input["active"].as_bool(),input["expected_generation"].as_u64(),input["reason"].as_str()) else {return response(Err(Failure(400,"suspension_decision_required")))};
            let did=crate::server::percent_decode(did);
            let mut result=state.lock().map_err(unavailable).and_then(|mut s|(if scope=="agent" {s.admin_suspend_agent(&did,generation,active,reason,&session.email)} else {s.admin_suspend_project(&did,generation,active,reason,&session.email)}).map_err(|e|{
                let error=e.to_string();
                if error.contains("version_changed"){Failure(409,"suspension_version_changed_refresh_before_retry")}
                else if error.contains("reason_required") || error.contains("unknown "){Failure(400,"invalid_suspension_decision")}
                else{Failure(503,"suspension_update_unavailable")}
            }));
            if let Ok(ref mut value)=result {
                // Attribution/history is already atomic with the durable policy.
                // This is a secondary index for the common audit-log view.
                if admin.audit(&session.email,&format!("{}.{}",scope,if active {"suspended"}else{"reactivated"}),&did,value,now).is_err(){value["audit_index_pending"]=json!(true);}
            }
            return response(result);
        }
    }
    if clean=="/v1/admin/console/finance" && method=="GET" {
        let param=|name:&str|path.split_once('?').and_then(|(_,q)|q.split('&').find_map(|p|p.split_once('=').filter(|(key,_)|*key==name).map(|(_,v)|v.to_owned())));
        let end=now/3600*3600;
        let start=param("start").and_then(|v|v.parse::<u64>().ok()).unwrap_or(end.saturating_sub(86400));
        let end=param("end").and_then(|v|v.parse::<u64>().ok()).unwrap_or(end);
        if start%3600!=0 || end%3600!=0 || end<=start || end>now/3600*3600 || end-start>366*86400 {return response(Err(Failure(400,"invalid_finance_window")))}
        if param("scope").as_deref()!=Some("node") {
            if let Some((status,body))=crate::fleet_finance::fleet_report(start,end,param("project_id").as_deref(),param("customer_id").as_deref(),param("node_id").as_deref()) {
                return HttpResponse{status,body,cookie:None};
            }
        }
        let runner=state.lock().ok().and_then(|s|s.private_node.as_ref().and_then(|p|p.runner.clone()));
        return match runner {
            Some(r)=>{let (status,body)=crate::private_node::forward(&r,"","","GET","admin/finance",json!({"start":start,"end":end,"project_id":param("project_id")}));HttpResponse{status,body,cookie:None}},
            None=>response(Ok(json!({"available":false}))),
        };
    }
    if clean=="/v1/admin/console/microvms" && method=="GET" {
        let project=path.split_once('?').and_then(|(_,q)|q.split('&').find_map(|p|p.strip_prefix("project_id="))).map(crate::server::percent_decode);
        if project.as_ref().is_some_and(|p|!p.starts_with("prj_") || p.len()!=28 || !p[4..].bytes().all(|b|b.is_ascii_hexdigit())) {return response(Err(Failure(400,"invalid_project")))}
        let runner=state.lock().ok().and_then(|s|s.private_node.as_ref().and_then(|p|p.runner.clone()));
        return match runner {
            Some(r)=>{let (status,body)=crate::private_node::forward(&r,"","","GET","admin/inventory",json!({"offset":offset,"project_id":project}));HttpResponse{status,body,cookie:None}},
            None=>response(Ok(json!({"vms":[],"available":false}))),
        };
    }
    if method=="GET" {
        return response(state.lock().map_err(unavailable).and_then(|s|s.admin_cloud_inventory(clean,offset).map_err(|_|Failure(404,"admin_resource_not_found"))));
    }
    response(Err(Failure(404,"admin_resource_not_found")))
}

#[cfg(test)] mod tests {
    use super::*;
    fn admin()->Admin {Admin::open(Path::new(":memory:"),"https://node.test".into(),HashSet::from(["owner@example.com".into()]),None).unwrap()}
    fn enroll(a:&Admin)->(String,Session) {
        let (email,new,old)=a.begin("owner@example.com","a strong unique password","ip",100).unwrap();
        a.pending("challenge",&email,new.as_deref(),old.as_deref(),100).unwrap();
        a.complete("challenge",&email,101).unwrap()
    }
    #[test] fn password_and_verified_factor_are_both_required() {
        let a=admin();let (token,_)=enroll(&a);
        assert!(a.begin("owner@example.com","a wrong unique password","ip",102).is_err());
        assert!(a.begin("other@example.com","a strong unique password","ip",102).is_err());
        assert!(a.complete("challenge","owner@example.com",102).is_err());
        assert!(a.session(&token,102).is_ok());
        assert!(a.session(&token,1903).is_err());
    }
    #[test] fn pending_enrollment_cannot_overwrite_existing_password() {
        let a=admin();let (_,new,old)=a.begin("owner@example.com","another safe password","other-ip",100).unwrap();
        a.pending("older","owner@example.com",new.as_deref(),old.as_deref(),100).unwrap();
        let _=enroll(&a);
        assert!(a.complete("older","owner@example.com",102).is_err());
        assert!(a.begin("owner@example.com","another safe password","ip",103).is_err());
    }
    #[test] fn logout_revocation_and_session_hash_storage() {
        let a=admin();let (token,_)=enroll(&a);
        let stored:String=a.db.lock().unwrap().query_row("SELECT token_hash FROM admin_sessions",[],|r|r.get(0)).unwrap();
        assert_ne!(stored,token);a.logout(&token).unwrap();assert!(a.session(&token,102).is_err());
    }
    #[test] fn password_attempt_limits_persist_in_database() {
        let a=admin();let _=enroll(&a);
        for _ in 0..9 {let _=a.begin("owner@example.com","incorrect password","ip",102);}
        assert_eq!(a.begin("owner@example.com","a strong unique password","ip",103).unwrap_err().0,429);
    }
    #[test] fn approval_idempotency_rejection_and_live_quota_apply() {
        let a=admin();
        let dir=std::env::temp_dir().join(format!("gap-admin-approval-{}",random()));
        std::fs::create_dir(&dir).unwrap();
        let path=dir.join("approvals.json");
        let owner=format!("did:gap:{}","a".repeat(64));
        let other=format!("did:gap:{}","b".repeat(64));
        std::fs::write(&path,json!({"agents":[other],"quotas":{},"always_on_agents":[]}).to_string()).unwrap();
        let policy=crate::private_node::PrivateNode{private:false,approvals:dir.join("private.json"),compose_approvals:Some(path.clone()),runner:None};
        let input=json!({"request_id":"a".repeat(32),"quota":{"vcpus":2,"memory_mib":4096,"max_vms":2},"reason":"Two application services","always_on":true});
        let request=a.request_access(&owner,"project",&input,100).unwrap();
        assert_eq!(request,a.request_access(&owner,"project",&input,101).unwrap());
        let mut conflicting=input.clone();conflicting["reason"]=json!("Changed request");
        assert_eq!(a.request_access(&owner,"project",&conflicting,102).unwrap_err().0,409);
        let id=request["id"].as_str().unwrap();
        assert_eq!(a.review_access(id,true,"owner@example.com",&policy,103).unwrap()["status"],"approved");
        assert_eq!(policy.microvm_quota(&owner).unwrap().max_vms,2);
        assert!(policy.always_on_allowed(&owner));
        assert!(policy.microvm_quota(&other).is_ok());
        assert_eq!(a.access_requests(Some(&other),0).unwrap()["requests"].as_array().unwrap().len(),0);
        let mut next=input.clone();next["request_id"]=json!("b".repeat(32));
        let rejected=a.request_access(&owner,"project",&next,104).unwrap();
        let rid=rejected["id"].as_str().unwrap();
        assert_eq!(a.review_access(rid,false,"owner@example.com",&policy,105).unwrap()["status"],"rejected");
        assert_eq!(a.review_access(rid,true,"owner@example.com",&policy,106).unwrap()["status"],"rejected");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test] fn disabling_administrator_revokes_existing_session() {
        let a=admin();let (token,_)=enroll(&a);
        a.db.lock().unwrap().execute("UPDATE administrators SET enabled=0",[]).unwrap();
        assert!(a.session(&token,102).is_err());
        assert!(a.begin("owner@example.com","a strong unique password","ip",102).is_err());
    }

}
