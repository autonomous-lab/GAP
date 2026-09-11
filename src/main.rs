//! GAP node — the HTTP server binary.
//!
//! Run:
//!   GAP_STORAGE=sqlite    gap-node           (default, SQLite file)
//!   GAP_STORAGE=clickhouse gap-node          (needs GAP_CLICKHOUSE_URL)
//!
//! Environment:
//!   GAP_ADDR             listen address, default 0.0.0.0:8080
//!   GAP_STORAGE          "sqlite" | "clickhouse"
//!   GAP_SQLITE_PATH      SQLite file, default ./gap-node.db
//!   GAP_CLICKHOUSE_URL   ClickHouse HTTP URL, e.g. http://clickhouse:8123
//!   GAP_DB_INIT          "1" to run ClickHouse migrations at startup
//!   GAP_ADMIN_TOKEN      bearer token required for Cloud administration

use gap::error::Result;
use gap::server::{route_with_ip, NodeState};
use gap::storage::clickhouse::{ClickHouseStorage, UreqTransport};
use gap::storage::sqlite::SqliteStorage;
use gap::storage::Storage;
use std::env;
use std::io::Read;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Response, Server};

fn build_storage() -> Result<Box<dyn Storage>> {
    let kind = env::var("GAP_STORAGE").unwrap_or_else(|_| "sqlite".into());
    match kind.as_str() {
        "sqlite" => {
            let path = env::var("GAP_SQLITE_PATH").unwrap_or_else(|_| "./gap-node.db".into());
            println!("[gap-node] storage: sqlite ({path})");
            Ok(Box::new(SqliteStorage::open(&path)?))
        }
        "clickhouse" => {
            let url =
                env::var("GAP_CLICKHOUSE_URL").unwrap_or_else(|_| "http://clickhouse:8123".into());
            println!("[gap-node] storage: clickhouse ({url})");
            let transport = UreqTransport::from_env(&url);
            let storage = ClickHouseStorage::new(transport);
            if env::var("GAP_DB_INIT").as_deref() == Ok("1") {
                storage.migrate()?;
                println!("[gap-node] clickhouse schema migrated");
            }
            // Read the cluster back into the in-memory mirrors that
            // every read goes through. Skipping this made ClickHouse a
            // write-only sink: durable, and never consulted again.
            match storage.hydrate_cloud() {
                Ok(h) => println!(
                    "[gap-node] clickhouse hydrated: {} event(s), {} identity(ies), \
{} announcement(s), {} contract(s), {} escrow(s), {} artifact(s), {} state entr(ies)",
                    h.events,
                    h.identities,
                    h.announcements,
                    h.contracts,
                    h.escrows,
                    h.deliverables,
                    h.state
                ),
                // A node that cannot read its own history must NOT
                // serve as though it had none.
                //
                // This was a warning, and the node started anyway with
                // empty mirrors on top of thirty-eight thousand stored
                // events. Every page read zero, every token was unknown,
                // and - the part that matters - `append_event` derives
                // the next sequence from what it has in memory, so the
                // first write would have restarted the spine at 1 and
                // forked it against the rows already there. It did not
                // happen only because an empty registry rejected every
                // authenticated call first. That is luck, not a design.
                //
                // Refusing to boot turns a degraded node into an outage,
                // which is the lesser failure: an outage is visible and
                // reversible, a forked audit chain is neither.
                Err(e) => {
                    return Err(gap::Error::Other(format!(
                        "clickhouse hydrate failed: {e}. Refusing to start: serving with empty \
                         state over a populated store would answer every query with zero and \
                         restart the audit spine at sequence 1. Fix the store, or point \
                         GAP_STORAGE elsewhere."
                    )));
                }
            }
            Ok(Box::new(storage))
        }
        other => Err(gap::Error::Other(format!(
            "unknown GAP_STORAGE: {other} (use sqlite or clickhouse)"
        ))),
    }
}

