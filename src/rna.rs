use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use base64::Engine as _;
use tokio::sync::{Notify, mpsc as tokio_mpsc, watch};
use wasmtime::{AsContextMut, Instance, Result};
use wtransport::config::{IpBindConfig, QuicTransportConfig};
use wtransport::endpoint::ConnectOptions;
use wtransport::error::{ConnectionError, StreamReadError};
use wtransport::tls::Sha256Digest;
use wtransport::{ClientConfig, Endpoint, VarInt};

use crate::backends::{RnaBackend, RnaConnectParams};
use crate::embind::{EmbindArg, call_function, has_function};
use crate::runtime::{RuntimeState, StoreState};

#[derive(Debug, Clone)]
pub enum RnaCallbackEvent {
    Handshake {
        id: u32,
        is_completed: bool,
        has_error: bool,
        error_name: String,
        error_source: String,
        stream_error_code: i32,
        close_code: i32,
    },
    ConnectSuccess {
        id: u32,
        max_datagram_size: u32,
    },
    ConnectError {
        id: u32,
    },
    CloseComplete {
        id: u32,
    },
    ConnectionClosed {
        id: u32,
        close_code: u32,
        serialized_reason_class: u32,
        has_serialized_state_reason: bool,
    },
    StreamAccepted {
        id: u32,
        stream_id: i32,
    },
    ReceiveRingSlot {
        id: u32,
        receive_kind: i32,
        stream_id: i32,
        slot_index: u32,
        data_len: u32,
    },
    StreamFin {
        id: u32,
        stream_id: i32,
    },
    StreamError {
        id: u32,
        stream_id: i32,
        stream_error_code: u32,
    },
    StreamWriteSuccess {
        id: u32,
        stream_id: i32,
        bytes_written: u32,
    },
    StreamWriteError {
        id: u32,
        stream_id: i32,
    },
    DatagramAcked {
        id: u32,
        datagram_hi: i32,
        datagram_lo: i32,
    },
    DatagramLost {
        id: u32,
        datagram_hi: i32,
        datagram_lo: i32,
    },
    ConnectionStats {
        id: u32,
        values: [f64; 16],
    },
}

enum OutStreamCmd {
    Write(Vec<u8>),
    ShutdownWrite,
}

struct IncomingStreamCtrl {
    paused: AtomicBool,
    cancelled: AtomicBool,
    awaiting_consumption: AtomicBool,
    notify: Notify,
}

impl IncomingStreamCtrl {
    fn new() -> Self {
        Self {
            paused: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            awaiting_consumption: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }
}

struct ActiveRnaConn {
    conn_id: u32,
    owner_thread: i32,
    token: u64,
    closed: AtomicBool,
    handshake_reported: AtomicBool,
    report_handshake: bool,
    use_stream_err_code: bool,
    fix_conn_ownership: bool,
    stats_interval_ms: u32,

    rx_data_ptr: i32,
    rx_head_ptr: i32,
    rx_tail_ptr: i32,
    rx_slot_size: u32,
    rx_slot_count: u32,

    ring_write_lock: Mutex<()>,
    dropped_datagrams: AtomicU64,
    connection: Mutex<Option<wtransport::Connection>>,
    conn_ready_tx: watch::Sender<Option<wtransport::Connection>>,
    outgoing_streams: Mutex<HashMap<i32, tokio_mpsc::UnboundedSender<OutStreamCmd>>>,
    incoming_streams: Mutex<HashMap<i32, Arc<IncomingStreamCtrl>>>,
    close_notify: Notify,
}

impl ActiveRnaConn {
    fn is_current(&self, map: &Mutex<HashMap<u32, Arc<ActiveRnaConn>>>) -> bool {
        map.lock()
            .unwrap()
            .get(&self.conn_id)
            .is_some_and(|c| c.token == self.token)
    }

    fn is_active(&self, map: &Mutex<HashMap<u32, Arc<ActiveRnaConn>>>) -> bool {
        !self.closed.load(Ordering::SeqCst)
            && (!self.fix_conn_ownership || self.is_current(map))
    }

    fn emit_handshake_once(
        &self,
        push: &dyn Fn(RnaCallbackEvent),
        is_completed: bool,
        has_error: bool,
        error_name: &str,
        error_source: &str,
        stream_error_code: i32,
        close_code: i32,
    ) {
        if !self.report_handshake {
            return;
        }
        if self
            .handshake_reported
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            push(RnaCallbackEvent::Handshake {
                id: self.conn_id,
                is_completed,
                has_error,
                error_name: error_name.chars().take(64).collect(),
                error_source: error_source.chars().take(64).collect(),
                stream_error_code,
                close_code,
            });
        }
    }

