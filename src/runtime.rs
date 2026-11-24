use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::time::Instant;
use wasmtime::{Engine, Instance, InstancePre};

use crate::audio::{AudioMixMode, CpalAudioBackend};
use crate::backends::{
    AudioBackend, HeadlessWebGl, HeadlessWebGpu, HttpBackend, HttpResponse, NullAudioBackend,
    ReqwestHttpBackend, RnaBackend, TokioWebSocketBackend, WebGlBackend, WebGpuBackend,
    WebSocketBackend, normalize_roblosecurity,
};
use crate::memory::SharedMem;
use crate::rna::{RnaCallbackEvent, WTransportRnaBackend};

pub const WASM_TLS_SIZE: usize = 0x2998;

#[derive(Debug, Clone)]
pub struct ExitStatus(pub i32);

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Program terminated with exit({})", self.0)
    }
}

impl std::error::Error for ExitStatus {}

#[derive(Debug, Clone)]
pub struct LiveRuntimeUnwind;

impl fmt::Display for LiveRuntimeUnwind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unwind to live runtime event loop")
    }
}

impl std::error::Error for LiveRuntimeUnwind {}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct EmbindTypeInfo {
    pub id: u32,
    pub name: String,
    pub kind: &'static str,
    pub char_size: usize,
    pub is_unsigned: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct EmbindFunctionInfo {
    pub name: String,
    pub arg_count: usize,
    pub arg_types: Vec<u32>,
    pub signature: String,
    pub raw_func_index: i32,
    pub context_ptr: i32,
    pub is_async: bool,
}

#[derive(Default)]
pub struct EmbindRegistry {
    pub types: HashMap<u32, EmbindTypeInfo>,
    pub functions: HashMap<String, Vec<EmbindFunctionInfo>>,
}

pub struct FiberSlot {
    pub state: Mutex<FiberControl>,
    pub cvar: Condvar,
}

#[derive(Default)]
pub struct FiberControl {
    pub ready_to_run: bool,
    pub resume_value: i32,
    pub destroy_requested: bool,
}

impl FiberSlot {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(FiberControl::default()),
            cvar: Condvar::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum PlatformInputEvent {
    Key {
        is_down: bool,
        repeat: bool,
        key: String,
        code: String,
        key_code: i32,
        char_code: i32,
        location: i32,
        ctrl: bool,
        shift: bool,
        alt: bool,
        meta: bool,
    },
    MouseButton {
        is_down: bool,
        button: i16,
        buttons: u16,
        x: i32,
        y: i32,
        ctrl: bool,
        shift: bool,
        alt: bool,
        meta: bool,
    },
    MouseMove {
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
        buttons: u16,
        ctrl: bool,
        shift: bool,
        alt: bool,
        meta: bool,
    },
    MouseWheel {
        delta_x: f64,
        delta_y: f64,
        delta_mode: i32,
        x: i32,
        y: i32,
        buttons: u16,
        ctrl: bool,
        shift: bool,
        alt: bool,
        meta: bool,
    },
    Focus {
        focused: bool,
    },
}

#[derive(Debug, Clone)]
pub struct CompletedFetch {
    pub fetch_id: i32,
    pub fetch_ptr: i32,
    pub onsuccess: i32,
    pub onerror: i32,
    pub onreadystatechange: i32,
    pub load_to_mem: bool,
    pub result: std::result::Result<HttpResponse, String>,
}

#[derive(Debug, Clone)]
pub enum WorkerTask {
    CheckMailbox,
    AsyncCall {
        fn_idx: i32,
        arg: i32,
        due: Instant,
    },
    FetchComplete(CompletedFetch),
    RnaEvent(RnaCallbackEvent),
}

#[derive(Debug, Clone)]
pub enum MainThreadTask {
    CheckMailbox,
    AsyncCall {
        fn_idx: i32,
        arg: i32,
        due: Instant,
    },
    FreeThreadData(i32),
    FetchComplete(CompletedFetch),
    WebSocketEvent {
        ws_id: i32,
        kind: u8, // 0=open, 1=message, 2=error, 3=close
        data: Vec<u8>,
        is_text: bool,
        was_clean: bool,
        close_code: u16,
        close_reason: String,
    },
    RnaEvent(RnaCallbackEvent),
    InputEvent(PlatformInputEvent),
    AudioMix(AudioMixMode),
}

pub struct OpfsCachedHandle {
    pub file: Option<std::fs::File>,
    pub ref_count: usize,
}

#[derive(Default)]
pub struct OpfsState {
    pub max_handles: i32,
    pub keep_pct: i32,
    pub sync_handles: HashMap<String, OpfsCachedHandle>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FetchEntry {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct WebSocketEntry {
    pub url: String,
    pub protocols: Vec<String>,
    pub on_open: Option<(i32, i32, i32)>,
    pub on_message: Option<(i32, i32, i32)>,
    pub on_error: Option<(i32, i32, i32)>,
    pub on_close: Option<(i32, i32, i32)>,
    pub event_ptr: i32,
    pub connected: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RnaConnEntry {
    pub url: String,
    pub receive_data_ptr: i32,
    pub receive_head_ptr: i32,
    pub receive_tail_ptr: i32,
    pub receive_slot_size: u32,
    pub receive_slot_count: u32,
    pub closed: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct EventListener {
    pub target: i32,
    pub event_type_id: i32,
    pub user_data: i32,
    pub callback: i32,
    pub use_capture: bool,
    pub target_thread: i32,
}

pub struct PlatformState {
    pub width: i32,
    pub height: i32,
    pub device_pixel_ratio: f64,
    pub pointer_locked: bool,
    pub pointer_lock_dirty: bool,
    pub page_visible: bool,
    pub key_event_buf: i32,
    pub mouse_event_buf: i32,
    pub wheel_event_buf: i32,
    pub focus_event_buf: i32,
    pub listeners: Vec<EventListener>,
    pub pending_resize: bool,
    pub last_pointer_type: String,
    pub keyboard_layout_status: i32,
    pub keyboard_layout_map: HashMap<String, String>,
    pub text_input_kind: String,
    pub text_value: String,
    pub pending_text: Vec<u8>,
    pub wasm_stack_trace_limit: f64,
    pub crash_attributes: HashMap<String, String>,
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub mouse_buttons: u16,
    pub modifiers: (bool, bool, bool, bool), // ctrl, shift, alt, meta
}

impl Default for PlatformState {
    fn default() -> Self {
        let mut keyboard_layout_map = HashMap::new();
        for ch in 'a'..='z' {
            let code = format!("Key{}", ch.to_ascii_uppercase());
            keyboard_layout_map.insert(code, ch.to_string());
        }
        Self {
            width: 1280,
            height: 720,
            device_pixel_ratio: 1.0,
            pointer_locked: false,
            pointer_lock_dirty: false,
            page_visible: true,
            key_event_buf: 0,
            mouse_event_buf: 0,
            wheel_event_buf: 0,
            focus_event_buf: 0,
            listeners: Vec::new(),
            pending_resize: false,
            last_pointer_type: "mouse".to_owned(),
            keyboard_layout_status: 1,
            keyboard_layout_map,
            text_input_kind: String::new(),
            text_value: String::new(),
            pending_text: Vec::new(),
            wasm_stack_trace_limit: 10.0,
            crash_attributes: HashMap::new(),
            cursor_x: 0,
            cursor_y: 0,
            mouse_buttons: 0,
            modifiers: (false, false, false, false),
        }
    }
}

pub struct RuntimeConfig {
    pub resources_dir: PathBuf,
    pub fs_dir: PathBuf,
    pub opfs_dir: PathBuf,
    pub host_info: HashMap<String, String>,
    pub env_vars: Vec<(String, String)>,
    pub enable_webgpu: bool,
    pub enable_audio: bool,
    pub audio_worklet: bool,
    pub audio_volume: f32,
    pub audio_muted: bool,
    pub width: i32,
    pub height: i32,
    pub device_pixel_ratio: f64,
    pub roblox_base_url: String,
    pub engine_base_url: String,
    pub channel: String,
    pub place_id: Option<u64>,
    pub auth_cookie: Option<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let mut host_info = HashMap::new();
        host_info.insert("hostOS".into(), "1".into()); // 1 = Windows / desktop
        host_info.insert("osName".into(), "Windows".into());
        host_info.insert("osVersion".into(), "10.0".into());
        host_info.insert("deviceModel".into(), "".into());
        host_info.insert("browserName".into(), "Chrome".into());
        host_info.insert("browserVersion".into(), "134.0.0.0".into());
        host_info.insert("mobile".into(), "0".into());
        host_info.insert("memoryMB".into(), "8192".into());
        host_info.insert("cpuLogicalCount".into(), cores.to_string());
        host_info.insert("screenWidthDip".into(), "1280".into());
        host_info.insert("screenHeightDip".into(), "720".into());
        host_info.insert("screenWidthPx".into(), "1280".into());
        host_info.insert("screenHeightPx".into(), "720".into());
        host_info.insert("dpi".into(), "96".into());
        host_info.insert("gpuVendor".into(), "WeldRuntime".into());
        host_info.insert("gpuRenderer".into(), "wgpu".into());
        host_info.insert("platform".into(), std::env::consts::OS.into());
        host_info.insert("architecture".into(), std::env::consts::ARCH.into());

        let place_id = std::env::var("ROBLOX_PLACE_ID")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        let auth_cookie = std::env::var("ROBLOX_COOKIE")
            .ok()
            .or_else(|| std::fs::read_to_string("./.roblosecurity").ok())
            .or_else(|| std::fs::read_to_string("./rbx-storage/auth_cookie.txt").ok())
            .map(|s| normalize_roblosecurity(&s))
            .filter(|s| !s.is_empty());
        let roblox_base_url = std::env::var("ROBLOX_BASE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "https://www.roblox.com/".into());
        let engine_base_url = std::env::var("ROBLOX_ENGINE_BASE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                "https://setup.rbxcdn.com/webclient/version-1ef59424f70d4932-RobloxWebClient"
                    .into()
            });
        let channel = std::env::var("ROBLOX_CHANNEL").unwrap_or_default();

        Self {
            resources_dir: PathBuf::from("./resources"),
            fs_dir: PathBuf::from("./resources/fs"),
            opfs_dir: PathBuf::from("./rbx-storage"),
            host_info,
            env_vars: vec![
                ("USER".into(), "web_user".into()),
                ("LOGNAME".into(), "web_user".into()),
                ("PATH".into(), "/".into()),
                ("PWD".into(), "/".into()),
                ("HOME".into(), "/home/web_user".into()),
                ("LANG".into(), "en_US.UTF-8".into()),
                ("_".into(), "./WebClient.wasm".into()),
            ],
            enable_webgpu: true,
            enable_audio: true,
            audio_worklet: true,
            audio_volume: 1.0,
            audio_muted: false,
            width: 1280,
            height: 720,
            device_pixel_ratio: 1.0,
            roblox_base_url,
            engine_base_url,
            channel,
            place_id,
            auth_cookie,
        }
    }
}

pub struct RuntimeState {
    pub engine: Engine,
    pub memory: SharedMem,
    pub instance_pre: OnceLock<Arc<InstancePre<StoreState>>>,
    pub start_time: Instant,
    pub config: RuntimeConfig,

    pub main_pthread_ptr: AtomicI32,
    pub main_loop_scheduled: AtomicBool,
    pub keepalive_count: AtomicI32,
    pub should_exit: AtomicBool,
    pub exit_code: AtomicI32,
    pub main_queue: Mutex<VecDeque<MainThreadTask>>,
    pub main_wakeup: (Mutex<bool>, Condvar),
    pub thread_mailboxes: Mutex<HashMap<i32, mpsc::Sender<WorkerTask>>>,
    pub pthread_tls: Mutex<HashMap<i32, i32>>,

    pub fibers: Mutex<HashMap<i32, Arc<FiberSlot>>>,

    pub embind: Mutex<EmbindRegistry>,
    pub opfs: Mutex<OpfsState>,
    pub fetches: Mutex<HashMap<i32, FetchEntry>>,
    pub active_fetches: Mutex<HashSet<i32>>,
    pub next_fetch_id: AtomicI32,
    pub websockets: Mutex<HashMap<i32, WebSocketEntry>>,
    pub next_ws_id: AtomicI32,
    pub rna_conns: Mutex<HashMap<u32, RnaConnEntry>>,
    pub platform: Mutex<PlatformState>,

    pub http_backend: Arc<dyn HttpBackend>,
    pub ws_backend: Arc<dyn WebSocketBackend>,
    pub rna_backend: Arc<dyn RnaBackend>,
    pub webgl_backend: Arc<dyn WebGlBackend>,
    pub webgpu_backend: Arc<dyn WebGpuBackend>,
    pub audio_backend: Arc<dyn AudioBackend>,
    pub audio_resumed: AtomicBool,

    pub rng_counter: AtomicU32,
}

impl RuntimeState {
    pub fn new(
        engine: Engine,
        memory: SharedMem,
        mut config: RuntimeConfig,
        webgpu_backend: Option<Arc<dyn WebGpuBackend>>,
    ) -> Arc<Self> {
        let mut platform = PlatformState::default();
        platform.width = config.width;
        platform.height = config.height;
        platform.device_pixel_ratio = config.device_pixel_ratio;

        let gpu = webgpu_backend.unwrap_or_else(|| Arc::new(HeadlessWebGpu::new()));
        let (vendor, renderer) = gpu.adapter_names();
        if !vendor.is_empty() {
            config.host_info.insert("gpuVendor".into(), vendor);
        }
        if !renderer.is_empty() {
            config.host_info.insert("gpuRenderer".into(), renderer);
        }

        let ws_backend = Arc::new(TokioWebSocketBackend::new());
        let rna_backend = Arc::new(WTransportRnaBackend::new());
        let cpal_audio = if config.enable_audio {
            match CpalAudioBackend::try_new(
                memory.clone(),
                config.audio_volume,
                config.audio_muted,
                config.audio_worklet,
            ) {
                Ok(a) => Some(a),
                Err(err) => {
                    eprintln!("[Audio] Falling back to silent NullAudioBackend: {err}");
                    None
                }
            }
        } else {
            println!("[Audio] Disabled via configuration (using NullAudioBackend)");
            None
        };
        let audio_backend: Arc<dyn AudioBackend> = match cpal_audio {
            Some(ref a) => a.clone(),
            None => Arc::new(NullAudioBackend),
        };
        let rt = Arc::new(Self {
            engine,
            memory,
            instance_pre: OnceLock::new(),
            start_time: Instant::now(),
            config,
            main_pthread_ptr: AtomicI32::new(0),
            main_loop_scheduled: AtomicBool::new(false),
            keepalive_count: AtomicI32::new(0),
            should_exit: AtomicBool::new(false),
            exit_code: AtomicI32::new(0),
            main_queue: Mutex::new(VecDeque::new()),
            main_wakeup: (Mutex::new(false), Condvar::new()),
            thread_mailboxes: Mutex::new(HashMap::new()),
            pthread_tls: Mutex::new(HashMap::new()),
            fibers: Mutex::new(HashMap::new()),
            embind: Mutex::new(EmbindRegistry::default()),
            opfs: Mutex::new(OpfsState::default()),
            fetches: Mutex::new(HashMap::new()),
            active_fetches: Mutex::new(HashSet::new()),
            next_fetch_id: AtomicI32::new(1),
            websockets: Mutex::new(HashMap::new()),
            next_ws_id: AtomicI32::new(1),
            rna_conns: Mutex::new(HashMap::new()),
            platform: Mutex::new(platform),
            http_backend: Arc::new(ReqwestHttpBackend::new()),
            ws_backend: ws_backend.clone(),
            rna_backend: rna_backend.clone(),
            webgl_backend: Arc::new(HeadlessWebGl::new()),
            webgpu_backend: gpu,
            audio_backend,
            audio_resumed: AtomicBool::new(false),
            rng_counter: AtomicU32::new(1),
        });
        ws_backend.attach_runtime(Arc::downgrade(&rt));
        rna_backend.attach_runtime(Arc::downgrade(&rt));
        if let Some(a) = cpal_audio {
            a.attach_runtime(Arc::downgrade(&rt));
        }
        rt
    }

    pub fn ms_since_start(&self) -> f64 {
        self.start_time.elapsed().as_secs_f64() * 1000.0
    }

    #[allow(dead_code)]
    pub fn rna_try_write_receive_slot(&self, conn_id: u32, data: &[u8]) -> wasmtime::Result<usize> {
        let entry = {
            let guard = self.rna_conns.lock().unwrap();
            guard.get(&conn_id).cloned()
        };
        let Some(entry) = entry else {
            return Ok(0);
        };
        if entry.closed || entry.receive_slot_count == 0 {
            return Ok(0);
        }
        let head = self.memory.atomic_load_i32(entry.receive_head_ptr)? as u32;
        let tail = self.memory.atomic_load_i32(entry.receive_tail_ptr)? as u32;
        if head.wrapping_sub(tail) >= entry.receive_slot_count {
            return Ok(0);
        }
        let n = data.len().min(entry.receive_slot_size as usize);
        let slot_idx = head % entry.receive_slot_count;
        let dst = entry
            .receive_data_ptr
            .wrapping_add((slot_idx * entry.receive_slot_size) as i32);
        self.memory.write_bytes(dst, &data[..n])?;
        self.memory
            .atomic_store_i32(entry.receive_head_ptr, head.wrapping_add(1) as i32)?;
        Ok(n)
    }

    pub fn push_main_task(&self, task: MainThreadTask) {
        self.main_queue.lock().unwrap().push_back(task);
        let (lock, cvar) = &self.main_wakeup;
        if let Ok(mut notified) = lock.lock() {
            *notified = true;
            cvar.notify_all();
        }
    }

    pub fn push_worker_task(&self, pthread_ptr: i32, task: WorkerTask) -> bool {
        if let Some(tx) = self.thread_mailboxes.lock().unwrap().get(&pthread_ptr) {
            tx.send(task).is_ok()
        } else {
            false
        }
    }

    pub fn push_rna_event(&self, owner_thread: i32, ev: RnaCallbackEvent) {
        let main_ptr = self.main_pthread_ptr.load(std::sync::atomic::Ordering::SeqCst);
        if owner_thread != 0
            && owner_thread != main_ptr
            && self.push_worker_task(owner_thread, WorkerTask::RnaEvent(ev.clone()))
        {
            return;
        }
        self.push_main_task(MainThreadTask::RnaEvent(ev));
    }
}

pub struct StoreState {
    pub runtime: Arc<RuntimeState>,
    pub instance: Option<Instance>,
    pub is_main_thread: bool,
    pub pthread_ptr: i32,
    pub proxied_caller_thread: i32,
    pub keepalive: i32,
}

impl StoreState {
    pub fn main_thread(runtime: Arc<RuntimeState>) -> Self {
        Self {
            runtime,
            instance: None,
            is_main_thread: true,
            pthread_ptr: 0,
            proxied_caller_thread: 0,
            keepalive: 0,
        }
    }

    pub fn worker_thread(runtime: Arc<RuntimeState>, pthread_ptr: i32) -> Self {
        Self {
            runtime,
            instance: None,
            is_main_thread: false,
            pthread_ptr,
            proxied_caller_thread: 0,
            keepalive: 0,
        }
    }

    pub fn fiber(runtime: Arc<RuntimeState>, is_main_thread: bool, pthread_ptr: i32) -> Self {
        Self {
            runtime,
            instance: None,
            is_main_thread,
            pthread_ptr,
            proxied_caller_thread: 0,
            keepalive: 0,
        }
    }
}