fn main() -> Result<()> {
    let private_node = gap::private_node::PrivateNode::from_env()?;
    let addr = env::var("GAP_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let storage = build_storage()?;

    // Node identity persistence (audit fix H-01): load the seed from
    // GAP_NODE_SEED (hex) or GAP_NODE_SEED_FILE. Without it, the node
    // DID changes on every restart.
    let seed: Option<[u8; 32]> = {
        let hex_seed = env::var("GAP_NODE_SEED").ok().or_else(|| {
            env::var("GAP_NODE_SEED_FILE")
                .ok()
                .and_then(|f| std::fs::read_to_string(f).ok())
                .map(|s| s.trim().to_string())
        });
        match hex_seed {
            Some(hex_str) => {
                let bytes = hex::decode(hex_str.trim())
                    .map_err(|_| gap::Error::Other("GAP_NODE_SEED must be 64 hex chars".into()))?;
                let arr: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| gap::Error::Other("GAP_NODE_SEED must be 32 bytes".into()))?;
                Some(arr)
            }
            None => None,
        }
    };
    let token_cap: u32 = env::var("GAP_RATE_TOKEN_CAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let ip_cap: u32 = env::var("GAP_RATE_IP_CAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600);
    let mut state = NodeState::cloud_with_rate_limits(storage, seed, token_cap, ip_cap);
    state.private_node = private_node;
    state.registration = gap::registration::Registration::from_env()
        .map_err(gap::Error::Other)?.map(Arc::new);
    state.cloud_admin = gap::cloud_admin::Admin::from_env()
        .map_err(gap::Error::Other)?.map(Arc::new);
    if let Ok(admin_token) = env::var("GAP_ADMIN_TOKEN") {
        state.set_admin_token(admin_token);
        println!("[gap-cloud] operator token configured");
    } else {
        println!("[gap-cloud] set GAP_ADMIN_TOKEN to enable operator top-ups");
    }

    let state = Arc::new(Mutex::new(state));

    let server = Arc::new(
        Server::http(&addr)
            .map_err(|e| gap::Error::Other(format!("failed to bind {addr}: {e}")))?,
    );
    println!("[gap-node] listening on http://{addr}");
    println!("[gap-node] node DID: {}", state.lock().unwrap().node_did());
    println!("[gap-node] agent card: http://{addr}/.well-known/gap-agent.json");

    // Worker pool: request parsing and response serialization run in
    // parallel; the state lock serializes only the protocol core
    // (event-sourcing requires one order, so writes stay ordered).
    let workers: usize = env::var("GAP_WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(8)
        });
    println!("[gap-node] worker pool: {workers} threads");

    // Maximum HTTP body; per-resource Cloud quotas are enforced separately.
    let max_body: u64 = env::var("GAP_MAX_BODY_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5 * 1024 * 1024);
    println!("[gap-node] max request body: {max_body} bytes");

    // Audit M-02: warn loudly when the node is exposed without TLS.
    let exposed = addr.starts_with("0.0.0.0") || addr.starts_with("::");
    if exposed {
        println!(
            "[gap-node] ⚠ SECURITY: node bound to {addr} without TLS. \
             In production, terminate TLS at the load balancer / reverse \
             proxy — bearer tokens and application data travel in cleartext."
        );
    }

    {
        let state = state.clone();
        std::thread::spawn(move || loop {
            gap::server::run_due_function_schedules(&state, gap::message::now_unix());
            std::thread::sleep(std::time::Duration::from_secs(30));
        });
        println!("[gap-node] function schedules: enabled");
    }

    let mut handles = Vec::new();
    for _ in 0..workers {
        let server = server.clone();
        let state = state.clone();
        handles.push(std::thread::spawn(move || loop {
            let mut request = match server.recv() {
                Ok(r) => r,
                Err(_) => break,
            };
            // Read the body, bounded.
            //
            // This used to `.take(LIMIT)` and carry on, which does not
            // reject an oversized body - it TRUNCATES it. The caller
            // then got a JSON parse error for a payload that was
            // perfectly well formed when it left, and no hint that size
            // was the problem. An agent hit exactly that delivering an
            // image and spent its time shrinking the PNG by trial and
            // error, because the node's answer pointed at the wrong
            // thing entirely.
            //
            // Read one byte past the limit: if it arrives, the body was
            // too big, and we say so with the status that means it.
            let mut body = Vec::new();
            request
                .as_reader()
                .take(max_body + 1)
                .read_to_end(&mut body)
                .ok();
            if body.len() as u64 > max_body {
                let msg = serde_json::json!({
                    "error": "payload too large",
                    "limit_bytes": max_body,
                    "hint": "Reduce the request size to the documented GAP Cloud resource limit; see /agents.md.",
                })
                .to_string();
                let response = Response::from_string(msg)
                    .with_status_code(413)
                    .with_header(
                        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
                    );
                let _ = request.respond(response);
                continue;
            }

            // HEAD is GET without the body (RFC 9110 section 9.3.2).
            //
            // The node answered 400 to every HEAD, on every path,
            // including /health. Uptime monitors send HEAD by default,
            // so did link checkers and preview bots - which meant every
            // automated check reported the site DOWN while a browser
            // loaded it perfectly. Reproduced straight against the node,
            // with Cloudflare out of the picture.
            let head_only = request.method().as_str() == "HEAD";
            let method = if head_only {
                "GET".to_string()
            } else {
                request.method().as_str().to_string()
            };
            let original_path = request.url().to_string();
            let mut auth = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .map(|h| h.value.as_str().to_string());
            let host = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Host"))
                .map(|h| h.value.as_str().trim().to_string())
                .unwrap_or_default();
            let custom_domain_request = request.headers().iter().any(|h| {
                h.field.equiv("X-GAP-Custom-Domain") && h.value.as_str().trim() == "1"
            });
            let custom_gap_request = original_path
                .split('?')
                .next()
                .unwrap_or(&original_path)
                .starts_with("/_gap/");
            let custom_project = (custom_domain_request && custom_gap_request)
                .then(|| {
                    state
                        .lock()
                        .ok()
                        .and_then(|guard| guard.custom_domain_project(&host))
                })
                .flatten();
            let path = custom_project
                .as_deref()
                .and_then(|project_id| gap::server::custom_function_alias(project_id, &original_path))
                .unwrap_or_else(|| original_path.clone());
            // The node is reached through the compose edge, so the TCP peer is
            // otherwise the same container for every visitor. Prefer the
            // address forwarded by Cloudflare/nginx for per-client limits.
            let client_ip = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("CF-Connecting-IP"))
                .or_else(|| {
                    request
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("X-Forwarded-For"))
                })
                .and_then(|h| h.value.as_str().split(',').next())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .or_else(|| request.remote_addr().map(|addr| addr.ip().to_string()));

            // Cloud-only boundary before archived protocol dispatch.
            let clean_path = path.split('?').next().unwrap_or(&path);
            // A dedicated admin origin must never serve tenant applications.
            // Path-only cookie isolation would allow tenant JavaScript to read
            // an administrator's session on the same origin.
            let admin_origin=env::var("GAP_ADMIN_ORIGIN").unwrap_or_default();
            let admin_host=admin_origin.trim_end_matches('/').strip_prefix("https://").unwrap_or("");
            let on_admin_host=!admin_host.is_empty() && host.eq_ignore_ascii_case(admin_host);
            let admin_api=clean_path.starts_with("/v1/admin/console/");
            let vm_console=on_admin_host && gap::cloud_vm_session::console_path(clean_path);
            if clean_path=="/microvms" && !custom_domain_request && !on_admin_host && !admin_host.is_empty() {
                let _=request.respond(Response::from_string("").with_status_code(303).with_header(Header::from_bytes("Location",format!("{}/microvms",admin_origin.trim_end_matches('/'))).unwrap()));continue;
            }
            if (on_admin_host && !vm_console) || clean_path=="/admin" || admin_api {
                let header=|name:&'static str|request.headers().iter().find(|h|h.field.equiv(name)).map(|h|h.value.as_str().to_string()).unwrap_or_default();
                let mut response=if custom_domain_request {
                    Response::from_string("Not found").with_status_code(404)
                } else if !on_admin_host {
                    if clean_path=="/admin" && !admin_host.is_empty() {
                        Response::from_string("").with_status_code(303).with_header(Header::from_bytes("Location",format!("{}/admin",admin_origin.trim_end_matches('/'))).unwrap())
                    } else {Response::from_string("Administrator origin required").with_status_code(403)}
                } else if clean_path=="/admin" && method=="GET" {
                    Response::from_string(if head_only {""} else {include_str!("ui/cloud_admin.html")}).with_header(Header::from_bytes("Content-Type","text/html; charset=utf-8").unwrap())
                } else if admin_api {
                    let out=gap::cloud_admin::handle(&state,&method,&path,&body,&header("Cookie"),&header("X-CSRF-Token"),&header("Origin"),client_ip.as_deref().unwrap_or("unknown"));
                    let mut r=Response::from_string(if head_only {String::new()} else {out.body.to_string()}).with_status_code(out.status).with_header(Header::from_bytes("Content-Type","application/json").unwrap());
                    if let Some(cookie)=out.cookie {r.add_header(Header::from_bytes("Set-Cookie",cookie).unwrap())}r
                } else if clean_path=="/health" && method=="GET" {
                    Response::from_string("{\"status\":\"ok\"}").with_header(Header::from_bytes("Content-Type","application/json").unwrap())
                } else {Response::from_string("Not found").with_status_code(404)};
                for (name,value) in [("Cache-Control","no-store"),("X-Content-Type-Options","nosniff"),("Referrer-Policy","no-referrer"),("X-Frame-Options","DENY"),("Content-Security-Policy","default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"),("X-Robots-Tag","noindex, nofollow")] {response.add_header(Header::from_bytes(name,value).unwrap())}
                let _=request.respond(response);continue;
            }
            if !custom_domain_request
                && !gap::cloud_surface::allowed_api(clean_path)
                && !clean_path.starts_with("/sites/")
                && gap::server::static_asset(clean_path).is_none()
                && gap::cloud_surface::page(clean_path).is_none()
            {
                let response = Response::from_string(
                    r#"{"error":{"code":"archived","message":"This endpoint is archived. See /agents.md for GAP Cloud."}}"#
                ).with_status_code(410).with_header(
                    Header::from_bytes("Content-Type", "application/json").unwrap()
                );
                let _ = request.respond(response);
                continue;
            }

            // Caddy marks requests accepted by its on-demand custom-domain
            // listener. Host routing happens before the GAP UI and API so `/`
            // belongs to the tenant. A removed/unknown mapping fails closed
            // instead of accidentally exposing the GAP homepage.
            if method == "GET" && custom_domain_request && !custom_gap_request {
                if let Some((site, is_public)) = gap::server::serve_custom_domain_site(
                    &state,
                    &host,
                    &path,
                    auth.as_deref(),
                    client_ip.as_deref(),
                ) {
                    let body: &[u8] = if head_only { &[] } else { &site.body };
                    let mut response = Response::from_data(body).with_status_code(site.status);
                    response.add_header(Header::from_bytes(&b"Content-Type"[..], site.media_type.as_bytes()).unwrap());
                    let cache = if is_public { &b"public, max-age=60"[..] } else { &b"private, no-store"[..] };
                    response.add_header(Header::from_bytes(&b"Cache-Control"[..], cache).unwrap());
                    if !is_public {
                        response.add_header(Header::from_bytes(&b"X-Robots-Tag"[..], &b"noindex, nofollow, noarchive"[..]).unwrap());
                    }
                    response.add_header(Header::from_bytes(&b"X-Content-Type-Options"[..], &b"nosniff"[..]).unwrap());
                    response.add_header(Header::from_bytes(&b"Referrer-Policy"[..], &b"no-referrer"[..]).unwrap());
                    response.add_header(Header::from_bytes(&b"Cross-Origin-Resource-Policy"[..], &b"same-origin"[..]).unwrap());
                    response.add_header(Header::from_bytes(
                        &b"Content-Security-Policy"[..],
                        gap::cloud::SITE_CONTENT_SECURITY_POLICY.as_bytes(),
                    ).unwrap());
                    if site.challenge {
                        response.add_header(Header::from_bytes(&b"WWW-Authenticate"[..], &b"Basic realm=\"Private GAP project\", charset=\"UTF-8\""[..]).unwrap());
                    }
                    let _ = request.respond(response);
                } else {
                    let response = Response::from_string("site not found")
                        .with_status_code(404)
                        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..]).unwrap());
                    let _ = request.respond(response);
                }
                continue;
            }

            // Static binary assets first: the Open Graph card is fetched
            // by crawlers that never send an Accept header we could
            // route on.
            if method == "GET" {
                let clean = path.split('?').next().unwrap_or(&path);
                if let Some((ctype, bytes)) = gap::server::static_asset(clean) {
                    let body: &[u8] = if head_only { &[] } else { bytes };
                    let mut response = Response::from_data(body).with_status_code(200);
                    response.add_header(
                        Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()).unwrap(),
                    );
                    // Immutable for a build: the bytes only change when
                    // the binary does.
                    response.add_header(
                        Header::from_bytes(&b"Cache-Control"[..], &b"public, max-age=86400"[..])
                            .unwrap(),
                    );
                    let _ = request.respond(response);
                    continue;
                }
            }

            // Private static sites own `/sites/`. Unlike the node's embedded
            // artwork these responses are tenant-controlled, authenticated and
            // never publicly cacheable or indexable.
            if method == "GET" && path.starts_with("/sites/") {
                if let Some(site) = gap::server::serve_private_site(
                    &state,
                    &path,
                    auth.as_deref(),
                    client_ip.as_deref(),
                ) {
                    let body: &[u8] = if head_only { &[] } else { &site.body };
                    let mut response = Response::from_data(body).with_status_code(site.status);
                    response.add_header(Header::from_bytes(&b"Content-Type"[..], site.media_type.as_bytes()).unwrap());
                    response.add_header(Header::from_bytes(&b"Cache-Control"[..], &b"private, no-store"[..]).unwrap());
                    response.add_header(Header::from_bytes(&b"X-Robots-Tag"[..], &b"noindex, nofollow, noarchive"[..]).unwrap());
                    response.add_header(Header::from_bytes(&b"X-Content-Type-Options"[..], &b"nosniff"[..]).unwrap());
                    response.add_header(Header::from_bytes(&b"Referrer-Policy"[..], &b"no-referrer"[..]).unwrap());
                    response.add_header(Header::from_bytes(&b"Cross-Origin-Resource-Policy"[..], &b"same-origin"[..]).unwrap());
                    response.add_header(Header::from_bytes(
                        &b"Content-Security-Policy"[..],
                        gap::cloud::SITE_CONTENT_SECURITY_POLICY.as_bytes(),
                    ).unwrap());
                    if site.challenge {
                        response.add_header(Header::from_bytes(&b"WWW-Authenticate"[..], &b"Basic realm=\"Private GAP project\", charset=\"UTF-8\""[..]).unwrap());
                    }
                    let _ = request.respond(response);
                    continue;
                }
            }

            // Web UI (public directory + operator console) before the
            // JSON API: browsers ask for HTML, agents ask for JSON.
            if let Some((ctype, body)) =
                (method == "GET").then(|| gap::cloud_surface::page(&path)).flatten()
            {
                let body = if head_only { String::new() } else if vm_console {
                    let public=env::var("GAP_PUBLIC_URL").unwrap_or_default().trim_end_matches('/').replace('&',"&amp;").replace('"',"&quot;").replace('<',"&lt;");
                    body.replace("href=\"/docs",&format!("href=\"{public}/docs")).replace("href=\"/\"",&format!("href=\"{public}/\""))
                } else { body };
                let mut response = Response::from_string(body).with_status_code(200);
                if vm_console {
                    for (name,value) in [("Cache-Control","no-store"),("X-Frame-Options","DENY"),("Referrer-Policy","no-referrer"),("Content-Security-Policy","frame-ancestors 'none'; base-uri 'none'")] {response.add_header(Header::from_bytes(name,value).unwrap());}
                }
                response.add_header(
                    Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()).unwrap(),
                );
                let _ = request.respond(response);
                continue;
            }

            let header = |name: &'static str| request.headers().iter().find(|h|h.field.equiv(name)).map(|h|h.value.as_str()).unwrap_or("");
            let session_cookie=header("Cookie");
            let session_csrf=header("X-GAP-VM-Session");
            if !custom_domain_request && on_admin_host && auth.is_none() {
                auth=gap::cloud_vm_session::authorization(&path,session_cookie,session_csrf);
            }
            if !custom_domain_request && clean_path.starts_with("/v1/cloud/projects/") && clean_path.ends_with("/browser-session") {
                let project=clean_path.trim_start_matches("/v1/cloud/projects/").trim_end_matches("/browser-session");
                let origin_ok=on_admin_host && header("Origin")==admin_origin.trim_end_matches('/');
                let (status,body,set_cookie)=if !origin_ok {
                    (403,serde_json::json!({"error":{"code":"origin_required"}}),None)
                } else if method=="POST" && !header("Authorization").is_empty() {
                    // Authorization of this project's VM listing proves ownership
                    // and current approval before issuing any session cookie.
                    let (status,result)=route_with_ip(&state,"GET",&format!("/v1/cloud/projects/{project}/vms"),&[],auth.as_deref(),client_ip.as_deref());
                    if status!=200 {(status,result,None)} else if let Some(token)=auth.as_deref() {
                        match gap::cloud_vm_session::issue(project,token) {
                            Ok((body,cookie))=>(201,body,Some(cookie)),
                            Err(())=>(503,serde_json::json!({"error":{"code":"sessions_unavailable"}}),None)
                        }
                    } else {(401,serde_json::json!({"error":{"code":"unauthorized"}}),None)}
                } else if method=="DELETE" {
                    // Origin was checked above. Allow idempotent logout even
                    // after expiry, so an expired session cannot trap the UI.
                    match gap::cloud_vm_session::revoke(session_cookie) {
                        Ok(cookie)=>(200,serde_json::json!({"disconnected":true}),Some(cookie)),
                        Err(())=>(503,serde_json::json!({"error":{"code":"session_revocation_failed"}}),None)
                    }
                } else {(401,serde_json::json!({"error":{"code":"unauthorized"}}),None)};
                let mut response=Response::from_string(body.to_string()).with_status_code(status);
                response.add_header(Header::from_bytes("Content-Type","application/json").unwrap());
                response.add_header(Header::from_bytes("Cache-Control","no-store").unwrap());
                if let Some(cookie)=set_cookie {response.add_header(Header::from_bytes("Set-Cookie",cookie).unwrap());}
                let _=request.respond(response);continue;
            }

            let (status, json_body) = if !gap::cloud_surface::allowed_api(&path) {
                (410, serde_json::json!({"error": {"code": "archived", "message": "This endpoint is archived. See /agents.md for GAP Cloud."}}))
            } else { route_with_ip(
                &state,
                &method,
                &path,
                &body,
                auth.as_deref(),
                client_ip.as_deref(),
            ) };
            let json_str = if head_only {
                String::new()
            } else {
                json_body.to_string()
            };

            let mut response = Response::from_string(json_str).with_status_code(status);
            if vm_console {response.add_header(Header::from_bytes("Cache-Control","no-store").unwrap());}
            response.add_header(
                Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
            );
            if path
                .split('?')
                .next()
                .unwrap_or(&path)
                .starts_with("/functions/")
            {
                response.add_header(
                    Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap(),
                );
                response.add_header(
                    Header::from_bytes(
                        &b"Access-Control-Allow-Methods"[..],
                        &b"GET, POST, PUT, PATCH, DELETE, OPTIONS"[..],
                    )
                    .unwrap(),
                );
                response.add_header(
                    Header::from_bytes(
                        &b"Access-Control-Allow-Headers"[..],
                        &b"Authorization, Content-Type"[..],
                    )
                    .unwrap(),
                );
            }

            let _ = request.respond(response);
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}
