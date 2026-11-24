use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc as tokio_mpsc;
use tokio_rustls::TlsConnector;

use crate::memory::SharedMem;
use crate::runtime::{MainThreadTask, RuntimeState};

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub final_url: String,
}

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout_ms: u32,
}

pub trait HttpBackend: Send + Sync + 'static {
    fn request(&self, req: &HttpRequest) -> Result<HttpResponse, String>;
}

pub const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/134.0.0.0 Safari/537.36";

pub struct ReqwestHttpBackend {
    client: OnceLock<reqwest::blocking::Client>,
    csrf_token: Mutex<Option<String>>,
}

impl ReqwestHttpBackend {
    pub fn new() -> Self {
        Self {
            client: OnceLock::new(),
            csrf_token: Mutex::new(None),
        }
    }

    fn client(&self) -> &reqwest::blocking::Client {
        self.client.get_or_init(|| {
            reqwest::blocking::Client::builder()
                .user_agent(BROWSER_USER_AGENT)
                .cookie_store(true)
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::blocking::Client::new())
        })
    }

    fn send_once(
        &self,
        req: &HttpRequest,
        extra_csrf: Option<&str>,
    ) -> Result<HttpResponse, String> {
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|e| format!("invalid HTTP method {:?}: {e}", req.method))?;
        let mut builder = self.client().request(method, &req.url);
        if req.timeout_ms > 0 {
            builder = builder.timeout(Duration::from_millis(req.timeout_ms as u64));
        }
        let mut has_csrf_hdr = false;
        for (k, v) in &req.headers {
            if k.eq_ignore_ascii_case("x-csrf-token") {
                has_csrf_hdr = true;
            }
            builder = builder.header(k.as_str(), v.as_str());
        }
        if !has_csrf_hdr {
            if let Some(tok) = extra_csrf {
                builder = builder.header("x-csrf-token", tok);
            }
        }
        if let Some(ref body) = req.body {
            builder = builder.body(body.clone());
        }

        let resp = builder.send().map_err(|e| format!("HTTP request failed: {e}"))?;
        let status = resp.status().as_u16();
        let status_text = resp
            .status()
            .canonical_reason()
            .unwrap_or("")
            .to_owned();
        let final_url = resp.url().to_string();
        let mut headers = Vec::new();
        for (k, v) in resp.headers() {
            if let Ok(vs) = v.to_str() {
                headers.push((k.as_str().to_owned(), vs.to_owned()));
            }
        }
        let body = resp
            .bytes()
            .map_err(|e| format!("failed reading HTTP body: {e}"))?
            .to_vec();

        Ok(HttpResponse {
            status,
            status_text,
            headers,
            body,
            final_url,
        })
    }
}

impl HttpBackend for ReqwestHttpBackend {
    fn request(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
        let is_rbx = req.url.contains("roblox.com") || req.url.contains("robloxlabs.com");
        let is_mutating = !req.method.eq_ignore_ascii_case("GET")
            && !req.method.eq_ignore_ascii_case("HEAD");
        let cached_csrf = if is_rbx && is_mutating {
            self.csrf_token.lock().unwrap().clone()
        } else {
            None
        };

        let resp = self.send_once(req, cached_csrf.as_deref())?;
        if resp.status == 403 && is_rbx && is_mutating {
            let new_csrf = resp
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("x-csrf-token"))
                .map(|(_, v)| v.trim().to_owned())
                .filter(|v| !v.is_empty());
            if let Some(tok) = new_csrf {
                *self.csrf_token.lock().unwrap() = Some(tok.clone());
                return self.send_once(req, Some(&tok));
            }
        }
        Ok(resp)
    }
}

pub trait WebSocketBackend: Send + Sync + 'static {
    fn is_supported(&self) -> bool {
        false
    }
    fn connect(&self, _id: i32, _url: &str, _protocols: &[String]) -> i32 {
        -1
    }
    fn send_text(&self, _id: i32, _text: &str) -> i32 {
        -3
    }
    fn send_binary(&self, _id: i32, _data: &[u8]) -> i32 {
        -3
    }
    fn close(&self, _id: i32, _code: u16, _reason: Option<&str>) -> i32 {
        0
    }
}

enum WsOutMsg {
    Text(String),
    Binary(Vec<u8>),
    Close(u16, Option<String>),
}

pub struct TokioWebSocketBackend {
    tokio_rt: OnceLock<tokio::runtime::Runtime>,
    runtime_ref: Mutex<Option<Weak<RuntimeState>>>,
    senders: Mutex<HashMap<i32, tokio_mpsc::UnboundedSender<WsOutMsg>>>,
}

impl TokioWebSocketBackend {
    pub fn new() -> Self {
        Self {
            tokio_rt: OnceLock::new(),
            runtime_ref: Mutex::new(None),
            senders: Mutex::new(HashMap::new()),
        }
    }