    fn try_write_ring_slot(
        &self,
        rt: &RuntimeState,
        conns_map: &Mutex<HashMap<u32, Arc<ActiveRnaConn>>>,
        receive_kind: i32,
        stream_id: i32,
        data: &[u8],
        stream_ctrl: Option<&IncomingStreamCtrl>,
    ) -> usize {
        if self.fix_conn_ownership && !self.is_active(conns_map) {
            return 0;
        }
        if self.rx_slot_count == 0 || self.rx_slot_size == 0 || data.is_empty() {
            return 0;
        }
        let _ring_guard = self.ring_write_lock.lock().unwrap();
        let Ok(head_i32) = rt.memory.atomic_load_i32(self.rx_head_ptr) else {
            return 0;
        };
        let Ok(tail_i32) = rt.memory.atomic_load_i32(self.rx_tail_ptr) else {
            return 0;
        };
        let head = head_i32 as u32;
        let tail = tail_i32 as u32;
        if head.wrapping_sub(tail) >= self.rx_slot_count {
            return 0;
        }
        let write_len = data.len().min(self.rx_slot_size as usize);
        let slot_index = head % self.rx_slot_count;
        let dst = self
            .rx_data_ptr
            .wrapping_add((slot_index * self.rx_slot_size) as i32);
        if rt.memory.write_bytes(dst, &data[..write_len]).is_err() {
            return 0;
        }
        if let Some(ctrl) = stream_ctrl {
            ctrl.awaiting_consumption.store(true, Ordering::SeqCst);
        }
        if rt
            .memory
            .atomic_store_i32(self.rx_head_ptr, head.wrapping_add(1) as i32)
            .is_err()
        {
            if let Some(ctrl) = stream_ctrl {
                ctrl.awaiting_consumption.store(false, Ordering::SeqCst);
            }
            return 0;
        }
        drop(_ring_guard);
        rt.push_rna_event(
            self.owner_thread,
            RnaCallbackEvent::ReceiveRingSlot {
                id: self.conn_id,
                receive_kind,
                stream_id,
                slot_index,
                data_len: write_len as u32,
            },
        );
        write_len
    }
}

pub struct WTransportRnaBackend {
    tokio_rt: OnceLock<tokio::runtime::Runtime>,
    runtime_ref: Mutex<Option<Weak<RuntimeState>>>,
    conns: Arc<Mutex<HashMap<u32, Arc<ActiveRnaConn>>>>,
    next_token: AtomicU64,
}

impl WTransportRnaBackend {
    pub fn new() -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        Self {
            tokio_rt: OnceLock::new(),
            runtime_ref: Mutex::new(None),
            conns: Arc::new(Mutex::new(HashMap::new())),
            next_token: AtomicU64::new(1),
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
                .expect("failed to build tokio runtime for WebTransport RNA")
        })
    }

    fn upgrade_rt(&self) -> Option<Arc<RuntimeState>> {
        self.runtime_ref
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
    }

    fn push_rna(&self, owner_thread: i32, ev: RnaCallbackEvent) {
        if let Some(rt) = self.upgrade_rt() {
            rt.push_rna_event(owner_thread, ev);
        }
    }
}

impl RnaBackend for WTransportRnaBackend {
    fn is_supported(&self) -> bool {
        true
    }

    fn connect(&self, params: RnaConnectParams) {
        let conn_id = params.conn_id;
        let owner_thread = params.owner_thread;
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        println!(
            "[WASM-RNA] Connecting id={conn_id} owner={owner_thread:#x} url={} cert_hashes={}",
            params.url,
            params.cert_hashes.len()
        );

        let (conn_ready_tx, _) = watch::channel(None);
        let conn_state = Arc::new(ActiveRnaConn {
            conn_id,
            owner_thread,
            token,
            closed: AtomicBool::new(false),
            handshake_reported: AtomicBool::new(false),
            report_handshake: params.report_handshake,
            use_stream_err_code: params.use_stream_err_code,
            fix_conn_ownership: params.fix_conn_ownership,
            stats_interval_ms: params.stats_interval_ms,
            rx_data_ptr: params.rx_data_ptr,
            rx_head_ptr: params.rx_head_ptr,
            rx_tail_ptr: params.rx_tail_ptr,
            rx_slot_size: params.rx_slot_size,
            rx_slot_count: params.rx_slot_count,
            ring_write_lock: Mutex::new(()),
            dropped_datagrams: AtomicU64::new(0),
            connection: Mutex::new(None),
            conn_ready_tx,
            outgoing_streams: Mutex::new(HashMap::new()),
            incoming_streams: Mutex::new(HashMap::new()),
            close_notify: Notify::new(),
        });

        self.conns.lock().unwrap().insert(conn_id, conn_state.clone());

        let weak_rt = self.runtime_ref.lock().unwrap().clone();
        let origin_hdr = weak_rt
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|rt| rt.config.roblox_base_url.trim_end_matches('/').to_owned())
            .filter(|s| !s.is_empty());
        let conns_map = self.conns.clone();

