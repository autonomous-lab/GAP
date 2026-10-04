use bytes::{Bytes, BytesMut};
use http_body::{Body, Frame, SizeHint};
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue, AUTHORIZATION, CONNECTION, CONTENT_TYPE, HOST};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::error::Error;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, OwnedSemaphorePermit, RwLock, Semaphore};
use tokio::time::Instant;

type BoxError = Box<dyn Error + Send + Sync>;
type ClientBody = Full<Bytes>;
type HttpClient = Client<HttpConnector, ClientBody>;
type InnerBody = UnsyncBoxBody<Bytes, BoxError>;
const MAX_BODY: usize = 16 * 1024 * 1024;
const MAX_ACTIVE: usize = 512;
const MAX_ACTIVE_PER_VM: usize = 64;
const ADMISSION_CACHE_TTL: Duration = Duration::from_secs(5);
const ADMISSION_REFRESH_LEAD: Duration = Duration::from_secs(3);
type RouteKey = (String, String);
type RouteEntry = Arc<RouteState>;
type AdmissionCache = Arc<Mutex<HashMap<RouteKey, RouteEntry>>>;

struct RouteState {
    current: RwLock<Option<Arc<Admission>>>,
    refresh: Arc<Mutex<()>>,
    slots: Arc<Semaphore>,
}

impl RouteState {
    fn new() -> Self {
        Self { current: RwLock::new(None), refresh: Arc::new(Mutex::new(())),
            slots: Arc::new(Semaphore::new(MAX_ACTIVE_PER_VM)) }
    }
}

#[derive(Clone)]
struct Config {
    bind: SocketAddr,
    control: Uri,
    token: Arc<str>,
    edge_token: Arc<str>,
    client: HttpClient,
    slots: Arc<Semaphore>,
}

struct Lease {
    admission: Arc<Admission>,
    _permit: OwnedSemaphorePermit,
    _route_permit: OwnedSemaphorePermit,
    _route: RouteEntry,
}

struct Admission {
    ticket: String,
    port: u16,
    cold: bool,
    expires: Instant,
    config: Config,
}

impl Drop for Admission {
    fn drop(&mut self) {
        let config = self.config.clone();
        let ticket = self.ticket.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = control(&config, json!({"action":"end","ticket":ticket}), Duration::from_secs(5)).await;
            });
        }
    }
}

async fn fetch_admission(config: &Config, project: &str, vm: &str, deadline: Duration)
    -> Result<Arc<Admission>, (StatusCode, String)> {
    let admitted = control(config,json!({"action":"begin","project_id":project,"vm_id":vm}),deadline).await?;
    let port = admitted["port"].as_u64().and_then(|p| u16::try_from(p).ok()).filter(|p| *p > 0)
        .ok_or((StatusCode::BAD_GATEWAY,"control_port_invalid".to_owned()))?;
    let ticket = admitted["ticket"].as_str().filter(|s| identity(s,"",32))
        .ok_or((StatusCode::BAD_GATEWAY,"control_ticket_invalid".to_owned()))?;
    let admission = Arc::new(Admission { ticket:ticket.to_owned(), port,
        cold:admitted["cold"].as_bool()==Some(true),
        expires:Instant::now()+ADMISSION_CACHE_TTL, config:config.clone() });
    Ok(admission)
}

async fn route_entry(cache: &AdmissionCache, project: &str, vm: &str) -> RouteEntry {
    let mut routes = cache.lock().await;
    routes.entry((project.to_owned(), vm.to_owned()))
        .or_insert_with(|| Arc::new(RouteState::new())).clone()
}

async fn admit(config: &Config, entry: &RouteEntry, project: String, vm: String)
    -> Result<Arc<Admission>, (StatusCode, String)> {
    if let Some(admission) = entry.current.read().await.as_ref() {
        if Instant::now() < admission.expires { return Ok(admission.clone()); }
    }
    // Only one cold/expired request per route reaches Python. Other VMs and
    // still-valid warm requests never wait for this route's refresh.
    let _refresh = entry.refresh.lock().await;
    if let Some(admission) = entry.current.read().await.as_ref() {
        if Instant::now() < admission.expires { return Ok(admission.clone()); }
    }
    *entry.current.write().await = None;
    let admission=fetch_admission(config,&project,&vm,Duration::from_secs(180)).await?;
    *entry.current.write().await = Some(admission.clone());
    Ok(admission)
}