    pub fn attach_runtime(&self, rt: Weak<RuntimeState>) {
        *self.runtime_ref.lock().unwrap() = Some(rt);
    }

    fn rt(&self) -> &tokio::runtime::Runtime {
        self.tokio_rt.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("failed to build tokio runtime for WebSocket")
        })
    }

    fn push_event(&self, task: MainThreadTask) {
        if let Some(weak) = self.runtime_ref.lock().unwrap().as_ref() {
            if let Some(rt) = weak.upgrade() {
                rt.push_main_task(task);
            }
        }
    }
}

impl WebSocketBackend for TokioWebSocketBackend {
    fn is_supported(&self) -> bool {
        true
    }

    fn connect(&self, id: i32, url: &str, protocols: &[String]) -> i32 {
        let Ok(parsed) = parse_ws_url(url) else {
            return -5;
        };
        let (tx, mut rx) = tokio_mpsc::unbounded_channel::<WsOutMsg>();
        self.senders.lock().unwrap().insert(id, tx);

        let weak_rt = self.runtime_ref.lock().unwrap().clone();
        let protos = protocols.to_vec();
        let extra_headers = if let Some(ref w) = weak_rt {
            if let Some(rt) = w.upgrade() {
                build_roblox_ws_headers(&parsed.host, &rt.config.roblox_base_url, rt.config.auth_cookie.as_deref())
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        self.rt().spawn(async move {
            let push = |task: MainThreadTask| {
                if let Some(ref w) = weak_rt {
                    if let Some(rt) = w.upgrade() {
                        rt.push_main_task(task);
                    }
                }
            };

            let res = run_ws_connection(id, &parsed, &protos, &extra_headers, &mut rx, &push).await;
            if let Err(err) = res {
                eprintln!("[WebSocket:{id}] error: {err}");
                push(MainThreadTask::WebSocketEvent {
                    ws_id: id,
                    kind: 2,
                    data: Vec::new(),
                    is_text: false,
                    was_clean: false,
                    close_code: 1006,
                    close_reason: err,
                });
            }
        });

        0
    }

    fn send_text(&self, id: i32, text: &str) -> i32 {
        if let Some(tx) = self.senders.lock().unwrap().get(&id) {
            if tx.send(WsOutMsg::Text(text.to_owned())).is_ok() {
                return 0;
            }
        }
        -3
    }

    fn send_binary(&self, id: i32, data: &[u8]) -> i32 {
        if let Some(tx) = self.senders.lock().unwrap().get(&id) {
            if tx.send(WsOutMsg::Binary(data.to_vec())).is_ok() {
                return 0;
            }
        }
        -3
    }

    fn close(&self, id: i32, code: u16, reason: Option<&str>) -> i32 {
        if let Some(tx) = self.senders.lock().unwrap().remove(&id) {
            let _ = tx.send(WsOutMsg::Close(code, reason.map(str::to_owned)));
            0
        } else {
            let _ = self.push_event(MainThreadTask::WebSocketEvent {
                ws_id: id,
                kind: 3,
                data: Vec::new(),
                is_text: false,
                was_clean: true,
                close_code: if code == 0 { 1000 } else { code },
                close_reason: reason.unwrap_or("").to_owned(),
            });
            0
        }
    }
}

struct ParsedWsUrl {
    tls: bool,
    host: String,
    port: u16,
    path_and_query: String,
}

fn parse_ws_url(url: &str) -> Result<ParsedWsUrl, String> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("ws://") {
        (false, r)
    } else {
        return Err(format!("unsupported WebSocket scheme: {url}"));
    };
    let (authority, path_and_query) = match rest.find('/') {
        Some(idx) => (&rest[..idx], rest[idx..].to_owned()),
        None => (rest, "/".to_owned()),
    };
    let (host, port) = if let Some(colon) = authority.rfind(':') {
        if let Ok(p) = authority[colon + 1..].parse::<u16>() {
            (authority[..colon].to_owned(), p)
        } else {
            (authority.to_owned(), if tls { 443 } else { 80 })
        }
    } else {
        (authority.to_owned(), if tls { 443 } else { 80 })
    };
    Ok(ParsedWsUrl {
        tls,
        host,
        port,
        path_and_query,
    })
}

pub fn normalize_roblosecurity(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('"').trim_matches('\'').trim();
    if let Some(rest) = trimmed.strip_prefix(".ROBLOSECURITY=") {
        rest.split(';').next().unwrap_or(rest).trim().to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn build_roblox_ws_headers(
    host: &str,
    roblox_base_url: &str,
    auth_cookie: Option<&str>,
) -> Vec<(String, String)> {
    let mut hdrs = vec![("User-Agent".to_owned(), BROWSER_USER_AGENT.to_owned())];
    let is_rbx = host.ends_with(".roblox.com")
        || host == "roblox.com"
        || host.ends_with(".robloxlabs.com");
    if is_rbx {
        let origin = roblox_base_url.trim_end_matches('/');
        hdrs.push(("Origin".to_owned(), origin.to_owned()));
        if let Some(c) = auth_cookie {
            let clean = normalize_roblosecurity(c);
            if !clean.is_empty() {
                hdrs.push(("Cookie".to_owned(), format!(".ROBLOSECURITY={clean}")));
            }
        }
    }
    hdrs
}

async fn run_ws_connection(
    id: i32,
    parsed: &ParsedWsUrl,
    protocols: &[String],
    extra_headers: &[(String, String)],
    rx: &mut tokio_mpsc::UnboundedReceiver<WsOutMsg>,
    push: &impl Fn(MainThreadTask),
) -> Result<(), String> {
    let addr = format!("{}:{}", parsed.host, parsed.port);
    let tcp = TcpStream::connect(&addr)
        .await
        .map_err(|e| format!("TCP connect {addr} failed: {e}"))?;

    if parsed.tls {
        use rustls_platform_verifier::ConfigVerifierExt;
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let tls_cfg = rustls::ClientConfig::with_platform_verifier()
            .map_err(|e| format!("TLS verifier init failed: {e}"))?;
        let connector = TlsConnector::from(Arc::new(tls_cfg));
        let server_name = rustls::pki_types::ServerName::try_from(parsed.host.clone())
            .map_err(|e| format!("invalid DNS name {}: {e}", parsed.host))?;
        let stream = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| format!("TLS handshake failed: {e}"))?;
        drive_ws_stream(id, parsed, protocols, extra_headers, stream, rx, push).await
    } else {
        drive_ws_stream(id, parsed, protocols, extra_headers, tcp, rx, push).await
    }
}

async fn drive_ws_stream<S>(
    id: i32,
    parsed: &ParsedWsUrl,
    protocols: &[String],
    extra_headers: &[(String, String)],
    stream: S,
    rx: &mut tokio_mpsc::UnboundedReceiver<WsOutMsg>,
    push: &impl Fn(MainThreadTask),
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    let host_header = if (parsed.tls && parsed.port == 443) || (!parsed.tls && parsed.port == 80) {
        parsed.host.clone()
    } else {
        format!("{}:{}", parsed.host, parsed.port)
    };
    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n",
        parsed.path_and_query, host_header
    );
    if !protocols.is_empty() {
        req.push_str(&format!("Sec-WebSocket-Protocol: {}\r\n", protocols.join(", ")));
    }
    for (k, v) in extra_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    write_half
        .write_all(req.as_bytes())
        .await
        .map_err(|e| format!("WS handshake write failed: {e}"))?;

    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .await
        .map_err(|e| format!("WS handshake read failed: {e}"))?;
    if !status_line.contains("101") {
        return Err(format!("WS handshake rejected: {}", status_line.trim()));
    }
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("WS header read failed: {e}"))?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
    }

    push(MainThreadTask::WebSocketEvent {
        ws_id: id,
        kind: 0,
        data: Vec::new(),
        is_text: false,
        was_clean: false,
        close_code: 0,
        close_reason: String::new(),
    });

    let mut frag_buf: Vec<u8> = Vec::new();
    let mut frag_is_text = false;

    loop {
        tokio::select! {
            out = rx.recv() => {
                match out {
                    Some(WsOutMsg::Text(t)) => {
                        write_ws_frame(&mut write_half, 0x1, t.as_bytes()).await?;
                    }
                    Some(WsOutMsg::Binary(b)) => {
                        write_ws_frame(&mut write_half, 0x2, &b).await?;
                    }
                    Some(WsOutMsg::Close(code, reason)) => {
                        let c = if code == 0 { 1000 } else { code };
                        let mut payload = c.to_be_bytes().to_vec();
                        if let Some(r) = reason {
                            payload.extend_from_slice(r.as_bytes());
                        }
                        let _ = write_ws_frame(&mut write_half, 0x8, &payload).await;
                        push(MainThreadTask::WebSocketEvent {
                            ws_id: id,
                            kind: 3,
                            data: Vec::new(),
                            is_text: false,
                            was_clean: true,
                            close_code: c,
                            close_reason: String::new(),
                        });
                        return Ok(());
                    }
                    None => return Ok(()),
                }
            }
            frame = read_ws_frame(&mut reader) => {
                let (fin, opcode, payload) = frame?;
                match opcode {
                    0x1 | 0x2 => {
                        if fin {
                            push(MainThreadTask::WebSocketEvent {
                                ws_id: id,
                                kind: 1,
                                data: payload,
                                is_text: opcode == 0x1,
                                was_clean: false,
                                close_code: 0,
                                close_reason: String::new(),
                            });
                        } else {
                            frag_is_text = opcode == 0x1;
                            frag_buf = payload;
                        }
                    }
                    0x0 => {
                        frag_buf.extend_from_slice(&payload);
                        if fin {
                            let full = std::mem::take(&mut frag_buf);
                            push(MainThreadTask::WebSocketEvent {
                                ws_id: id,
                                kind: 1,
                                data: full,
                                is_text: frag_is_text,
                                was_clean: false,
                                close_code: 0,
                                close_reason: String::new(),
                            });
                        }
                    }
                    0x8 => {
                        let code = if payload.len() >= 2 {
                            u16::from_be_bytes([payload[0], payload[1]])
                        } else {
                            1000
                        };
                        let reason = if payload.len() > 2 {
                            String::from_utf8_lossy(&payload[2..]).into_owned()
                        } else {
                            String::new()
                        };
                        let _ = write_ws_frame(&mut write_half, 0x8, &code.to_be_bytes()).await;
                        push(MainThreadTask::WebSocketEvent {
                            ws_id: id,
                            kind: 3,
                            data: Vec::new(),
                            is_text: false,
                            was_clean: true,
                            close_code: code,
                            close_reason: reason,
                        });
                        return Ok(());
                    }
                    0x9 => {
                        let _ = write_ws_frame(&mut write_half, 0xA, &payload).await;
                    }
                    0xA => {}
                    _ => {}
                }
            }
        }
    }
}