        self.rt().spawn(async move {
            let push = |ev: RnaCallbackEvent| {
                if let Some(ref w) = weak_rt {
                    if let Some(rt) = w.upgrade() {
                        rt.push_rna_event(owner_thread, ev);
                    }
                }
            };

            let client_cfg = match build_wtransport_config(&params.cert_hashes) {
                Ok(cfg) => cfg,
                Err(err) => {
                    eprintln!("[WASM-RNA] config build failed id={conn_id}: {err}");
                    conn_state.emit_handshake_once(&push, false, true, "WebTransportError", "session", -1, -1);
                    conn_state.closed.store(true, Ordering::SeqCst);
                    remove_if_token(&conns_map, conn_id, token);
                    push(RnaCallbackEvent::ConnectError { id: conn_id });
                    return;
                }
            };

            let endpoint = match Endpoint::client(client_cfg) {
                Ok(ep) => ep,
                Err(err) => {
                    eprintln!("[WASM-RNA] endpoint init failed id={conn_id}: {err}");
                    conn_state.emit_handshake_once(&push, false, true, "WebTransportError", "session", -1, -1);
                    conn_state.closed.store(true, Ordering::SeqCst);
                    remove_if_token(&conns_map, conn_id, token);
                    push(RnaCallbackEvent::ConnectError { id: conn_id });
                    return;
                }
            };

            let connect_opts = {
                let mut b = ConnectOptions::builder(&params.url)
                    .add_header("user-agent", crate::backends::BROWSER_USER_AGENT);
                if let Some(ref orig) = origin_hdr {
                    b = b.add_header("origin", orig);
                }
                b.build()
            };

            let wt_conn = tokio::select! {
                _ = conn_state.close_notify.notified() => {
                    return;
                }
                res = endpoint.connect(connect_opts) => {
                    match res {
                        Ok(c) => c,
                        Err(err) => {
                            if conn_state.is_active(&conns_map) {
                                eprintln!("[WASM-RNA] connect failed id={conn_id}: {err}");
                                conn_state.emit_handshake_once(
                                    &push,
                                    false,
                                    true,
                                    "WebTransportError",
                                    "session",
                                    -1,
                                    -1,
                                );
                                conn_state.closed.store(true, Ordering::SeqCst);
                                conn_state.close_notify.notify_waiters();
                                remove_if_token(&conns_map, conn_id, token);
                                push(RnaCallbackEvent::ConnectError { id: conn_id });
                            }
                            return;
                        }
                    }
                }
            };

            if !conn_state.is_active(&conns_map) {
                wt_conn.close(VarInt::from_u32(0), b"");
                return;
            }

            let max_dgram = wt_conn.max_datagram_size().unwrap_or(1200) as u32;
            *conn_state.connection.lock().unwrap() = Some(wt_conn.clone());
            let _ = conn_state.conn_ready_tx.send_replace(Some(wt_conn.clone()));

            eprintln!(
                "[WASM-RNA] Connected id={conn_id} remote={} max_datagram={max_dgram}",
                wt_conn.remote_address()
            );
            conn_state.emit_handshake_once(&push, true, false, "", "", -1, -1);
            push(RnaCallbackEvent::ConnectSuccess {
                id: conn_id,
                max_datagram_size: max_dgram,
            });

            // 1. Incoming unidirectional streams task
            {
                let cs = conn_state.clone();
                let cm = conns_map.clone();
                let wrt = weak_rt.clone();
                let c_uni = wt_conn.clone();
                tokio::spawn(async move {
                    let mut next_stream_id: i32 = 3;
                    loop {
                        let recv_res = tokio::select! {
                            _ = cs.close_notify.notified() => return,
                            r = c_uni.accept_uni() => r,
                        };
                        match recv_res {
                            Ok(recv_stream) => {
                                if !cs.is_active(&cm) {
                                    return;
                                }
                                let sid = next_stream_id;
                                next_stream_id = next_stream_id.wrapping_add(4);
                                eprintln!(
                                    "[WASM-RNA] Incoming uni stream accepted id={} stream={sid}",
                                    cs.conn_id
                                );
                                let ctrl = Arc::new(IncomingStreamCtrl::new());
                                cs.incoming_streams.lock().unwrap().insert(sid, ctrl.clone());
                                if let Some(ref w) = wrt {
                                    if let Some(rt) = w.upgrade() {
                                        rt.push_rna_event(
                                            cs.owner_thread,
                                            RnaCallbackEvent::StreamAccepted {
                                                id: cs.conn_id,
                                                stream_id: sid,
                                            },
                                        );
                                    }
                                }
                                spawn_incoming_stream_reader(
                                    cs.clone(),
                                    cm.clone(),
                                    wrt.clone(),
                                    sid,
                                    recv_stream,
                                    ctrl,
                                );
                            }
                            Err(_) => return,
                        }
                    }
                });
            }

            // 1b. Incoming bidirectional streams task (diagnostic + safety)
            {
                let cs = conn_state.clone();
                let c_bi = wt_conn.clone();
                tokio::spawn(async move {
                    loop {
                        let bi_res = tokio::select! {
                            _ = cs.close_notify.notified() => return,
                            r = c_bi.accept_bi() => r,
                        };
                        match bi_res {
                            Ok((_send, _recv)) => {
                                println!("[WASM-RNA] Incoming BI stream accepted id={}", cs.conn_id);
                            }
                            Err(_) => return,
                        }
                    }
                });
            }

            // 2. Incoming datagrams task
            {
                let cs = conn_state.clone();
                let cm = conns_map.clone();
                let wrt = weak_rt.clone();
                let c_dg = wt_conn.clone();
                tokio::spawn(async move {
                    let mut dg_rx_count: u64 = 0;
                    loop {
                        let dg_res = tokio::select! {
                            _ = cs.close_notify.notified() => return,
                            r = c_dg.receive_datagram() => r,
                        };
                        match dg_res {
                            Ok(dgram) => {
                                if !cs.is_active(&cm) {
                                    return;
                                }
                                let Some(ref w) = wrt else { return };
                                let Some(rt) = w.upgrade() else { return };
                                let bytes: &[u8] = &dgram;
                                dg_rx_count += 1;
                                if dg_rx_count <= 8 || dg_rx_count % 100 == 0 {
                                    eprintln!(
                                        "[WASM-RNA] RX datagram #{dg_rx_count} id={} len={}",
                                        cs.conn_id,
                                        bytes.len()
                                    );
                                }
                                if bytes.len() > cs.rx_slot_size as usize
                                    || cs.try_write_ring_slot(&rt, &cm, 1, 0, bytes, None) == 0
                                {
                                    cs.dropped_datagrams.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            Err(e) => {
                                eprintln!("[WASM-RNA] receive_datagram ended id={}: {e}", cs.conn_id);
                                return;
                            }
                        }
                    }
                });
            }

            // 3. Connection stats timer task (if requested)
            if conn_state.stats_interval_ms > 0 {
                let cs = conn_state.clone();
                let cm = conns_map.clone();
                let wrt = weak_rt.clone();
                let c_st = wt_conn.clone();
                let interval = Duration::from_millis(conn_state.stats_interval_ms.max(100) as u64);
                tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(interval);
                    loop {
                        tokio::select! {
                            _ = cs.close_notify.notified() => return,
                            _ = ticker.tick() => {
                                if !cs.is_active(&cm) {
                                    return;
                                }
                                let rtt_ms = c_st.rtt().as_secs_f64() * 1000.0;
                                let dropped = cs.dropped_datagrams.load(Ordering::Relaxed) as f64;
                                let values = [
                                    rtt_ms, -1.0, -1.0, -1.0,
                                    -1.0, -1.0, -1.0, -1.0,
                                    -1.0, -1.0, -1.0, dropped,
                                    -1.0, -1.0, -1.0, -1.0,
                                ];
                                if let Some(ref w) = wrt {
                                    if let Some(rt) = w.upgrade() {
                                        rt.push_rna_event(
                                            cs.owner_thread,
                                            RnaCallbackEvent::ConnectionStats {
                                                id: cs.conn_id,
                                                values,
                                            },
                                        );
                                    }
                                }
                            }
                        }
                    }
                });
            }

            // 4. Wait for connection closure (keep `endpoint` alive for lifetime of connection)
            let close_err = tokio::select! {
                _ = conn_state.close_notify.notified() => {
                    let _ = &endpoint;
                    return;
                }
                e = wt_conn.closed() => e,
            };
            let _ = &endpoint;

            if conn_state.fix_conn_ownership && !conn_state.is_current(&conns_map) {
                return;
            }
            if !conn_state.closed.swap(true, Ordering::SeqCst) {
                match close_err {
                    ConnectionError::ApplicationClosed(app_close) => {
                        let code = app_close.code().into_inner() as u32;
                        let parsed = parse_serialized_reason(app_close.reason());
                        push(RnaCallbackEvent::ConnectionClosed {
                            id: conn_id,
                            close_code: code,
                            serialized_reason_class: parsed.unwrap_or(0),
                            has_serialized_state_reason: parsed.is_some(),
                        });
                    }
                    ConnectionError::LocallyClosed => {
                        push(RnaCallbackEvent::ConnectionClosed {
                            id: conn_id,
                            close_code: 0,
                            serialized_reason_class: 0,
                            has_serialized_state_reason: false,
                        });
                    }
                    other => {
                        eprintln!("[WASM-RNA] connection closed with error id={conn_id}: {other}");
                        push(RnaCallbackEvent::ConnectionClosed {
                            id: conn_id,
                            close_code: 0,
                            serialized_reason_class: 0,
                            has_serialized_state_reason: false,
                        });
                    }
                }
            }
            remove_if_token(&conns_map, conn_id, token);
        });
    }

    fn open_stream(&self, conn_id: u32, stream_id: i32) {
        let Some(cs) = self.conns.lock().unwrap().get(&conn_id).cloned() else {
            return;
        };
        if !cs.is_active(&self.conns) {
            return;
        }

        let (tx, mut rx) = tokio_mpsc::unbounded_channel::<OutStreamCmd>();
        cs.outgoing_streams.lock().unwrap().insert(stream_id, tx);

        let cm = self.conns.clone();
        let wrt = self.runtime_ref.lock().unwrap().clone();
        let owner_thread = cs.owner_thread;
        let mut conn_rx = cs.conn_ready_tx.subscribe();

        self.rt().spawn(async move {
            let push = |ev: RnaCallbackEvent| {
                if let Some(ref w) = wrt {
                    if let Some(rt) = w.upgrade() {
                        rt.push_rna_event(owner_thread, ev);
                    }
                }
            };

            let wt_conn = loop {
                if let Some(c) = conn_rx.borrow_and_update().clone() {
                    break c;
                }
                if !cs.is_active(&cm) {
                    return;
                }
                tokio::select! {
                    _ = cs.close_notify.notified() => return,
                    res = conn_rx.changed() => {
                        if res.is_err() {
                            return;
                        }
                    }
                }
            };

            let send_stream_res = match wt_conn.open_uni().await {
                Ok(opening) => opening.await.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };

            let mut send_stream = match send_stream_res {
                Ok(s) => {
                    eprintln!("[WASM-RNA] Outgoing uni stream opened id={conn_id} stream={stream_id}");
                    s
                }
                Err(err) => {
                    if cs.is_active(&cm) {
                        eprintln!("[WASM-RNA] open_uni failed id={conn_id} stream={stream_id}: {err}");
                        push(RnaCallbackEvent::StreamWriteError {
                            id: conn_id,
                            stream_id,
                        });
                    }
                    return;
                }
            };

            let mut tx_chunks: u64 = 0;
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    OutStreamCmd::Write(bytes) => {
                        if !cs.is_active(&cm) {
                            if !cs.fix_conn_ownership {
                                push(RnaCallbackEvent::StreamWriteError {
                                    id: conn_id,
                                    stream_id,
                                });
                            }
                            break;
                        }
                        let len = bytes.len() as u32;
                        tx_chunks += 1;
                        if tx_chunks <= 4 || tx_chunks % 200 == 0 {
                            eprintln!(
                                "[WASM-RNA] TX stream write #{tx_chunks} id={conn_id} stream={stream_id} len={len}"
                            );
                        }
                        match send_stream.write_all(&bytes).await {
                            Ok(()) => {
                                if !cs.fix_conn_ownership || cs.is_active(&cm) {
                                    push(RnaCallbackEvent::StreamWriteSuccess {
                                        id: conn_id,
                                        stream_id,
                                        bytes_written: len,
                                    });
                                }
                            }
                            Err(err) => {
                                eprintln!(
                                    "[WASM-RNA] stream write failed id={conn_id} stream={stream_id}: {err}"
                                );
                                if cs.is_active(&cm) {
                                    push(RnaCallbackEvent::StreamWriteError {
                                        id: conn_id,
                                        stream_id,
                                    });
                                }
                                break;
                            }
                        }
                    }
                    OutStreamCmd::ShutdownWrite => {
                        let _ = send_stream.finish().await;
                        break;
                    }
                }
            }
        });
    }

    fn write_stream(&self, conn_id: u32, stream_id: i32, data: &[u8]) {
        let Some(cs) = self.conns.lock().unwrap().get(&conn_id).cloned() else {
            self.push_rna(
                0,
                RnaCallbackEvent::StreamWriteError {
                    id: conn_id,
                    stream_id,
                },
            );
            return;
        };
        if !cs.is_active(&self.conns) {
            self.push_rna(
                cs.owner_thread,
                RnaCallbackEvent::StreamWriteError {
                    id: conn_id,
                    stream_id,
                },
            );
            return;
        }
        let sent = cs
            .outgoing_streams
            .lock()
            .unwrap()
            .get(&stream_id)
            .is_some_and(|tx| tx.send(OutStreamCmd::Write(data.to_vec())).is_ok());
        if !sent {
            self.push_rna(
                cs.owner_thread,
                RnaCallbackEvent::StreamWriteError {
                    id: conn_id,
                    stream_id,
                },
            );
        }
    }

    fn shutdown_stream_write(&self, conn_id: u32, stream_id: i32) {
        let cs = self.conns.lock().unwrap().get(&conn_id).cloned();
        if let Some(cs) = cs {
            if let Some(tx) = cs.outgoing_streams.lock().unwrap().remove(&stream_id) {
                let _ = tx.send(OutStreamCmd::ShutdownWrite);
            }
        }
    }

    fn shutdown_stream_read(&self, conn_id: u32, stream_id: i32) {
        let cs = self.conns.lock().unwrap().get(&conn_id).cloned();
        if let Some(cs) = cs {
            if let Some(ctrl) = cs.incoming_streams.lock().unwrap().remove(&stream_id) {
                ctrl.cancelled.store(true, Ordering::SeqCst);
                ctrl.paused.store(true, Ordering::SeqCst);
                ctrl.notify.notify_waiters();
            }
        }
    }

    fn pause_stream_read(&self, conn_id: u32, stream_id: i32) {
        let cs = self.conns.lock().unwrap().get(&conn_id).cloned();
        if let Some(cs) = cs {
            if let Some(ctrl) = cs.incoming_streams.lock().unwrap().get(&stream_id) {
                ctrl.paused.store(true, Ordering::SeqCst);
            }
        }
    }

    fn resume_stream_read(&self, conn_id: u32, stream_id: i32) {
        let cs = self.conns.lock().unwrap().get(&conn_id).cloned();
        if let Some(cs) = cs {
            if !cs.is_active(&self.conns) {
                return;
            }
            if let Some(ctrl) = cs.incoming_streams.lock().unwrap().get(&stream_id) {
                if !ctrl.cancelled.load(Ordering::SeqCst) {
                    ctrl.paused.store(false, Ordering::SeqCst);
                    ctrl.notify.notify_one();
                }
            }
        }
    }

    fn receive_ring_consumed(&self, conn_id: u32, kind: i32, stream_id: i32) {
        let Some(cs) = self.conns.lock().unwrap().get(&conn_id).cloned() else {
            return;
        };
        if cs.closed.load(Ordering::SeqCst) {
            return;
        }
        let streams: Vec<(i32, Arc<IncomingStreamCtrl>)> = cs
            .incoming_streams
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();

        for (sid, ctrl) in streams {
            if kind == 0 && sid == stream_id {
                ctrl.awaiting_consumption.store(false, Ordering::SeqCst);
            }
            ctrl.notify.notify_one();
        }
    }

    fn send_datagram(&self, conn_id: u32, data: &[u8], hi: i32, lo: i32) {
        let cs = self.conns.lock().unwrap().get(&conn_id).cloned();
        let Some(cs) = cs else {
            self.push_rna(
                0,
                RnaCallbackEvent::DatagramLost {
                    id: conn_id,
                    datagram_hi: hi,
                    datagram_lo: lo,
                },
            );
            return;
        };
        if cs.closed.load(Ordering::SeqCst) {
            if !cs.fix_conn_ownership || cs.is_active(&self.conns) {
                self.push_rna(
                    cs.owner_thread,
                    RnaCallbackEvent::DatagramLost {
                        id: conn_id,
                        datagram_hi: hi,
                        datagram_lo: lo,
                    },
                );
            }
            return;
        }
        let wt_conn = cs.connection.lock().unwrap().clone();
        let ok = wt_conn
            .as_ref()
            .is_some_and(|c| c.send_datagram(data).is_ok());

        if !cs.fix_conn_ownership || cs.is_active(&self.conns) {
            if (hi == 0 && lo < 8) || (lo % 100 == 0) {
                println!(
                    "[WASM-RNA] TX datagram id={conn_id} seq=({hi},{lo}) len={} ok={ok}",
                    data.len()
                );
            }
            if ok {
                self.push_rna(
                    cs.owner_thread,
                    RnaCallbackEvent::DatagramAcked {
                        id: conn_id,
                        datagram_hi: hi,
                        datagram_lo: lo,
                    },
                );
            } else {
                self.push_rna(
                    cs.owner_thread,
                    RnaCallbackEvent::DatagramLost {
                        id: conn_id,
                        datagram_hi: hi,
                        datagram_lo: lo,
                    },
                );
            }
        }
    }

    fn send_connection_close(&self, conn_id: u32) {
        if let Some(cs) = self.conns.lock().unwrap().get(&conn_id).cloned() {
            let owner = cs.owner_thread;
            let push = |ev: RnaCallbackEvent| self.push_rna(owner, ev);
            cs.emit_handshake_once(&push, false, false, "", "", -1, -1);
            cs.closed.store(true, Ordering::SeqCst);
            if let Some(wt) = cs.connection.lock().unwrap().as_ref() {
                wt.close(VarInt::from_u32(0), b"");
            }
            cs.close_notify.notify_waiters();
        }
    }

    fn close(&self, conn_id: u32) {
        let mut owner = 0;
        if let Some(cs) = self.conns.lock().unwrap().remove(&conn_id) {
            owner = cs.owner_thread;
            let push = |ev: RnaCallbackEvent| self.push_rna(owner, ev);
            cs.emit_handshake_once(&push, false, false, "", "", -1, -1);
            cs.closed.store(true, Ordering::SeqCst);
            if let Some(wt) = cs.connection.lock().unwrap().as_ref() {
                wt.close(VarInt::from_u32(0), b"");
            }
            cs.close_notify.notify_waiters();
        }
        self.push_rna(owner, RnaCallbackEvent::CloseComplete { id: conn_id });
    }
}