async fn expire_admissions(cache: AdmissionCache, config: Config) {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    loop {
        interval.tick().await;
        let routes: Vec<_> = cache.lock().await.iter().map(|(key,entry)|(key.clone(),entry.clone())).collect();
        for ((project,vm),entry) in routes {
            let current=entry.current.read().await.clone();
            if let Some(admission)=current {
                let now=Instant::now();
                if now>=admission.expires {
                    let mut slot=entry.current.write().await;
                    if slot.as_ref().is_some_and(|latest| now>=latest.expires) {*slot=None;}
                } else if admission.expires-now<=ADMISSION_REFRESH_LEAD {
                    if let Ok(_refresh)=entry.refresh.clone().try_lock_owned() {
                        let entry=entry.clone();
                        let config=config.clone();
                        tokio::spawn(async move {
                            let result=fetch_admission(&config,&project,&vm,Duration::from_secs(3)).await;
                            *entry.current.write().await=result.ok();
                            drop(_refresh);
                        });
                    }
                }
            }
        }
        // Expired/denied routes must not accumulate without bound.
        cache.lock().await.retain(|_,entry| {
            if let Ok(current)=entry.current.try_read() {
                current.is_some() || Arc::strong_count(entry)>1 || entry.refresh.clone().try_lock_owned().is_err()
            } else { true }
        });
    }
}

struct TrackedBody {
    inner: Pin<Box<InnerBody>>,
    lease: Option<Lease>,
}

impl TrackedBody {
    fn new(inner: InnerBody, lease: Option<Lease>) -> Self {
        Self { inner: Box::pin(inner), lease }
    }
}

impl Body for TrackedBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let result = self.inner.as_mut().poll_frame(cx);
        if matches!(result, Poll::Ready(None)) { self.lease.take(); }
        result
    }

    fn is_end_stream(&self) -> bool { self.inner.is_end_stream() }
    fn size_hint(&self) -> SizeHint { self.inner.size_hint() }
}

fn boxed_full(bytes: impl Into<Bytes>) -> InnerBody {
    Full::new(bytes.into()).map_err(|never| -> BoxError { match never {} }).boxed_unsync()
}

fn error_response(status: StatusCode, code: &str) -> Response<TrackedBody> {
    let body = json!({"error":code}).to_string();
    Response::builder().status(status).header(CONTENT_TYPE, "application/json")
        .header("cache-control", "no-store")
        .body(TrackedBody::new(boxed_full(body), None)).expect("static response")
}

fn identity(value: &str, prefix: &str, digits: usize) -> bool {
    value.len() == prefix.len() + digits && value.starts_with(prefix)
        && value[prefix.len()..].bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn is_hop_header(name: &HeaderName) -> bool {
    matches!(name.as_str(), "connection" | "keep-alive" | "proxy-authenticate" |
        "proxy-authorization" | "te" | "trailer" | "transfer-encoding")
}

fn is_private_header(name: &HeaderName) -> bool {
    matches!(name.as_str(), "x-gap-project" | "x-gap-vm" | "x-gap-vm-admission" |
        "x-gap-vm-identity" | "x-gap-origin-authorization" | "x-gap-origin-host")
}

async fn control(config: &Config, payload: Value, deadline: Duration) -> Result<Value, (StatusCode, String)> {
    let body = payload.to_string();
    let req = Request::builder().method(Method::POST).uri(config.control.clone())
        .header(AUTHORIZATION, format!("Bearer {}", config.token))
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body))).map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR,"control_request".into()))?;
    let response = tokio::time::timeout(deadline, config.client.request(req)).await
        .map_err(|_| (StatusCode::GATEWAY_TIMEOUT,"control_timeout".into()))?
        .map_err(|_| (StatusCode::SERVICE_UNAVAILABLE,"control_unavailable".into()))?;
    let status = response.status();
    let bytes = tokio::time::timeout(Duration::from_secs(5), response.into_body().collect()).await
        .map_err(|_| (StatusCode::GATEWAY_TIMEOUT,"control_response_timeout".into()))?
        .map_err(|_| (StatusCode::BAD_GATEWAY,"control_response_invalid".into()))?
        .to_bytes();
    if bytes.len() > 4096 { return Err((StatusCode::BAD_GATEWAY,"control_response_too_large".into())); }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| (StatusCode::BAD_GATEWAY,"control_response_invalid".into()))?;
    if status != StatusCode::OK {
        let code = value.pointer("/error/code").and_then(Value::as_str).unwrap_or("control_denied");
        return Err((status,code.to_owned()));
    }
    Ok(value)
}