async fn write_ws_frame<W: tokio::io::AsyncWrite + Unpin>(
    w: &mut W,
    opcode: u8,
    payload: &[u8],
) -> Result<(), String> {
    let mut header = Vec::with_capacity(14);
    header.push(0x80 | (opcode & 0x0F));
    let len = payload.len();
    if len < 126 {
        header.push(0x80 | (len as u8));
    } else if len <= 65535 {
        header.push(0x80 | 126);
        header.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        header.push(0x80 | 127);
        header.extend_from_slice(&(len as u64).to_be_bytes());
    }
    let mask: [u8; 4] = [0x12, 0x34, 0x56, 0x78];
    header.extend_from_slice(&mask);
    let mut masked = payload.to_vec();
    for (i, b) in masked.iter_mut().enumerate() {
        *b ^= mask[i & 3];
    }
    w.write_all(&header)
        .await
        .map_err(|e| format!("WS frame header write failed: {e}"))?;
    w.write_all(&masked)
        .await
        .map_err(|e| format!("WS frame payload write failed: {e}"))?;
    Ok(())
}

async fn read_ws_frame<R: tokio::io::AsyncRead + Unpin>(
    r: &mut R,
) -> Result<(bool, u8, Vec<u8>), String> {
    let mut hdr = [0u8; 2];
    r.read_exact(&mut hdr)
        .await
        .map_err(|e| format!("WS read closed: {e}"))?;
    let fin = (hdr[0] & 0x80) != 0;
    let opcode = hdr[0] & 0x0F;
    let masked = (hdr[1] & 0x80) != 0;
    let mut len = (hdr[1] & 0x7F) as u64;
    if len == 126 {
        let mut ext = [0u8; 2];
        r.read_exact(&mut ext).await.map_err(|e| e.to_string())?;
        len = u16::from_be_bytes(ext) as u64;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        r.read_exact(&mut ext).await.map_err(|e| e.to_string())?;
        len = u64::from_be_bytes(ext);
    }
    let mut mask = [0u8; 4];
    if masked {
        r.read_exact(&mut mask).await.map_err(|e| e.to_string())?;
    }
    if len > 64 * 1024 * 1024 {
        return Err("WS frame exceeded 64 MiB".into());
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).await.map_err(|e| e.to_string())?;
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i & 3];
        }
    }
    Ok((fin, opcode, payload))
}