fn spawn_incoming_stream_reader(
    cs: Arc<ActiveRnaConn>,
    cm: Arc<Mutex<HashMap<u32, Arc<ActiveRnaConn>>>>,
    wrt: Option<Weak<RuntimeState>>,
    stream_id: i32,
    mut recv_stream: wtransport::RecvStream,
    ctrl: Arc<IncomingStreamCtrl>,
) {
    tokio::spawn(async move {
        let buf_cap = (cs.rx_slot_size as usize).clamp(4096, 65536);
        let mut read_buf = vec![0u8; buf_cap];
        let mut pending: Vec<u8> = Vec::new();
        let mut pending_off: usize = 0;

        loop {
            if ctrl.cancelled.load(Ordering::SeqCst) || !cs.is_active(&cm) {
                recv_stream.stop(VarInt::from_u32(0));
                return;
            }

            // Register interest on `ctrl.notify` BEFORE checking state flags or writing ring slots
            // so a concurrent `resume_stream_read` / `receive_ring_consumed` can never be missed.
            let notified = ctrl.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if ctrl.paused.load(Ordering::SeqCst)
                || ctrl.awaiting_consumption.load(Ordering::SeqCst)
            {
                tokio::select! {
                    _ = cs.close_notify.notified() => return,
                    _ = &mut notified => continue,
                }
            }

            if pending_off < pending.len() {
                let Some(ref w) = wrt else { return };
                let Some(rt) = w.upgrade() else { return };
                let written = cs.try_write_ring_slot(
                    &rt,
                    &cm,
                    0,
                    stream_id,
                    &pending[pending_off..],
                    Some(&ctrl),
                );
                if written == 0 {
                    tokio::select! {
                        _ = cs.close_notify.notified() => return,
                        _ = &mut notified => continue,
                    }
                }
                pending_off += written;
                if pending_off >= pending.len() {
                    pending.clear();
                    pending_off = 0;
                }
                continue;
            }

            let read_res = tokio::select! {
                _ = cs.close_notify.notified() => return,
                r = recv_stream.read(&mut read_buf) => r,
            };

            if ctrl.cancelled.load(Ordering::SeqCst) || !cs.is_active(&cm) {
                return;
            }

            match read_res {
                Ok(None) => {
                    if let Some(ref w) = wrt {
                        if let Some(rt) = w.upgrade() {
                            rt.push_rna_event(
                                cs.owner_thread,
                                RnaCallbackEvent::StreamFin {
                                    id: cs.conn_id,
                                    stream_id,
                                },
                            );
                        }
                    }
                    return;
                }
                Ok(Some(0)) => continue,
                Ok(Some(n)) => {
                    pending.clear();
                    pending.extend_from_slice(&read_buf[..n]);
                    pending_off = 0;
                }
                Err(err) => {
                    let err_code = match err {
                        StreamReadError::Reset(code) if cs.use_stream_err_code => {
                            let c = code.into_inner();
                            if c > 0 && c <= u32::MAX as u64 {
                                c as u32
                            } else {
                                0
                            }
                        }
                        StreamReadError::NotConnected if cs.use_stream_err_code => {
                            return;
                        }
                        _ => 0,
                    };
                    if let Some(ref w) = wrt {
                        if let Some(rt) = w.upgrade() {
                            rt.push_rna_event(
                                cs.owner_thread,
                                RnaCallbackEvent::StreamError {
                                    id: cs.conn_id,
                                    stream_id,
                                    stream_error_code: err_code,
                                },
                            );
                        }
                    }
                    return;
                }
            }
        }
    });
}