async fn read_body(mut body: Incoming) -> Result<Bytes, StatusCode> {
    let mut bytes = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| StatusCode::BAD_REQUEST)?;
        if let Ok(data) = frame.into_data() {
            if bytes.len().saturating_add(data.len()) > MAX_BODY { return Err(StatusCode::PAYLOAD_TOO_LARGE); }
            bytes.extend_from_slice(&data);
        }
    }
    Ok(bytes.freeze())
}

async fn proxy(mut request: Request<Incoming>, config: Config, cache: AdmissionCache) -> Response<TrackedBody> {
    let headers = request.headers();
    let supplied = headers.get("x-gap-vm-admission").map(HeaderValue::as_bytes).unwrap_or_default();
    if supplied.len() != config.edge_token.len() || supplied.ct_eq(config.edge_token.as_bytes()).unwrap_u8() != 1 {
        return error_response(StatusCode::FORBIDDEN,"http_admission_required");
    }
    let project = match headers.get("x-gap-project").and_then(|h| h.to_str().ok()) {
        Some(v) if identity(v,"prj_",24) => v.to_owned(),
        _ => return error_response(StatusCode::NOT_FOUND,"unknown_application"),
    };
    let vm = match headers.get("x-gap-vm").and_then(|h| h.to_str().ok()) {
        Some(v) if identity(v,"vm_",32) => v.to_owned(),
        _ => return error_response(StatusCode::NOT_FOUND,"unknown_application"),
    };
    if request.uri().path().len() > 8192 || request.uri().to_string().len() > 16384 {
        return error_response(StatusCode::FORBIDDEN,"invalid_http_path");
    }
    if request.headers().get_all(hyper::header::CONTENT_LENGTH).iter().count() > 1
        || request.headers().get(hyper::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok()).is_some_and(|n| n > MAX_BODY) {
        return error_response(StatusCode::PAYLOAD_TOO_LARGE,"request_body_too_large");
    }
    let route = route_entry(&cache,&project,&vm).await;
    let route_permit = match route.slots.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return error_response(StatusCode::TOO_MANY_REQUESTS,"vm_http_gateway_busy"),
    };
    let permit = match config.slots.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return error_response(StatusCode::SERVICE_UNAVAILABLE,"http_gateway_busy"),
    };
    let admission = match admit(&config,&route,project,vm).await {
        Ok(value) => value,
        Err((status,code)) => return error_response(status,&code),
    };
    let port = admission.port;
    let lease = Lease {admission, _permit: permit, _route_permit: route_permit, _route: route};
    if lease.admission.cold {
        let target=SocketAddr::from(([127,0,0,1],port));
        let deadline=tokio::time::Instant::now()+Duration::from_secs(90);
        loop {
            if let Ok(Ok(stream))=tokio::time::timeout(Duration::from_secs(2),TcpStream::connect(target)).await {
                drop(stream);
                break;
            }
            if tokio::time::Instant::now()>=deadline {
                return error_response(StatusCode::SERVICE_UNAVAILABLE,"application_not_ready");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    let upgrading = request.headers().get(hyper::header::UPGRADE).is_some();
    let downstream_upgrade = upgrading.then(|| hyper::upgrade::on(&mut request));
    let uri = match format!("http://127.0.0.1:{port}{}",request.uri()).parse::<Uri>() {
        Ok(uri) => uri,
        Err(_) => return error_response(StatusCode::BAD_REQUEST,"invalid_http_uri"),
    };
    let (mut parts,body) = request.into_parts();
    parts.uri = uri;
    let remove: Vec<_> = parts.headers.keys()
        .filter(|name| is_private_header(name) || is_hop_header(name) && (!upgrading || *name!=CONNECTION))
        .cloned().collect();
    for name in remove { parts.headers.remove(name); }
    if !parts.headers.contains_key(HOST) {
        parts.headers.insert(HOST,HeaderValue::from_static("localhost"));
    }
    let bytes = match read_body(body).await {
        Ok(bytes) => bytes,
        Err(status) => return error_response(status,"invalid_request_body"),
    };
    let upstream_req = Request::from_parts(parts,Full::new(bytes));
    let mut upstream = match tokio::time::timeout(Duration::from_secs(120),config.client.request(upstream_req)).await {
        Ok(Ok(response)) => response,
        _ => return error_response(StatusCode::SERVICE_UNAVAILABLE,"application_gateway_failed"),
    };
    let upstream_upgrade = (upgrading && upstream.status()==StatusCode::SWITCHING_PROTOCOLS)
        .then(|| hyper::upgrade::on(&mut upstream));
    let (mut parts,body) = upstream.into_parts();
    let remove: Vec<_> = parts.headers.keys()
        .filter(|name| is_hop_header(name) && (upstream_upgrade.is_none() || *name!=CONNECTION))
        .cloned().collect();
    for name in remove { parts.headers.remove(name); }
    parts.headers.remove("service-worker-allowed");
    if let (Some(downstream),Some(upstream))=(downstream_upgrade,upstream_upgrade) {
        tokio::spawn(async move {
            if let (Ok(downstream),Ok(upstream))=tokio::join!(downstream,upstream) {
                let (mut downstream,mut upstream)=(TokioIo::new(downstream),TokioIo::new(upstream));
                let mut heartbeat=tokio::time::interval(Duration::from_secs(15));
                heartbeat.tick().await;
                let transfer=copy_bidirectional(&mut downstream,&mut upstream);
                tokio::pin!(transfer);
                loop {
                    tokio::select! {
                        _=&mut transfer => break,
                        _=heartbeat.tick() => {
                            let _=control(&lease.admission.config,json!({"action":"touch","ticket":lease.admission.ticket}),Duration::from_secs(5)).await;
                        }
                    }
                }
            }
            drop(lease);
        });
        return Response::from_parts(parts,TrackedBody::new(boxed_full(Bytes::new()),None));
    }
    let body = body.map_err(|e| -> BoxError { Box::new(e) }).boxed_unsync();
    Response::from_parts(parts,TrackedBody::new(body,Some(lease)))
}

fn argument(args: &[String], name: &str) -> Result<String, BoxError> {
    let index = args.iter().position(|arg| arg==name).ok_or("missing gateway argument")?;
    Ok(args.get(index+1).ok_or("missing gateway argument value")?.clone())
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let args: Vec<String> = std::env::args().collect();
    let bind = argument(&args,"--bind")?.parse()?;
    let control = argument(&args,"--control")?.parse()?;
    let token = std::fs::read_to_string(argument(&args,"--token-file")?)?.trim().to_owned();
    let edge_token = std::fs::read_to_string(argument(&args,"--edge-token-file")?)?.trim().to_owned();
    if token.len() < 32 || edge_token.len() < 32 { return Err("gateway token unavailable".into()); }
    let mut connector = HttpConnector::new();
    connector.enforce_http(true);
    connector.set_nodelay(true);
    let client = Client::builder(TokioExecutor::new()).build(connector);
    let config = Config {bind,control,token:token.into(),edge_token:edge_token.into(),client,
                         slots:Arc::new(Semaphore::new(MAX_ACTIVE))};
    let cache: AdmissionCache = Arc::new(Mutex::new(HashMap::new()));
    tokio::spawn(expire_admissions(cache.clone(),config.clone()));
    let listener = TcpListener::bind(config.bind).await?;
    loop {
        let (stream,_) = listener.accept().await?;
        stream.set_nodelay(true)?;
        let config=config.clone();
        let cache=cache.clone();
        tokio::spawn(async move {
            let service=service_fn(move |request| {
                let config=config.clone();
                let cache=cache.clone();
                async move { Ok::<_,Infallible>(proxy(request,config,cache).await) }
            });
            if let Err(error)=http1::Builder::new().serve_connection(TokioIo::new(stream),service).with_upgrades().await {
                eprintln!("http gateway connection: {error}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_generated_identifiers() {
        assert!(identity(&format!("prj_{}","a".repeat(24)),"prj_",24));
        assert!(!identity(&format!("prj_{}","A".repeat(24)),"prj_",24));
        assert!(!identity("prj_../etc/passwd","prj_",24));
    }
    #[test]
    fn private_headers_never_reach_guest() {
        assert!(is_private_header(&HeaderName::from_static("x-gap-vm-admission")));
        assert!(!is_private_header(&HOST));
    }
}