#[derive(Debug, Clone)]
pub struct RnaConnectParams {
    pub conn_id: u32,
    pub owner_thread: i32,
    pub url: String,
    pub cert_hashes: Vec<String>,
    pub rx_data_ptr: i32,
    pub rx_head_ptr: i32,
    pub rx_tail_ptr: i32,
    pub rx_slot_size: u32,
    pub rx_slot_count: u32,
    pub use_stream_err_code: bool,
    pub fix_conn_ownership: bool,
    #[allow(dead_code)]
    pub datagram_compat: bool,
    pub report_handshake: bool,
    pub stats_interval_ms: u32,
}

pub trait RnaBackend: Send + Sync + 'static {
    fn is_supported(&self) -> bool {
        false
    }
    fn connect(&self, _params: RnaConnectParams) {}
    fn open_stream(&self, _conn_id: u32, _stream_id: i32) {}
    fn write_stream(&self, _conn_id: u32, _stream_id: i32, _data: &[u8]) {}
    fn shutdown_stream_write(&self, _conn_id: u32, _stream_id: i32) {}
    fn shutdown_stream_read(&self, _conn_id: u32, _stream_id: i32) {}
    fn pause_stream_read(&self, _conn_id: u32, _stream_id: i32) {}
    fn resume_stream_read(&self, _conn_id: u32, _stream_id: i32) {}
    fn receive_ring_consumed(&self, _conn_id: u32, _kind: i32, _stream_id: i32) {}
    fn send_datagram(&self, _conn_id: u32, _data: &[u8], _hi: i32, _lo: i32) {}
    fn send_connection_close(&self, _conn_id: u32) {}
    fn close(&self, _conn_id: u32) {}
}

