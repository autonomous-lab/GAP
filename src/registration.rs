//! Durable, one-use email verification. Local Postfix owns upstream credentials.
use hmac::{Hmac, Mac};
use lettre::{message::Mailbox, Message, SmtpTransport, Transport};
use rand::{Rng, RngCore};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::Sha256;
use std::{path::Path, sync::Mutex, time::Duration};

type Result<T> = std::result::Result<T, Failure>;
#[derive(Debug)]
pub struct Failure(pub u16, pub &'static str);
impl Failure {
    pub fn response(self) -> (u16, Value) {
        (self.0, json!({"error":{"code":self.1,"message":self.1}}))
    }
}
fn unavailable<E>(_: E) -> Failure {
    Failure(503, "registration_unavailable")
}
fn invalid() -> Failure {
    Failure(400, "invalid_verification")
}

pub struct Registration {
    db: Mutex<Connection>,
    key: [u8; 32],
    from: Mailbox,
    smtp: SmtpTransport,
    origin: String,
}
impl Registration {
    pub fn from_env() -> std::result::Result<Option<Self>, String> {
        Self::configured("GAP_EMAIL_VERIFICATION_REQUIRED", "GAP_REGISTRATION_DB", "/data/registration.sqlite", false)
    }
    pub fn admin_from_env() -> std::result::Result<Option<Self>, String> {
        Self::configured("GAP_CLOUD_ADMIN_ENABLED", "GAP_ADMIN_CHALLENGES_DB", "/data/admin-challenges.sqlite", true)
    }
    fn configured(flag: &str, db_var: &str, default_path: &str, admin: bool) -> std::result::Result<Option<Self>, String> {
        match std::env::var(flag).as_deref() {
            Err(_) | Ok("") | Ok("0") => return Ok(None),
            Ok("1") => (),
            _ => return Err(format!("{flag} must be 0 or 1")),
        }
        // Identity and verified-email writes must be acknowledged by storage.
        // ClickHouse's optional fire-and-forget projection mode is incompatible.
        if std::env::var("GAP_STORAGE").as_deref() == Ok("clickhouse")
            && std::env::var("GAP_CLICKHOUSE_ASYNC_INSERT")
                .is_ok_and(|v| !matches!(v.trim(), "0" | "false" | "no" | ""))
        {
            return Err("email registration requires GAP_CLICKHOUSE_ASYNC_INSERT=0".into());
        }
        let get = |name| std::env::var(name).map_err(|_| format!("missing {name}"));
        let mut key: [u8; 32] = hex::decode(get("GAP_MASTER_KEY")?)
            .map_err(|_| "invalid master key")?
            .try_into()
            .map_err(|_| "invalid master key")?;
        let host = get("GAP_SMTP_HOST")?;
        // This transport deliberately supports only an on-host, unencrypted relay.
        // Never silently downgrade a remote authenticated SMTP connection.
        let ip: std::net::Ipv4Addr = host
            .parse()
            .map_err(|_| "SMTP host must be a private IPv4 address")?;
        if !ip.is_private() && !ip.is_loopback() {
            return Err("SMTP host must be private or loopback".into());
        }
        let port: u16 = get("GAP_SMTP_PORT")?
            .parse()
            .map_err(|_| "invalid SMTP port")?;
        if port == 0 {
            return Err("invalid SMTP port".into());
        }
        let from = get("GAP_SMTP_FROM")?;
        if admin {
            let mut mac=Hmac::<Sha256>::new_from_slice(&key).expect("HMAC key");
            mac.update(b"gap-admin-email-key-v1");
            key=mac.finalize().into_bytes().into();
        }
        let mut origin = get(if admin {"GAP_ADMIN_ORIGIN"} else {"GAP_PUBLIC_URL"})?;
        if !origin.starts_with("https://") || origin.contains(['\r', '\n']) {
            return Err("registration requires an HTTPS GAP_PUBLIC_URL".into());
        }
        if admin { origin=format!("{}/admin (administrator authentication)",origin.trim_end_matches('/')); }
        let path = std::env::var(db_var).unwrap_or_else(|_| default_path.into());
        Self::open(Path::new(&path), key, &host, port, &from, &origin)
            .map(Some)
            .map_err(|_| "cannot initialize email verification".into())
    }

    fn open(
        path: &Path,
        key: [u8; 32],
        host: &str,
        port: u16,
        from: &str,
        origin: &str,
    ) -> Result<Self> {
        let db = Connection::open(path).map_err(unavailable)?;
        #[cfg(unix)]
        if path != Path::new(":memory:") {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(unavailable)?;
        }
        db.busy_timeout(Duration::from_secs(5))
            .map_err(unavailable)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS email_challenges (
              id TEXT PRIMARY KEY, email TEXT NOT NULL, email_key TEXT NOT NULL,
              ip_key TEXT NOT NULL, digest BLOB NOT NULL, created INTEGER NOT NULL,
              attempts INTEGER NOT NULL DEFAULT 0, ready INTEGER NOT NULL DEFAULT 0,
              consumed INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX IF NOT EXISTS email_challenge_created ON email_challenges(created);
            CREATE INDEX IF NOT EXISTS email_challenge_email ON email_challenges(email_key,created);
            CREATE INDEX IF NOT EXISTS email_challenge_ip ON email_challenges(ip_key,created);",
        )
        .map_err(unavailable)?;
        let columns:Vec<String>=db.prepare("PRAGMA table_info(email_challenges)").map_err(unavailable)?
            .query_map([],|r|r.get(1)).map_err(unavailable)?.collect::<std::result::Result<_,_>>().map_err(unavailable)?;
        if !columns.iter().any(|c|c=="context") {
            db.execute("ALTER TABLE email_challenges ADD COLUMN context TEXT NOT NULL DEFAULT ''",[]).map_err(unavailable)?;
        }
        Ok(Self {
            db: Mutex::new(db),
            key,
            from: from.parse().map_err(unavailable)?,
            smtp: SmtpTransport::builder_dangerous(host)
                .port(port)
                .timeout(Some(Duration::from_secs(5)))
                .build(),
            origin: origin.into(),
        })
    }
    fn digest(&self, purpose: &str, value: &str) -> Vec<u8> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC key length");
        mac.update(b"gap-email-verification-v1\0");
        mac.update(purpose.as_bytes());
        mac.update(b"\0");
        mac.update(value.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }
    fn email(input: &str) -> Result<String> {
        // Bare ASCII mailbox only. No display names, lists or header syntax.
        if input.len() > 254
            || !input.is_ascii()
            || input
                .chars()
                .any(|c| c.is_whitespace() || "<>,;\"".contains(c))
        {
            return Err(Failure(400, "invalid_email"));
        }
        let address: lettre::Address = input.parse().map_err(|_| Failure(400, "invalid_email"))?;
        Ok(format!(
            "{}@{}",
            address.user(),
            address.domain().to_ascii_lowercase()
        ))
    }
    #[cfg(test)]
    fn prepare(&self, email: &str, ip: &str, now: u64) -> Result<(String, String, String)> {
        self.prepare_for(email,ip,now,"")
    }
    fn prepare_for(&self, email: &str, ip: &str, now: u64, context:&str) -> Result<(String, String, String)> {
        let now = i64::try_from(now).map_err(unavailable)?;
        let email = Self::email(email)?;
        let email_key = hex::encode(self.digest("email-rate", &email.to_ascii_lowercase()));
        let ip_key = hex::encode(self.digest("ip-rate", ip));
        let mut db = self.db.lock().map_err(unavailable)?;
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(unavailable)?;
        tx.execute(
            "DELETE FROM email_challenges WHERE created < ?",
            [now.saturating_sub(86400)],
        )
        .map_err(unavailable)?;
        let (total, per_email, per_ip, recent): (u32,u32,u32,u32) = tx.query_row(
            "SELECT count(*),coalesce(sum(email_key=?1),0),coalesce(sum(ip_key=?2),0),coalesce(sum(email_key=?1 AND created>?3),0)
             FROM email_challenges WHERE created>=?4",
            params![email_key,ip_key,now.saturating_sub(60),now.saturating_sub(3600)],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(unavailable)?;
        if total >= 300 || per_email >= 5 || per_ip >= 30 || recent > 0 {
            return Err(Failure(429, "verification_rate_limited"));
        }
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let id = hex::encode(bytes);
        let code = format!("{:06}", rand::rngs::OsRng.gen_range(0..1_000_000u32));
        let digest = self.digest("code", &Self::code_message(&id,&code,context));
        // The previous challenge stops working when a new one is requested.
        tx.execute(
            "UPDATE email_challenges SET consumed=1 WHERE email_key=?",
            [&email_key],
        )
        .map_err(unavailable)?;
        tx.execute("INSERT INTO email_challenges(id,email,email_key,ip_key,digest,created,context) VALUES(?,?,?,?,?,?,?)",
            params![id,email,email_key,ip_key,digest,now,context]).map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        Ok((id, email, code))
    }
    pub fn request(&self, email: &str, ip: &str, now: u64) -> Result<Value> {
        self.request_for(email,ip,now,"")
    }
    pub fn request_link(&self,email:&str,ip:&str,now:u64,did:&str)->Result<Value> {
        self.request_for(email,ip,now,did)
    }
    fn code_message(id:&str,code:&str,context:&str)->String {
        if context.is_empty(){format!("{id}:{code}")}else{format!("{id}:{context}:{code}")}
    }
    fn request_for(&self,email:&str,ip:&str,now:u64,context:&str)->Result<Value> {
        let (id, email, code) = self.prepare_for(email, ip, now,context)?;
        let purpose=if context.is_empty(){String::new()}else{format!("Attach this email to existing GAP identity: {context}\n")};
        let message = Message::builder().from(self.from.clone()).to(email.parse().map_err(unavailable)?)
            .subject("Your GAP verification code")
            .body(format!("Your GAP verification code is: {code}\n\nRequested for: {}\n{purpose}This code expires in 10 minutes and can be used once.\nIf you did not request it, ignore this email.\nNever share this code with anyone.\n", self.origin))
            .map_err(unavailable)?;
        // No global NodeState lock is held during network I/O.
        self.smtp
            .send(&message)
            .map_err(|_| Failure(503, "verification_delivery_failed"))?;
        self.db
            .lock()
            .map_err(unavailable)?
            .execute("UPDATE email_challenges SET ready=1 WHERE id=?", [&id])
            .map_err(unavailable)?;
        Ok(json!({"verification_required":true,"challenge_id":id,"expires_in":600}))
    }
    pub fn verify(&self, id: &str, code: &str, now: u64) -> Result<String> {
        self.verify_for(id,code,now,"")
    }
    pub fn verify_link(&self,id:&str,code:&str,now:u64,did:&str)->Result<String> {
        self.verify_for(id,code,now,did)
    }
    fn verify_for(&self, id: &str, code: &str, now: u64, context:&str) -> Result<String> {
        if id.len() != 64
            || !id.bytes().all(|b| b.is_ascii_hexdigit())
            || code.len() != 6
            || !code.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(invalid());
        }
        let now = i64::try_from(now).map_err(unavailable)?;
        let mut db = self.db.lock().map_err(unavailable)?;
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(unavailable)?;
        let found: Option<(String,Vec<u8>,i64,u32,bool,bool)> = tx.query_row(
            "SELECT email,digest,created,attempts,ready,consumed FROM email_challenges WHERE id=? AND context=?", params![id,context],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional().map_err(unavailable)?;
        let Some((email, digest, created, attempts, ready, consumed)) = found else {
            return Err(invalid());
        };
        if !ready
            || consumed
            || attempts >= 5
            || now < created
            || now >= created.saturating_add(600)
        {
            return Err(invalid());
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC key length");
        mac.update(b"gap-email-verification-v1\0code\0");
        mac.update(Self::code_message(id,code,context).as_bytes());
        let valid = mac.verify_slice(&digest).is_ok();
        tx.execute(
            "UPDATE email_challenges SET attempts=attempts+1,consumed=? WHERE id=?",
            params![valid, id],
        )
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        if valid {
            Ok(email)
        } else {
            Err(invalid())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn service() -> Registration {
        Registration::open(
            Path::new(":memory:"),
            [17; 32],
            "127.0.0.1",
            25,
            "gap@example.com",
            "https://gap.example.com",
        )
        .unwrap()
    }
    fn ready(s: &Registration, id: &str) {
        s.db.lock()
            .unwrap()
            .execute("UPDATE email_challenges SET ready=1 WHERE id=?", [id])
            .unwrap();
    }
    #[test]
    fn linking_challenges_are_bound_to_identity_and_cannot_create_another_identity() {
        let s=service();
        let (id,_,code)=s.prepare_for("owner@example.com","ip",100,"did:gap:one").unwrap();ready(&s,&id);
        assert!(s.verify(&id,&code,101).is_err());
        assert!(s.verify_link(&id,&code,101,"did:gap:two").is_err());
        assert_eq!(s.verify_link(&id,&code,101,"did:gap:one").unwrap(),"owner@example.com");
        assert!(s.verify_link(&id,&code,101,"did:gap:one").is_err());
        let (id,_,code)=s.prepare("new@example.com","ip",200).unwrap();ready(&s,&id);
        assert!(s.verify_link(&id,&code,201,"did:gap:one").is_err());
        assert!(s.verify(&id,&code,201).is_ok());
    }
    #[test]
    fn one_use_expiry_attempts_delivery_and_reissue() {
        let s = service();
        let (id, _, code) = s.prepare("agent@example.com", "ip", 10000).unwrap();
        assert!(s.verify(&id, &code, 10001).is_err()); // delivery not confirmed
        ready(&s, &id);
        assert_eq!(s.verify(&id, &code, 10002).unwrap(), "agent@example.com");
        assert!(s.verify(&id, &code, 10003).is_err());
        let (id, _, code) = s.prepare("agent@example.com", "ip", 10061).unwrap();
        ready(&s, &id);
        let wrong = if code == "000000" { "111111" } else { "000000" };
        for _ in 0..5 {
            assert!(s.verify(&id, wrong, 10062).is_err());
        }
        assert!(s.verify(&id, &code, 10063).is_err());
        let (old, _, oldcode) = s.prepare("agent@example.com", "ip", 10122).unwrap();
        ready(&s, &old);
        let (id, _, code) = s.prepare("agent@example.com", "ip", 10183).unwrap();
        ready(&s, &id);
        assert!(s.verify(&old, &oldcode, 10184).is_err());
        assert!(s.verify(&id, &code, 10783).is_err());
    }
    #[test]
    fn limits_survive_attempts_and_do_not_accept_header_injection() {
        let s = service();
        for bad in [
            "a@b.com\r\nBcc: victim@example.com",
            "Name <a@b.com>",
            "a@b.com,b@c.com",
            "",
            "a b@c.com",
        ] {
            assert!(s.prepare(bad, "ip", 10000).is_err());
        }
        for i in 0..5 {
            s.prepare("agent@example.com", "ip", 10000 + i * 61)
                .unwrap();
        }
        assert_eq!(
            s.prepare("agent@example.com", "ip", 10305).unwrap_err().0,
            429
        );
        assert_eq!(
            s.prepare("AGENT@example.com", "ip", 10305).unwrap_err().0,
            429
        );
        s.prepare("other@example.com", "ip", 10305).unwrap();
    }
    #[test]
    fn smtp_acceptance_enables_challenge_without_exposing_code() {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream.write_all(b"220 test SMTP\r\n").unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut body = String::new();
            let mut data = false;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                if data {
                    if line == ".\r\n" {
                        stream.write_all(b"250 queued\r\n").unwrap();
                        data = false;
                    } else {
                        body.push_str(&line);
                    }
                } else if line.starts_with("DATA") {
                    stream.write_all(b"354 send data\r\n").unwrap();
                    data = true;
                } else if line.starts_with("QUIT") {
                    stream.write_all(b"221 bye\r\n").unwrap();
                    break;
                } else {
                    stream.write_all(b"250 ok\r\n").unwrap();
                }
            }
            body
        });
        let s = Registration::open(
            Path::new(":memory:"),
            [17; 32],
            "127.0.0.1",
            port,
            "gap@example.com",
            "https://gap.example.com",
        )
        .unwrap();
        let response = s.request("agent@example.com", "ip", 10000).unwrap();
        assert!(response.get("code").is_none());
        let body = peer.join().unwrap();
        let code = body
            .split("Your GAP verification code is: ")
            .nth(1)
            .unwrap()
            .chars()
            .take(6)
            .collect::<String>();
        assert_eq!(
            s.verify(response["challenge_id"].as_str().unwrap(), &code, 10001)
                .unwrap(),
            "agent@example.com"
        );
    }
    #[test]
    fn challenges_and_rate_limits_survive_reopening() {
        let path = std::env::temp_dir().join(format!(
            "gap-registration-test-{}.sqlite",
            rand::random::<u64>()
        ));
        let s = Registration::open(
            &path,
            [17; 32],
            "127.0.0.1",
            25,
            "gap@example.com",
            "https://gap.example.com",
        )
        .unwrap();
        let (id, _, code) = s.prepare("agent@example.com", "ip", 10000).unwrap();
        ready(&s, &id);
        drop(s);
        let s = Registration::open(
            &path,
            [17; 32],
            "127.0.0.1",
            25,
            "gap@example.com",
            "https://gap.example.com",
        )
        .unwrap();
        assert_eq!(
            s.prepare("agent@example.com", "ip", 10001).unwrap_err().0,
            429
        );
        assert!(s.verify(&id, &code, 10002).is_ok());
        drop(s);
        std::fs::remove_file(path).unwrap();
    }
}