fn remove_if_token(
    map: &Mutex<HashMap<u32, Arc<ActiveRnaConn>>>,
    conn_id: u32,
    token: u64,
) {
    let mut guard = map.lock().unwrap();
    if guard.get(&conn_id).is_some_and(|c| c.token == token) {
        guard.remove(&conn_id);
    }
}

fn build_wtransport_config(cert_hashes_b64: &[String]) -> std::result::Result<ClientConfig, String> {
    let builder = ClientConfig::builder().with_bind_config(IpBindConfig::InAddrAnyV4);
    let mut cfg = if cert_hashes_b64.is_empty() {
        builder
            .with_native_certs()
            .keep_alive_interval(Some(Duration::from_secs(5)))
            .build()
    } else {
        let mut digests = Vec::with_capacity(cert_hashes_b64.len());
        for b64 in cert_hashes_b64 {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .map_err(|e| format!("invalid base64 serverCertificateHash {b64:?}: {e}"))?;
            let arr: [u8; 32] = decoded
                .try_into()
                .map_err(|_| format!("serverCertificateHash {b64:?} is not 32 bytes"))?;
            digests.push(Sha256Digest::new(arr));
        }
        builder
            .with_server_certificate_hashes(digests)
            .keep_alive_interval(Some(Duration::from_secs(5)))
            .build()
    };

    let mut transport = QuicTransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(5)));
    transport.enable_segmentation_offload(false);
    transport.datagram_receive_buffer_size(Some(4 * 1024 * 1024));
    transport.datagram_send_buffer_size(4 * 1024 * 1024);
    cfg.quic_config_mut().transport_config(Arc::new(transport));

    Ok(cfg)
}