#[allow(dead_code)]
pub struct NullRnaBackend;
impl RnaBackend for NullRnaBackend {}

pub use crate::audio::{AudioBackend, NullAudioBackend};

#[derive(Debug, Clone)]
pub struct GlContextInfo {
    pub id: i32,
    pub major_version: i32,
    pub minor_version: i32,
    pub alpha: bool,
    pub depth: bool,
    pub stencil: bool,
    pub antialias: bool,
    pub premultiplied_alpha: bool,
    pub preserve_drawing_buffer: bool,
    pub enable_extensions_by_default: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GlMappedBuffer {
    pub target: i32,
    pub offset: i32,
    pub length: i32,
    pub guest_mem: i32,
    pub access: i32,
}

pub trait WebGlBackend: Send + Sync + 'static {
    fn create_context(&self, attrs: GlContextInfo) -> i32;
    fn destroy_context(&self, ctx: i32) -> i32;
    fn make_current(&self, ctx: i32) -> i32;
    fn get_context(&self, ctx: i32) -> Option<GlContextInfo>;
    fn next_object_id(&self) -> i32;
    fn get_error(&self) -> i32;
    fn get_string_cached(&self, name: i32) -> Option<i32>;
    fn set_string_cached(&self, name: i32, ptr: i32);
    fn set_mapped_buffer(&self, target: i32, buf: GlMappedBuffer);
    fn take_mapped_buffer(&self, target: i32) -> Option<GlMappedBuffer>;
    fn set_pixel_store(&self, pname: i32, param: i32);
}

pub struct HeadlessWebGl {
    next_id: AtomicI32,
    next_ctx: AtomicI32,
    current_ctx: AtomicI32,
    contexts: Mutex<HashMap<i32, GlContextInfo>>,
    string_cache: Mutex<HashMap<i32, i32>>,
    mapped_buffers: Mutex<HashMap<i32, GlMappedBuffer>>,
    unpack_alignment: AtomicI32,
    unpack_row_length: AtomicI32,
}

impl HeadlessWebGl {
    pub fn new() -> Self {
        Self {
            next_id: AtomicI32::new(1),
            next_ctx: AtomicI32::new(1),
            current_ctx: AtomicI32::new(0),
            contexts: Mutex::new(HashMap::new()),
            string_cache: Mutex::new(HashMap::new()),
            mapped_buffers: Mutex::new(HashMap::new()),
            unpack_alignment: AtomicI32::new(4),
            unpack_row_length: AtomicI32::new(0),
        }
    }
}

impl WebGlBackend for HeadlessWebGl {
    fn create_context(&self, mut attrs: GlContextInfo) -> i32 {
        let id = self.next_ctx.fetch_add(1, Ordering::Relaxed);
        attrs.id = id;
        self.contexts.lock().unwrap().insert(id, attrs);
        id
    }

    fn destroy_context(&self, ctx: i32) -> i32 {
        self.contexts.lock().unwrap().remove(&ctx);
        let _ = self
            .current_ctx
            .compare_exchange(ctx, 0, Ordering::SeqCst, Ordering::SeqCst);
        0
    }

    fn make_current(&self, ctx: i32) -> i32 {
        if ctx == 0 {
            self.current_ctx.store(0, Ordering::SeqCst);
            return 0;
        }
        if self.contexts.lock().unwrap().contains_key(&ctx) {
            self.current_ctx.store(ctx, Ordering::SeqCst);
            0
        } else {
            -5
        }
    }

    fn get_context(&self, ctx: i32) -> Option<GlContextInfo> {
        self.contexts.lock().unwrap().get(&ctx).cloned()
    }

    fn next_object_id(&self) -> i32 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn get_error(&self) -> i32 {
        0
    }

    fn get_string_cached(&self, name: i32) -> Option<i32> {
        self.string_cache.lock().unwrap().get(&name).copied()
    }

    fn set_string_cached(&self, name: i32, ptr: i32) {
        self.string_cache.lock().unwrap().insert(name, ptr);
    }

    fn set_mapped_buffer(&self, target: i32, buf: GlMappedBuffer) {
        self.mapped_buffers.lock().unwrap().insert(target, buf);
    }

