//! Durable, encrypted browser sessions on the isolated management origin.
use std::{path::Path, sync::{Mutex, OnceLock}, time::{Duration, SystemTime, UNIX_EPOCH}};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
const COOKIE: &str = "__Secure-GAPVM";
#[derive(Serialize,Deserialize)]
struct Session { id_hash: String, project: String, token: String, csrf_hash: String, expires: u64 }
struct Store { db: Mutex<Connection>, vault: crate::vault::Vault }
static STORE: OnceLock<Result<Store,()>> = OnceLock::new();
fn now() -> u64 {SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()}
fn store() -> Result<&'static Store,()> {
    STORE.get_or_init(|| {
        let master=std::env::var("GAP_MASTER_KEY").map_err(|_|())?;
        let key: [u8;32]=hex::decode(master.trim()).map_err(|_|())?.try_into().map_err(|_|())?;
        let path=std::env::var("GAP_VM_SESSIONS_DB").unwrap_or_else(|_|"/data/cloud-vm-sessions.sqlite".into());
        Store::open(Path::new(&path),&key)
    }).as_ref().map_err(|_|())
}
impl Store {
    fn open(path: &Path,key: &[u8;32]) -> Result<Self,()> {
        let db=Connection::open(path).map_err(|_|())?;
        #[cfg(unix)] if path!=Path::new(":memory:") {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o600)).map_err(|_|())?;
        }
        db.busy_timeout(Duration::from_secs(5)).map_err(|_|())?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
          CREATE TABLE IF NOT EXISTS vm_sessions(id_hash TEXT PRIMARY KEY,payload TEXT NOT NULL,expires INTEGER NOT NULL);
          CREATE INDEX IF NOT EXISTS vm_session_expiry ON vm_sessions(expires);").map_err(|_|())?;
        let mut hash=Sha256::new();hash.update(b"gap-vm-session-encryption-v1\0");hash.update(key);
        let derived:[u8;32]=hash.finalize().into();
        Ok(Self{db:Mutex::new(db),vault:crate::vault::Vault::new(&derived)})
    }
    fn issue(&self,project:&str,token:&str,at:u64) -> Result<(Value,String),()> {
        let id=crate::new_id("session");let csrf=crate::new_id("csrf");
        let id_hash=crate::sha256_hex(id.as_bytes());let expires=at.checked_add(8*3600).ok_or(())?;
        let session=Session{id_hash:id_hash.clone(),project:project.into(),token:token.into(),csrf_hash:crate::sha256_hex(csrf.as_bytes()),expires};
        let payload=self.vault.seal(&serde_json::to_string(&session).map_err(|_|())?);
        let mut db=self.db.lock().map_err(|_|())?;
        let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_|())?;
        tx.execute("DELETE FROM vm_sessions WHERE expires<=?",params![i64::try_from(at).map_err(|_|())?]).map_err(|_|())?;
        let count:i64=tx.query_row("SELECT COUNT(*) FROM vm_sessions",[],|r|r.get(0)).map_err(|_|())?;
        if count>=10000 {return Err(())}
        tx.execute("INSERT INTO vm_sessions(id_hash,payload,expires) VALUES(?,?,?)",params![id_hash,payload,i64::try_from(expires).map_err(|_|())?]).map_err(|_|())?;
        tx.commit().map_err(|_|())?;
        Ok((json!({"project_id":project,"csrf":csrf}),cookie(&id,false)))
    }
    fn authorization(&self,path:&str,cookies:&str,csrf:&str,at:u64) -> Option<String> {
        if csrf.is_empty() {return None}
        let id_hash=crate::sha256_hex(cookie_id(cookies)?.as_bytes());
        let (payload,expires):(String,i64)=self.db.lock().ok()?.query_row("SELECT payload,expires FROM vm_sessions WHERE id_hash=?",params![id_hash],|r|Ok((r.get(0)?,r.get(1)?))).optional().ok()??;
        let expires=u64::try_from(expires).ok()?;
        if expires<=at || !payload.starts_with("enc:v1:") {return None}
        let session:Session=serde_json::from_str(&self.vault.open(&payload).ok()?).ok()?;
        // Bind the authenticated ciphertext to both the lookup key and expiry.
        if session.expires!=expires || session.id_hash!=id_hash || session.csrf_hash!=crate::sha256_hex(csrf.as_bytes()) {return None}
        let prefix=format!("/v1/cloud/projects/{}/",session.project);
        if !path.starts_with(&prefix) || !console_path(path) {return None}
        Some(session.token)
    }
    fn revoke(&self,cookies:&str) -> Result<String,()> {
        if let Some(id)=cookie_id(cookies) {
            self.db.lock().map_err(|_|())?.execute("DELETE FROM vm_sessions WHERE id_hash=?",params![crate::sha256_hex(id.as_bytes())]).map_err(|_|())?;
        }
        Ok(cookie("",true))
    }
}
fn cookie_id(cookies: &str) -> Option<&str> {
    let mut values=cookies.split(';').filter_map(|c|c.trim().strip_prefix("__Secure-GAPVM="));
    let id=values.next()?; if values.next().is_some() {return None} Some(id)
}
fn cookie(value:&str,clear:bool)->String {
    format!("{COOKIE}={value}; Path=/v1/cloud/projects; Secure; HttpOnly; SameSite=Strict{}",if clear {"; Max-Age=0"} else {""})
}
pub fn issue(project:&str,token:&str)->Result<(Value,String),()> {store()?.issue(project,token,now())}
pub fn authorization(path:&str,cookies:&str,csrf:&str)->Option<String> {store().ok()?.authorization(path,cookies,csrf,now())}
pub fn revoke(cookies:&str)->Result<String,()> {store()?.revoke(cookies)}
pub fn console_path(path: &str) -> bool {
    let path=path.split('?').next().unwrap_or(path);
    if path=="/microvms" {return true}
    let Some(rest)=path.strip_prefix("/v1/cloud/projects/") else {return false};
    let Some((project,action))=rest.split_once('/') else {return false};
    project.len()==28 && project.starts_with("prj_") && project[4..].bytes().all(|c|c.is_ascii_hexdigit()) &&
        (action=="vms" || action=="vm" || action.starts_with("vm/") || action=="access-requests" || action=="browser-session")
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn persistent_encrypted_scoped_sessions() {
        let dir=std::env::temp_dir().join(crate::new_id("vm-session-test"));std::fs::create_dir(&dir).unwrap();let path=dir.join("sessions.sqlite");let key=[7u8;32];
        let project="prj_aaaaaaaaaaaaaaaaaaaaaaaa";let url=format!("/v1/cloud/projects/{project}/vms");
        let (body,set)={let s=Store::open(&path,&key).unwrap();s.issue(project,"Bearer secret-test",100).unwrap()};
        let c=set.split(';').next().unwrap();let csrf=body["csrf"].as_str().unwrap();
        assert!(set.contains("HttpOnly"));assert!(!set.contains("Max-Age"));
        let s=Store::open(&path,&key).unwrap();
        assert_eq!(s.authorization(&url,c,csrf,101).as_deref(),Some("Bearer secret-test"));
        assert!(s.authorization(&url,c,"",101).is_none());
        assert!(s.authorization(&url,&format!("{c}; {c}"),csrf,101).is_none());
        assert!(s.authorization("/v1/cloud/projects/prj_bbbbbbbbbbbbbbbbbbbbbbbb/vms",c,csrf,101).is_none());
        assert!(s.authorization(&url.replace("/vms","/kv"),c,csrf,101).is_none());
        assert!(s.authorization(&url,c,csrf,100+8*3600).is_none());
        let payload:String=s.db.lock().unwrap().query_row("SELECT payload FROM vm_sessions",[],|r|r.get(0)).unwrap();
        assert!(!payload.contains("secret-test"));assert!(!payload.contains(csrf));
        assert!(Store::open(&path,&[8u8;32]).unwrap().authorization(&url,c,csrf,101).is_none());
        s.revoke(c).unwrap();drop(s);
        assert!(Store::open(&path,&key).unwrap().authorization(&url,c,csrf,101).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test] fn changed_expiry_or_unencrypted_record_is_rejected() {
        let s=Store::open(Path::new(":memory:"),&[1u8;32]).unwrap();
        let (body,set)=s.issue("prj_aaaaaaaaaaaaaaaaaaaaaaaa","Bearer test",1).unwrap();
        let c=set.split(';').next().unwrap();let csrf=body["csrf"].as_str().unwrap();let url="/v1/cloud/projects/prj_aaaaaaaaaaaaaaaaaaaaaaaa/vms";
        s.db.lock().unwrap().execute("UPDATE vm_sessions SET expires=expires+100",[]).unwrap();
        assert!(s.authorization(url,c,csrf,2).is_none());
        s.db.lock().unwrap().execute("UPDATE vm_sessions SET payload='{}'",[]).unwrap();
        assert!(s.authorization(url,c,csrf,2).is_none());
    }
}