fn parse_serialized_reason(reason: &[u8]) -> Option<u32> {
    let s = std::str::from_utf8(reason).ok()?;
    if let Some(hex) = s.strip_prefix("RBX1:") {
        if hex.len() == 8 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return u32::from_str_radix(hex, 16).ok();
        }
        return Some(0);
    }
    if reason.len() < 8 {
        return None;
    }
    if reason[..4].iter().any(|&b| b > 127) {
        return Some(0);
    }
    Some(u32::from_be_bytes([
        reason[0], reason[1], reason[2], reason[3],
    ]))
}

fn call_rna_embind(
    mut store: impl AsContextMut<Data = StoreState>,
    inst: Instance,
    rt: &RuntimeState,
    name: &str,
    args: &[EmbindArg],
) -> Result<()> {
    if !has_function(rt, name) {
        eprintln!("[WASM-RNA] missing Embind export {name}");
        return Ok(());
    }
    if let Err(err) = call_function(&mut store, inst, rt, name, args) {
        eprintln!("[WASM-RNA] Embind {name} error: {err:?}");
        return Err(err);
    }
    Ok(())
}

pub fn dispatch_rna_event(
    mut store: impl AsContextMut<Data = StoreState>,
    inst: Instance,
    rt: &RuntimeState,
    ev: RnaCallbackEvent,
) -> Result<()> {
    match ev {
        RnaCallbackEvent::Handshake {
            id,
            is_completed,
            has_error,
            error_name,
            error_source,
            stream_error_code,
            close_code,
        } => {
            let _ = call_rna_embind(
                &mut store,
                inst,
                rt,
                "rbx_wasm_rna_on_handshake",
                &[
                    EmbindArg::UInt(id),
                    EmbindArg::Bool(is_completed),
                    EmbindArg::Bool(has_error),
                    EmbindArg::String(error_name),
                    EmbindArg::String(error_source),
                    EmbindArg::Int(stream_error_code),
                    EmbindArg::Int(close_code),
                ],
            );
        }
        RnaCallbackEvent::ConnectSuccess {
            id,
            max_datagram_size,
        } => {
            let _ = call_rna_embind(
                &mut store,
                inst,
                rt,
                "rbx_wasm_rna_on_connect_success",
                &[EmbindArg::UInt(id), EmbindArg::UInt(max_datagram_size)],
            );
        }
        RnaCallbackEvent::ConnectError { id } => {
            let _ = call_rna_embind(
                &mut store,
                inst,
                rt,
                "rbx_wasm_rna_on_connect_error",
                &[EmbindArg::UInt(id)],
            );
        }
        RnaCallbackEvent::CloseComplete { id } => {
            let _ = call_rna_embind(
                &mut store,
                inst,
                rt,
                "rbx_wasm_rna_on_close_complete",
                &[EmbindArg::UInt(id)],
            );
        }
        RnaCallbackEvent::ConnectionClosed {
            id,
            close_code,
            serialized_reason_class,
            has_serialized_state_reason,
        } => {
            let _ = call_rna_embind(
                &mut store,
                inst,
                rt,
                "rbx_wasm_rna_on_connection_closed",
                &[
                    EmbindArg::UInt(id),
                    EmbindArg::UInt(close_code),
                    EmbindArg::UInt(serialized_reason_class),
                    EmbindArg::Bool(has_serialized_state_reason),
                ],
            );
        }
        RnaCallbackEvent::StreamAccepted { id, stream_id } => {
            let _ = call_rna_embind(
                &mut store,
                inst,
                rt,
                "rbx_wasm_rna_on_stream_accepted",
                &[EmbindArg::UInt(id), EmbindArg::Int(stream_id)],
            );
        }
        RnaCallbackEvent::ReceiveRingSlot {
            id,
            receive_kind,
            stream_id,
            slot_index,
            data_len,
        } => {
            let res = call_rna_embind(
                &mut store,
                inst,
                rt,
                "rbx_wasm_rna_on_receive_ring_slot",
                &[
                    EmbindArg::UInt(id),
                    EmbindArg::Int(receive_kind),
                    EmbindArg::Int(stream_id),
                    EmbindArg::UInt(slot_index),
                    EmbindArg::UInt(data_len),
                ],
            );
            if res.is_err() && receive_kind == 0 {
                let _ = call_rna_embind(
                    &mut store,
                    inst,
                    rt,
                    "rbx_wasm_rna_on_stream_error",
                    &[
                        EmbindArg::UInt(id),
                        EmbindArg::Int(stream_id),
                        EmbindArg::UInt(0),
                    ],
                );
            }
        }
        RnaCallbackEvent::StreamFin { id, stream_id } => {
            let _ = call_rna_embind(
                &mut store,
                inst,
                rt,
                "rbx_wasm_rna_on_stream_fin",
                &[EmbindArg::UInt(id), EmbindArg::Int(stream_id)],
            );
        }
        RnaCallbackEvent::StreamError {
            id,
            stream_id,
            stream_error_code,
        } => {
            let _ = call_rna_embind(
                &mut store,
                inst,
                rt,
                "rbx_wasm_rna_on_stream_error",
                &[
                    EmbindArg::UInt(id),
                    EmbindArg::Int(stream_id),
                    EmbindArg::UInt(stream_error_code),
                ],
            );
        }
        RnaCallbackEvent::StreamWriteSuccess {
            id,
            stream_id,
            bytes_written,
        } => {
            let _ = call_rna_embind(
                &mut store,
                inst,
                rt,
                "rbx_wasm_rna_on_stream_write_success",
                &[
                    EmbindArg::UInt(id),
                    EmbindArg::Int(stream_id),
                    EmbindArg::UInt(bytes_written),
                ],
            );
        }
        RnaCallbackEvent::StreamWriteError { id, stream_id } => {
            let _ = call_rna_embind(
                &mut store,
                inst,
                rt,
                "rbx_wasm_rna_on_stream_write_error",
                &[EmbindArg::UInt(id), EmbindArg::Int(stream_id)],
            );
        }
        RnaCallbackEvent::DatagramAcked {
            id,
            datagram_hi,
            datagram_lo,
        } => {
            let _ = call_rna_embind(
                &mut store,
                inst,
                rt,
                "rbx_wasm_rna_on_datagram_acked",
                &[
                    EmbindArg::UInt(id),
                    EmbindArg::Int(datagram_hi),
                    EmbindArg::Int(datagram_lo),
                ],
            );
        }
        RnaCallbackEvent::DatagramLost {
            id,
            datagram_hi,
            datagram_lo,
        } => {
            let _ = call_rna_embind(
                &mut store,
                inst,
                rt,
                "rbx_wasm_rna_on_datagram_lost",
                &[
                    EmbindArg::UInt(id),
                    EmbindArg::Int(datagram_hi),
                    EmbindArg::Int(datagram_lo),
                ],
            );
        }
        RnaCallbackEvent::ConnectionStats { id, values } => {
            let mut args = Vec::with_capacity(17);
            args.push(EmbindArg::UInt(id));
            for v in values {
                args.push(EmbindArg::Float(v));
            }
            let _ = call_rna_embind(&mut store, inst, rt, "rbx_wasm_rna_on_connection_stats", &args);
        }
    }
    Ok(())
}