    fn take_mapped_buffer(&self, target: i32) -> Option<GlMappedBuffer> {
        self.mapped_buffers.lock().unwrap().remove(&target)
    }

    fn set_pixel_store(&self, pname: i32, param: i32) {
        if pname == 3317 {
            self.unpack_alignment.store(param, Ordering::Relaxed);
        } else if pname == 3314 {
            self.unpack_row_length.store(param, Ordering::Relaxed);
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct WgpuBufferMeta {
    pub size: u64,
    pub usage: u32,
    pub mapped_ptr: i32,
}

#[derive(Debug, Clone)]
pub struct WgpuTextureMeta {
    pub depth_or_layers: i32,
    pub dimension: i32,
    pub usage: u64,
}

#[allow(unused_variables)]
pub trait WebGpuBackend: Send + Sync + 'static {
    fn adapter_names(&self) -> (String, String) {
        ("WeldRuntime".into(), "Headless".into())
    }
    fn get_or_init_device(&self) -> Option<(i32, i32)>;
    fn set_device(&self, device_id: i32, queue_id: i32);
    fn preferred_format(&self) -> i32 {
        27 // bgra8unorm
    }
    fn has_feature(&self, feature: i32) -> i32 {
        0
    }
    fn fill_limits(&self, mem: &SharedMem, r: i32) -> wasmtime::Result<()>;
    fn device_destroy(&self) {}

    fn create_buffer(
        &self,
        mem: &SharedMem,
        id: i32,
        desc_ptr: i32,
        size: u64,
        usage: u32,
        mapped: bool,
        mapped_ptr: i32,
    ) -> wasmtime::Result<bool>;
    fn get_buffer(&self, id: i32) -> Option<WgpuBufferMeta>;
    fn set_buffer_mapped_ptr(&self, id: i32, ptr: i32);
    fn map_async(
        &self,
        mem: &SharedMem,
        buf_id: i32,
        mode: i64,
        offset: i32,
        size: i32,
        mapped_ptr: i32,
    ) -> wasmtime::Result<i32> {
        Ok(1)
    }
    fn unmap_buffer(&self, mem: &SharedMem, id: i32) -> wasmtime::Result<i32>;

    fn create_shader_module(&self, mem: &SharedMem, id: i32, desc_ptr: i32) -> wasmtime::Result<()> {
        Ok(())
    }
    fn get_shader_compilation_info(
        &self,
        mem: &SharedMem,
        id: i32,
        struct_ptr: i32,
    ) -> wasmtime::Result<i32> {
        if struct_ptr != 0 {
            mem.write_u32(struct_ptr + 4, 0)?;
        }
        Ok(1)
    }

    fn create_texture(&self, mem: &SharedMem, id: i32, desc_ptr: i32) -> wasmtime::Result<()>;
    fn get_texture(&self, id: i32) -> Option<WgpuTextureMeta>;
    fn create_texture_view(
        &self,
        mem: &SharedMem,
        id: i32,
        tex_id: i32,
        desc_ptr: i32,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
    fn create_sampler(&self, mem: &SharedMem, id: i32, desc_ptr: i32) -> wasmtime::Result<()> {
        Ok(())
    }
    fn create_bind_group_layout(
        &self,
        mem: &SharedMem,
        id: i32,
        desc_ptr: i32,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
    fn create_pipeline_layout(
        &self,
        mem: &SharedMem,
        id: i32,
        desc_ptr: i32,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
    fn create_bind_group(&self, mem: &SharedMem, id: i32, desc_ptr: i32) -> wasmtime::Result<()> {
        Ok(())
    }
    fn create_render_pipeline(
        &self,
        mem: &SharedMem,
        id: i32,
        desc_ptr: i32,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
    fn create_compute_pipeline(
        &self,
        mem: &SharedMem,
        id: i32,
        desc_ptr: i32,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
    fn create_query_set(&self, mem: &SharedMem, id: i32, desc_ptr: i32) -> wasmtime::Result<()> {
        Ok(())
    }
    fn create_surface(&self, id: i32) {}
    fn surface_configure(&self, mem: &SharedMem, surf_id: i32, cfg_ptr: i32) -> wasmtime::Result<()> {
        Ok(())
    }

    fn create_command_encoder(
        &self,
        mem: &SharedMem,
        id: i32,
        desc_ptr: i32,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
    fn encoder_begin_render_pass(
        &self,
        mem: &SharedMem,
        enc_id: i32,
        pass_id: i32,
        desc_ptr: i32,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
    fn encoder_begin_compute_pass(
        &self,
        mem: &SharedMem,
        enc_id: i32,
        pass_id: i32,
        desc_ptr: i32,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
    fn encoder_finish(&self, enc_id: i32, cmd_id: i32) {}
    fn encoder_copy_buffer_to_buffer(
        &self,
        enc_id: i32,
        src: i32,
        src_off: u64,
        dst: i32,
        dst_off: u64,
        size: u64,
    ) {
    }
    fn encoder_copy_texture_to_texture(
        &self,
        mem: &SharedMem,
        enc_id: i32,
        src_ptr: i32,
        dst_ptr: i32,
        ext_ptr: i32,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
    fn encoder_copy_texture_to_buffer(
        &self,
        mem: &SharedMem,
        enc_id: i32,
        src_ptr: i32,
        dst_ptr: i32,
        ext_ptr: i32,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
    fn encoder_resolve_query_set(
        &self,
        enc_id: i32,
        qs_id: i32,
        first: u32,
        count: u32,
        dst_buf: i32,
        dst_off: u64,
    ) {
    }
    fn encoder_push_debug_group(&self, mem: &SharedMem, enc_id: i32, lbl_ptr: i32) -> wasmtime::Result<()> {
        Ok(())
    }
    fn encoder_pop_debug_group(&self, enc_id: i32) {}

    fn compute_pass_set_pipeline(&self, pass_id: i32, pl_id: i32) {}
    fn compute_pass_set_bind_group(
        &self,
        mem: &SharedMem,
        pass_id: i32,
        idx: u32,
        bg_id: i32,
        dyn_cnt: u32,
        dyn_ptr: i32,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
    fn compute_pass_dispatch(&self, pass_id: i32, x: u32, y: u32, z: u32) {}
    fn compute_pass_push_debug_group(
        &self,
        mem: &SharedMem,
        pass_id: i32,
        lbl_ptr: i32,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
    fn compute_pass_pop_debug_group(&self, pass_id: i32) {}
    fn compute_pass_end(&self, pass_id: i32) {}

    fn render_pass_set_pipeline(&self, pass_id: i32, pl_id: i32) {}
    fn render_pass_set_bind_group(
        &self,
        mem: &SharedMem,
        pass_id: i32,
        idx: u32,
        bg_id: i32,
        dyn_cnt: u32,
        dyn_ptr: i32,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
    fn render_pass_set_vertex_buffer(
        &self,
        pass_id: i32,
        slot: u32,
        buf_id: i32,
        off: u64,
        size: i64,
    ) {
    }
    fn render_pass_set_index_buffer(
        &self,
        pass_id: i32,
        buf_id: i32,
        fmt: i32,
        off: u64,
        size: i64,
    ) {
    }
    fn render_pass_draw(&self, pass_id: i32, vc: u32, ic: u32, fv: u32, fi: u32) {}
    fn render_pass_draw_indexed(
        &self,
        pass_id: i32,
        ic: u32,
        inc: u32,
        fi: u32,
        bv: i32,
        fin: u32,
    ) {
    }
    fn render_pass_set_viewport(
        &self,
        pass_id: i32,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        min_d: f32,
        max_d: f32,
    ) {
    }
    fn render_pass_set_scissor_rect(&self, pass_id: i32, x: u32, y: u32, w: u32, h: u32) {}
    fn render_pass_set_stencil_reference(&self, pass_id: i32, ref_val: u32) {}
    fn render_pass_push_debug_group(
        &self,
        mem: &SharedMem,
        pass_id: i32,
        lbl_ptr: i32,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
    fn render_pass_pop_debug_group(&self, pass_id: i32) {}
    fn render_pass_end(&self, pass_id: i32) {}

    fn queue_submit(
        &self,
        mem: &SharedMem,
        queue_id: i32,
        count: u32,
        cmds_ptr: i32,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
    fn queue_write_buffer(
        &self,
        mem: &SharedMem,
        queue_id: i32,
        buf_id: i32,
        offset: u64,
        data_ptr: i32,
        size: u32,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
    fn queue_write_texture(
        &self,
        mem: &SharedMem,
        queue_id: i32,
        dst_ptr: i32,
        data_ptr: i32,
        size: u32,
        layout_ptr: i32,
        ext_ptr: i32,
    ) -> wasmtime::Result<()> {
        Ok(())
    }
    fn present_browser_frame(&self, dev_id: i32, tex_id: i32, cmd_buf_id: i32) {}
    fn resize_surface(&self, width: u32, height: u32) {}
    fn remove_object(&self, id: i32);
}

pub struct HeadlessWebGpu {
    device_queue: Mutex<Option<(i32, i32)>>,
    buffers: Mutex<HashMap<i32, WgpuBufferMeta>>,
    textures: Mutex<HashMap<i32, WgpuTextureMeta>>,
}

impl HeadlessWebGpu {
    pub fn new() -> Self {
        Self {
            device_queue: Mutex::new(None),
            buffers: Mutex::new(HashMap::new()),
            textures: Mutex::new(HashMap::new()),
        }
    }
}

impl WebGpuBackend for HeadlessWebGpu {
    fn get_or_init_device(&self) -> Option<(i32, i32)> {
        *self.device_queue.lock().unwrap()
    }

    fn set_device(&self, device_id: i32, queue_id: i32) {
        *self.device_queue.lock().unwrap() = Some((device_id, queue_id));
    }

    fn has_feature(&self, feature: i32) -> i32 {
        i32::from(matches!(feature, 1 | 4 | 5 | 12 | 14))
    }

    fn fill_limits(&self, mem: &SharedMem, r: i32) -> wasmtime::Result<()> {
        fill_default_wgpu_limits(mem, r)
    }

    fn create_buffer(
        &self,
        _mem: &SharedMem,
        id: i32,
        _desc_ptr: i32,
        size: u64,
        usage: u32,
        _mapped: bool,
        mapped_ptr: i32,
    ) -> wasmtime::Result<bool> {
        self.buffers.lock().unwrap().insert(
            id,
            WgpuBufferMeta {
                size,
                usage,
                mapped_ptr,
            },
        );
        Ok(true)
    }

    fn get_buffer(&self, id: i32) -> Option<WgpuBufferMeta> {
        self.buffers.lock().unwrap().get(&id).cloned()
    }

    fn set_buffer_mapped_ptr(&self, id: i32, ptr: i32) {
        if let Some(b) = self.buffers.lock().unwrap().get_mut(&id) {
            b.mapped_ptr = ptr;
        }
    }

    fn unmap_buffer(&self, _mem: &SharedMem, id: i32) -> wasmtime::Result<i32> {
        if let Some(b) = self.buffers.lock().unwrap().get_mut(&id) {
            let p = b.mapped_ptr;
            b.mapped_ptr = 0;
            Ok(p)
        } else {
            Ok(0)
        }
    }

    fn create_texture(&self, mem: &SharedMem, id: i32, desc_ptr: i32) -> wasmtime::Result<()> {
        let usage = mem.read_u32(desc_ptr + 16)? as u64;
        let dimension = mem.read_i32(desc_ptr + 24)?;
        let depth_or_layers = mem.read_i32(desc_ptr + 36)?;
        self.textures.lock().unwrap().insert(
            id,
            WgpuTextureMeta {
                depth_or_layers,
                dimension,
                usage,
            },
        );
        Ok(())
    }

    fn get_texture(&self, id: i32) -> Option<WgpuTextureMeta> {
        self.textures.lock().unwrap().get(&id).cloned()
    }

    fn remove_object(&self, id: i32) {
        self.buffers.lock().unwrap().remove(&id);
        self.textures.lock().unwrap().remove(&id);
    }
}

pub fn fill_default_wgpu_limits(mem: &SharedMem, r: i32) -> wasmtime::Result<()> {
    mem.write_u32(r + 4, 8192)?;
    mem.write_u32(r + 8, 8192)?;
    mem.write_u32(r + 12, 2048)?;
    mem.write_u32(r + 16, 256)?;
    mem.write_u32(r + 20, 4)?;
    mem.write_u32(r + 24, 24)?;
    mem.write_u32(r + 28, 1000)?;
    mem.write_u32(r + 32, 8)?;
    mem.write_u32(r + 36, 4)?;
    mem.write_u32(r + 40, 16)?;
    mem.write_u32(r + 44, 16)?;
    mem.write_u32(r + 48, 8)?;
    mem.write_u32(r + 52, 4)?;
    mem.write_u32(r + 56, 12)?;
    mem.write_u64(r + 64, 65536)?;
    mem.write_u64(r + 72, 134_217_728)?;
    mem.write_u32(r + 80, 256)?;
    mem.write_u32(r + 84, 256)?;
    mem.write_u32(r + 88, 8)?;
    mem.write_u64(r + 96, 268_435_456)?;
    mem.write_u32(r + 104, 16)?;
    mem.write_u32(r + 108, 2048)?;
    mem.write_u32(r + 112, 16)?;
    mem.write_u32(r + 116, 8)?;
    mem.write_u32(r + 120, 32)?;
    mem.write_u32(r + 124, 16384)?;
    mem.write_u32(r + 128, 256)?;
    mem.write_u32(r + 132, 256)?;
    mem.write_u32(r + 136, 256)?;
    mem.write_u32(r + 140, 64)?;
    mem.write_u32(r + 144, 65535)?;
    mem.write_u32(r + 148, 0)?;

    let next = mem.read_i32(r)?;
    if next != 0 {
        mem.write_u32(next + 8, 8)?;
        mem.write_u32(next + 12, 4)?;
        mem.write_u32(next + 16, 8)?;
        mem.write_u32(next + 20, 4)?;
    }
    Ok(())
}
