mod audio;
mod backends;
mod em_asm;
mod embind;
mod fibers;
mod gfx;
mod memory;
mod network;
mod opfs;
mod platform;
mod rna;
mod runtime;
mod threads;
mod wasi;
mod wgpu_backend;

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use wasmtime::{Config, Engine, Linker, MemoryType, Module, Result, SharedMemory, Store};
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, KeyCode, ModifiersState, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

use crate::backends::{HttpRequest, WebGpuBackend, normalize_roblosecurity};
use crate::embind::{EmbindArg, call_function, has_function};
use crate::memory::{SharedMem, instance_free, instance_malloc};
use crate::runtime::{
    ExitStatus, LiveRuntimeUnwind, MainThreadTask, PlatformInputEvent, RuntimeConfig,
    RuntimeState, StoreState,
};
use crate::wgpu_backend::RealWebGpu;

fn load_config_file(cfg: &mut RuntimeConfig, path: &Path) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&raw) else {
        eprintln!("[Config] Failed to parse JSON config from {}", path.display());
        return;
    };
    if let Some(pid) = map.get("place_id").or_else(|| map.get("placeId")) {
        if let Some(n) = pid.as_u64() {
            cfg.place_id = Some(n);
        } else if let Some(s) = pid.as_str() {
            cfg.place_id = parse_place_id_arg(s);
        }
    }
    if let Some(c) = map
        .get("auth_cookie")
        .or_else(|| map.get("authCookie"))
        .or_else(|| map.get("cookie"))
        .and_then(|v| v.as_str())
    {
        let clean = normalize_roblosecurity(c);
        if !clean.is_empty() {
            cfg.auth_cookie = Some(clean);
        }
    }
    if let Some(url) = map
        .get("base_url")
        .or_else(|| map.get("roblox_base_url"))
        .or_else(|| map.get("baseUrl"))
        .and_then(|v| v.as_str())
    {
        if !url.trim().is_empty() {
            cfg.roblox_base_url = ensure_trailing_slash(url);
        }
    }
    if let Some(eurl) = map
        .get("engine_base_url")
        .or_else(|| map.get("engineBase"))
        .and_then(|v| v.as_str())
    {
        if !eurl.trim().is_empty() {
            cfg.engine_base_url = eurl.trim().trim_end_matches('/').to_owned();
        }
    }
    if let Some(ch) = map.get("channel").and_then(|v| v.as_str()) {
        cfg.channel = ch.to_owned();
    }
    if let Some(w) = map.get("width").and_then(|v| v.as_i64()) {
        cfg.width = (w as i32).max(1);
    }
    if let Some(h) = map.get("height").and_then(|v| v.as_i64()) {
        cfg.height = (h as i32).max(1);
    }
    if let Some(wgpu) = map.get("enable_webgpu").and_then(|v| v.as_bool()) {
        cfg.enable_webgpu = wgpu;
    }
    if let Some(aud) = map
        .get("enable_audio")
        .or_else(|| map.get("audio"))
        .and_then(|v| v.as_bool())
    {
        cfg.enable_audio = aud;
    }
    if let Some(wk) = map.get("audio_worklet").and_then(|v| v.as_bool()) {
        cfg.audio_worklet = wk;
    }
    if let Some(vol) = map
        .get("audio_volume")
        .or_else(|| map.get("volume"))
        .and_then(|v| v.as_f64())
    {
        cfg.audio_volume = (vol as f32).clamp(0.0, 2.0);
    }
    if let Some(m) = map
        .get("audio_muted")
        .or_else(|| map.get("mute"))
        .and_then(|v| v.as_bool())
    {
        cfg.audio_muted = m;
    }
    println!("[Config] Loaded settings from {}", path.display());
}

fn ensure_trailing_slash(url: &str) -> String {
    let t = url.trim();
    let with_scheme = if t.contains("://") {
        t.to_owned()
    } else {
        format!("https://{t}")
    };
    if with_scheme.ends_with('/') {
        with_scheme
    } else {
        format!("{with_scheme}/")
    }
}

fn parse_place_id_arg(s: &str) -> Option<u64> {
    let trimmed = s.trim();
    if let Ok(id) = trimmed.parse::<u64>() {
        return (id > 0).then_some(id);
    }
    if let Some(idx) = trimmed.find("placeId=") {
        let rest = &trimmed[idx + "placeId=".len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(id) = digits.parse::<u64>() {
            return (id > 0).then_some(id);
        }
    }
    if let Some(idx) = trimmed.find("/games/") {
        let rest = &trimmed[idx + "/games/".len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(id) = digits.parse::<u64>() {
            return (id > 0).then_some(id);
        }
    }
    None
}

fn print_usage() {
    println!(
        "\
Usage: weld_runtime [OPTIONS] [PLACE_ID_OR_URL]

Options:
  -p, --place-id <ID|URL>   Roblox Place ID or game URL (env: ROBLOX_PLACE_ID)
  -c, --cookie <COOKIE>     .ROBLOSECURITY auth cookie (env: ROBLOX_COOKIE)
      --cookie-file <PATH>  Read .ROBLOSECURITY cookie from file (default checks ./.roblosecurity, ./rbx-storage/auth_cookie.txt)
      --config <PATH>       Load JSON config (default checks ./weld.json)
      --base-url <URL>      Roblox base URL (default: https://www.roblox.com/)
      --engine-base <URL>   CDN engine base URL for VFS assets (env: ROBLOX_ENGINE_BASE_URL)
      --channel <NAME>      Engine channel override (env: ROBLOX_CHANNEL)
      --headless            Run without creating a GUI window (uses HeadlessWebGpu)
      --no-webgpu           Force WebGL fallback instead of WebGPU
      --no-audio            Disable audio output (uses silent NullAudioBackend)
      --mute                Start with audio output muted
      --volume <0.0..2.0>   Set master output volume (default: 1.0)
      --audio-script-proc   Use FMOD ScriptProcessor fallback instead of AudioWorklet fastpath
  -h, --help                Show this help message"
    );
}

fn parse_args(cfg: &mut RuntimeConfig) -> bool {
    let default_cfg_path = Path::new("./weld.json");
    if default_cfg_path.is_file() {
        load_config_file(cfg, default_cfg_path);
    }

    let mut headless = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "--headless" => headless = true,
            "--no-webgpu" => cfg.enable_webgpu = false,
            "--no-audio" => cfg.enable_audio = false,
            "--mute" => cfg.audio_muted = true,
            "--audio-script-proc" => cfg.audio_worklet = false,
            "--volume" => {
                if let Some(v) = args.next() {
                    if let Ok(vol) = v.trim().parse::<f32>() {
                        cfg.audio_volume = vol.clamp(0.0, 2.0);
                    }
                }
            }
            "--config" => {
                if let Some(v) = args.next() {
                    load_config_file(cfg, Path::new(&v));
                }
            }
            "-p" | "--place-id" => {
                if let Some(v) = args.next() {
                    cfg.place_id = parse_place_id_arg(&v);
                }
            }
            "--base-url" => {
                if let Some(v) = args.next() {
                    cfg.roblox_base_url = ensure_trailing_slash(&v);
                }
            }
            "--engine-base" => {
                if let Some(v) = args.next() {
                    let clean = v.trim().trim_end_matches('/');
                    if !clean.is_empty() {
                        cfg.engine_base_url = clean.to_owned();
                    }
                }
            }
            "--channel" => {
                if let Some(v) = args.next() {
                    cfg.channel = v;
                }
            }
            "-c" | "--cookie" => {
                if let Some(v) = args.next() {
                    let clean = normalize_roblosecurity(&v);
                    if !clean.is_empty() {
                        cfg.auth_cookie = Some(clean);
                    }
                }
            }
            "--cookie-file" => {
                if let Some(v) = args.next() {
                    match std::fs::read_to_string(&v) {
                        Ok(raw) => {
                            let clean = normalize_roblosecurity(&raw);
                            if !clean.is_empty() {
                                cfg.auth_cookie = Some(clean);
                            }
                        }
                        Err(e) => eprintln!("[Config] Failed to read cookie file {v}: {e}"),
                    }
                }
            }
            other if !other.starts_with('-') && cfg.place_id.is_none() => {
                cfg.place_id = parse_place_id_arg(other);
            }
            _ => {}
        }
    }
    headless
}

fn main() -> Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    println!("=== WeldRuntime ===");
    let mut rt_config = RuntimeConfig::default();
    let headless = parse_args(&mut rt_config);

    match rt_config.place_id {
        Some(pid) => println!("[Config] Startup place_id = {pid}"),
        None => eprintln!(
            "[Config] Warning: startup place_id is not set! Engine will exit after init.\n\
             [Config] Pass `--place-id <ID>` (or `ROBLOX_PLACE_ID=<ID>` / `weld.json`) to join a place."
        ),
    }
    if let Some(ref c) = rt_config.auth_cookie {
        println!("[Config] Auth cookie loaded (len={} chars)", c.len());
    } else {
        eprintln!(
            "[Config] Warning: no .ROBLOSECURITY cookie configured (`--cookie`, `--cookie-file`, `ROBLOX_COOKIE`, or `./.roblosecurity`).\n\
             [Config] Joining public Roblox servers requires an authenticated session."
        );
    }

    if headless {
        let rt_ref = Arc::new(std::sync::OnceLock::<Arc<RuntimeState>>::new());
        run_wasm_main(rt_config, None, rt_ref)
    } else {
        eprintln!("[winit] Creating EventLoop...");
        let event_loop = EventLoop::new().map_err(|e| wasmtime::format_err!("EventLoop: {e}"))?;
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            std::time::Instant::now() + Duration::from_millis(16),
        ));
        let mut app = WeldApp {
            config: Some(rt_config),
            window: None,
            rt_slot: Arc::new(std::sync::OnceLock::new()),
            wasm_thread: None,
            modifiers: ModifiersState::empty(),
        };
        eprintln!("[winit] Running EventLoop...");
        event_loop
            .run_app(&mut app)
            .map_err(|e| wasmtime::format_err!("EventLoop run: {e}"))?;
        if let Some(handle) = app.wasm_thread.take() {
            if let Err(e) = handle.join() {
                eprintln!("[Runtime] WASM thread panicked: {e:?}");
            }
        }
        Ok(())
    }
}

struct WeldApp {
    config: Option<RuntimeConfig>,
    window: Option<Arc<Window>>,
    rt_slot: Arc<std::sync::OnceLock<Arc<RuntimeState>>>,
    wasm_thread: Option<thread::JoinHandle<()>>,
    modifiers: ModifiersState,
}

impl ApplicationHandler for WeldApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        eprintln!("[winit] resumed: creating window...");
        let cfg = self.config.take().unwrap_or_default();
        let attrs = Window::default_attributes()
            .with_title("Roblox WebClient (WeldRuntime / wgpu)")
            .with_inner_size(winit::dpi::LogicalSize::new(
                cfg.width as f64,
                cfg.height as f64,
            ));
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create winit window"),
        );
        eprintln!("[winit] Window created, initializing wgpu...");
        let dpr = window.scale_factor();
        let phys = window.inner_size();

        let mut cfg = cfg;
        cfg.width = phys.width.max(1) as i32;
        cfg.height = phys.height.max(1) as i32;
        cfg.device_pixel_ratio = dpr;

        let webgpu: Arc<dyn WebGpuBackend> = match RealWebGpu::new(window.clone()) {
            Ok(gpu) => Arc::new(gpu),
            Err(e) => {
                eprintln!("[wgpu] Failed to initialize real WebGPU backend: {e:#}");
                std::process::exit(1);
            }
        };

        self.window = Some(window);
        let rt_slot = self.rt_slot.clone();
        let handle = thread::Builder::new()
            .name("rbx-wasm-main".into())
            .spawn(move || {
                if let Err(err) = run_wasm_main(cfg, Some(webgpu), rt_slot.clone()) {
                    eprintln!("[Runtime] Fatal WASM error: {err:#}");
                }
                if let Some(rt) = rt_slot.get() {
                    rt.should_exit.store(true, Ordering::SeqCst);
                }
            })
            .expect("failed to spawn rbx-wasm-main thread");
        self.wasm_thread = Some(handle);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let rt = self.rt_slot.get().cloned();
        match event {
            WindowEvent::CloseRequested => {
                if let Some(ref rt) = rt {
                    rt.should_exit.store(true, Ordering::SeqCst);
                }
                event_loop.exit();
            }
            WindowEvent::Resized(new_size) => {
                let w = new_size.width.max(1);
                let h = new_size.height.max(1);
                if let Some(ref rt) = rt {
                    {
                        let mut p = rt.platform.lock().unwrap();
                        p.width = w as i32;
                        p.height = h as i32;
                        p.pending_resize = true;
                    }
                    rt.webgpu_backend.resize_surface(w, h);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(ref rt) = rt {
                    let mut p = rt.platform.lock().unwrap();
                    p.device_pixel_ratio = scale_factor;
                    p.pending_resize = true;
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
                if let Some(ref rt) = rt {
                    let s = self.modifiers;
                    rt.platform.lock().unwrap().modifiers = (
                        s.control_key(),
                        s.shift_key(),
                        s.alt_key(),
                        s.super_key(),
                    );
                }
            }
            WindowEvent::Focused(focused) => {
                if !focused {
                    if let Some(ref w) = self.window {
                        let _ = w.set_cursor_grab(CursorGrabMode::None);
                        w.set_cursor_visible(true);
                    }
                }
                if let Some(ref rt) = rt {
                    {
                        let mut p = rt.platform.lock().unwrap();
                        p.page_visible = focused;
                        if !focused && p.pointer_locked {
                            p.pointer_locked = false;
                            p.pointer_lock_dirty = false;
                        }
                    }
                    rt.push_main_task(MainThreadTask::InputEvent(PlatformInputEvent::Focus {
                        focused,
                    }));
                }
            }
            WindowEvent::KeyboardInput { event: kev, .. } => {
                if let Some(ref rt) = rt {
                    let is_down = kev.state == ElementState::Pressed;
                    let (ctrl, shift, alt, meta) = (
                        self.modifiers.control_key(),
                        self.modifiers.shift_key(),
                        self.modifiers.alt_key(),
                        self.modifiers.super_key(),
                    );
                    let (code, key_code) = map_physical_key(kev.physical_key);
                    let key = match &kev.logical_key {
                        Key::Character(s) => s.to_string(),
                        Key::Named(n) => format!("{n:?}"),
                        _ => code.clone(),
                    };
                    let char_code = kev
                        .text
                        .as_ref()
                        .and_then(|t| t.chars().next())
                        .map(|c| c as i32)
                        .unwrap_or(0);
                    rt.push_main_task(MainThreadTask::InputEvent(PlatformInputEvent::Key {
                        is_down,
                        repeat: kev.repeat,
                        key,
                        code,
                        key_code,
                        char_code,
                        location: 0,
                        ctrl,
                        shift,
                        alt,
                        meta,
                    }));
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(ref rt) = rt {
                    let (x, y) = (position.x as i32, position.y as i32);
                    let locked_and_moved = {
                        let mut p = rt.platform.lock().unwrap();
                        let dx = x - p.cursor_x;
                        let dy = y - p.cursor_y;
                        p.cursor_x = x;
                        p.cursor_y = y;
                        p.last_pointer_type = "mouse".into();
                        if p.pointer_locked {
                            None
                        } else {
                            Some((dx, dy, p.mouse_buttons, p.modifiers))
                        }
                    };
                    if let Some((dx, dy, buttons, (ctrl, shift, alt, meta))) = locked_and_moved {
                        rt.push_main_task(MainThreadTask::InputEvent(
                            PlatformInputEvent::MouseMove {
                                x,
                                y,
                                dx,
                                dy,
                                buttons,
                                ctrl,
                                shift,
                                alt,
                                meta,
                            },
                        ));
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(ref rt) = rt {
                    let is_down = state == ElementState::Pressed;
                    let (btn_id, mask) = match button {
                        MouseButton::Left => (0i16, 1u16),
                        MouseButton::Middle => (1i16, 4u16),
                        MouseButton::Right => (2i16, 2u16),
                        MouseButton::Back => (3i16, 8u16),
                        MouseButton::Forward => (4i16, 16u16),
                        MouseButton::Other(n) => (n as i16, 0u16),
                    };
                    let (x, y, buttons, (ctrl, shift, alt, meta)) = {
                        let mut p = rt.platform.lock().unwrap();
                        if is_down {
                            p.mouse_buttons |= mask;
                        } else {
                            p.mouse_buttons &= !mask;
                        }
                        p.last_pointer_type = "mouse".into();
                        (p.cursor_x, p.cursor_y, p.mouse_buttons, p.modifiers)
                    };
                    rt.push_main_task(MainThreadTask::InputEvent(
                        PlatformInputEvent::MouseButton {
                            is_down,
                            button: btn_id,
                            buttons,
                            x,
                            y,
                            ctrl,
                            shift,
                            alt,
                            meta,
                        },
                    ));
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(ref rt) = rt {
                    let (delta_x, delta_y, delta_mode) = match delta {
                        MouseScrollDelta::LineDelta(x, y) => (-x as f64 * 40.0, -y as f64 * 40.0, 0),
                        MouseScrollDelta::PixelDelta(pos) => (-pos.x, -pos.y, 0),
                    };
                    let (x, y, buttons, (ctrl, shift, alt, meta)) = {
                        let p = rt.platform.lock().unwrap();
                        (p.cursor_x, p.cursor_y, p.mouse_buttons, p.modifiers)
                    };
                    rt.push_main_task(MainThreadTask::InputEvent(
                        PlatformInputEvent::MouseWheel {
                            delta_x,
                            delta_y,
                            delta_mode,
                            x,
                            y,
                            buttons,
                            ctrl,
                            shift,
                            alt,
                            meta,
                        },
                    ));
                }
            }
            _ => {}
        }
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: DeviceId,
        event: DeviceEvent,
    ) {
        if let DeviceEvent::MouseMotion { delta: (dx_f, dy_f) } = event {
            if let Some(rt) = self.rt_slot.get() {
                let state_opt = {
                    let p = rt.platform.lock().unwrap();
                    if p.pointer_locked {
                        Some((p.cursor_x, p.cursor_y, p.mouse_buttons, p.modifiers))
                    } else {
                        None
                    }
                };
                if let Some((x, y, buttons, (ctrl, shift, alt, meta))) = state_opt {
                    let dx = dx_f.round() as i32;
                    let dy = dy_f.round() as i32;
                    if dx != 0 || dy != 0 {
                        rt.push_main_task(MainThreadTask::InputEvent(
                            PlatformInputEvent::MouseMove {
                                x,
                                y,
                                dx,
                                dy,
                                buttons,
                                ctrl,
                                shift,
                                alt,
                                meta,
                            },
                        ));
                    }
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(rt) = self.rt_slot.get() {
            if rt.should_exit.load(Ordering::SeqCst) {
                event_loop.exit();
                return;
            }
            if let Some(ref w) = self.window {
                let lock_change = {
                    let mut p = rt.platform.lock().unwrap();
                    if p.pointer_lock_dirty {
                        p.pointer_lock_dirty = false;
                        Some(p.pointer_locked)
                    } else {
                        None
                    }
                };
                if let Some(locked) = lock_change {
                    if locked {
                        let _ = w
                            .set_cursor_grab(CursorGrabMode::Locked)
                            .or_else(|_| w.set_cursor_grab(CursorGrabMode::Confined));
                        w.set_cursor_visible(false);
                    } else {
                        let _ = w.set_cursor_grab(CursorGrabMode::None);
                        w.set_cursor_visible(true);
                    }
                }
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            std::time::Instant::now() + Duration::from_millis(8),
        ));
    }
}

fn run_wasm_main(
    rt_config: RuntimeConfig,
    webgpu_backend: Option<Arc<dyn WebGpuBackend>>,
    rt_slot: Arc<std::sync::OnceLock<Arc<RuntimeState>>>,
) -> Result<()> {
    println!("[Runtime] Initializing Wasmtime...");

    let mut config = Config::new();
    config.wasm_threads(true);
    config.shared_memory(true);
    config.max_wasm_stack(1024 * 1024);

    let engine = Engine::new(&config)?;

    let wasm_path = "./resources/WebClient.wasm";
    let cwasm_path = "./resources/WebClient.cwasm";

    let module = if Path::new(cwasm_path).exists() {
        println!("[Runtime] Loading precompiled WASM from cache...");
        unsafe { Module::deserialize_file(&engine, cwasm_path)? }
    } else {
        println!("[Runtime] Compiling Roblox WASM (this may take a while)...");
        let wasm_bytes = std::fs::read(wasm_path)?;
        let cwasm_bytes = engine.precompile_module(&wasm_bytes)?;
        std::fs::write(cwasm_path, &cwasm_bytes)?;
        println!("[Runtime] Saved precompiled module to {cwasm_path}");
        unsafe { Module::deserialize(&engine, &cwasm_bytes)? }
    };

    println!("[Runtime] Module ready!");

    let shared_memory = SharedMemory::new(&engine, MemoryType::shared(4096, 65536)?)?;
    let rt = RuntimeState::new(
        engine.clone(),
        SharedMem::new(shared_memory.clone()),
        rt_config,
        webgpu_backend,
    );
    let _ = rt_slot.set(rt.clone());

    let mut store = Store::new(&engine, StoreState::main_thread(rt.clone()));
    let mut linker = Linker::new(&engine);

    linker.define(&store, "env", "memory", shared_memory)?;

    wasi::register(&mut linker)?;
    opfs::register(&mut linker)?;
    embind::register(&mut linker)?;
    fibers::register(&mut linker)?;
    em_asm::register(&mut linker)?;
    threads::register(&mut linker)?;
    network::register(&mut linker)?;
    gfx::register(&mut linker)?;
    platform::register(&mut linker)?;

    let missing: Vec<String> = module
        .imports()
        .filter(|imp| linker.get_by_import(&mut store, imp).is_none())
        .map(|imp| format!("{}.{}", imp.module(), imp.name()))
        .collect();
    if !missing.is_empty() {
        wasmtime::bail!("Unresolved WASM imports ({}): {:?}", missing.len(), missing);
    }

    println!("[Runtime] Pre-instantiating module (all imports resolved)...");
    let instance_pre = Arc::new(linker.instantiate_pre(&module)?);
    let _ = rt.instance_pre.set(instance_pre.clone());

    let instance = instance_pre.instantiate(&mut store)?;
    store.data_mut().instance = Some(instance);
    println!("[Runtime] WASM instantiated!");

    // 1. Global constructors and Embind registration
    let ctors = instance.get_typed_func::<(), ()>(&mut store, "__wasm_call_ctors")?;
    if let Err(err) = ctors.call(&mut store, ()) {
        if let Some(code) = extract_exit_or_unwind(&err)? {
            println!("[Runtime] Exited during ctors with code {code}");
            return Ok(());
        }
    }
    {
        let embind = rt.embind.lock().unwrap();
        let mut names: Vec<&str> = embind.functions.keys().map(String::as_str).collect::<HashSet<_>>().into_iter().collect();
        names.sort();
        println!(
            "[Runtime] Constructors executed! Embind types={}, functions={}",
            embind.types.len(),
            names.len()
        );
        println!("[Embind] Registered: {}", names.join(", "));
    }

    // 2. VFS and WasmPlayer.js pre-main configuration
    init_rbx_vfs(&mut store, instance, &rt)?;
    init_embind_player(&mut store, instance, &rt)?;

    // 3. __main_argc_argv
    let prog = "./WebClient.wasm";
    let arg0_ptr = instance_malloc(&mut store, instance, true, prog.len() + 1)?;
    rt.memory.write_cstr(arg0_ptr, prog, prog.len() + 1)?;
    let argv_ptr = instance_malloc(&mut store, instance, true, 8)?;
    rt.memory.write_i32(argv_ptr, arg0_ptr)?;
    rt.memory.write_i32(argv_ptr + 4, 0)?;

    let main_fn = instance.get_typed_func::<(i32, i32), i32>(&mut store, "__main_argc_argv")?;

    match main_fn.call(&mut store, (1, argv_ptr)) {
        Ok(code) => {
            println!("[Runtime] __main_argc_argv returned {code}");
        }
        Err(err) => {
            if err.is::<wasmtime::ThrownException>() {
                dump_wasm_exception(&mut store, &rt.memory);
                return Err(err);
            }
            match extract_exit_or_unwind(&err)? {
                Some(code) => {
                    println!("[Runtime] __main_argc_argv exited with {code}");
                    return Ok(());
                }
                None => println!("[Runtime] __main_argc_argv unwound into live runtime"),
            }
        }
    }

    let _ = instance_free(&mut store, instance, true, argv_ptr);
    let _ = instance_free(&mut store, instance, true, arg0_ptr);

    // 4. Main engine loop
    if rt.config.enable_audio && has_function(&rt, "resume_audio") {
        let _ = call_function(&mut store, instance, &rt, "resume_audio", &[]);
    }
    let iter_fn = instance
        .get_typed_func::<(), ()>(&mut store, "main_loop_iteration")
        .ok();
    let mut loop_tick: u64 = 0;
    while !rt.should_exit.load(Ordering::SeqCst)
        && (rt.main_loop_scheduled.load(Ordering::SeqCst)
            || rt.keepalive_count.load(Ordering::SeqCst) > 0)
    {
        threads::pump_main_tasks(&mut store, instance)?;
        if rt.main_loop_scheduled.load(Ordering::SeqCst) {
            if let Some(ref iter) = iter_fn {
                if let Err(err) = iter.call(&mut store, ()) {
                    if err.is::<wasmtime::ThrownException>() {
                        dump_wasm_exception(&mut store, &rt.memory);
                        return Err(err);
                    }
                    if let Some(code) = extract_exit_or_unwind(&err)? {
                        println!("[Runtime] Main loop exited with {code}");
                        break;
                    }
                }
            }
            loop_tick = loop_tick.wrapping_add(1);
            // In a native desktop host there is no browser autoplay restriction; trigger
            // `resume_audio` once FMOD has finished initializing if no input gesture happened yet.
            if loop_tick == 30
                && rt.config.enable_audio
                && !rt.audio_resumed.load(Ordering::Relaxed)
                && has_function(&rt, "resume_audio")
            {
                let _ = call_function(&mut store, instance, &rt, "resume_audio", &[]);
            }
        }
        let sleep_budget = if rt.main_loop_scheduled.load(Ordering::SeqCst) {
            Duration::from_millis(2)
        } else {
            Duration::from_millis(8)
        };
        let (lock, cvar) = &rt.main_wakeup;
        if let Ok(mut notified) = lock.lock() {
            if !*notified && rt.main_queue.lock().unwrap().is_empty() {
                if let Ok((mut g, _)) = cvar.wait_timeout(notified, sleep_budget) {
                    *g = false;
                }
            } else {
                *notified = false;
            }
        }
    }

    println!(
        "[Runtime] Finished (exit_code={})",
        rt.exit_code.load(Ordering::SeqCst)
    );
    Ok(())
}

fn extract_exit_or_unwind(err: &wasmtime::Error) -> Result<Option<i32>> {
    if let Some(ExitStatus(code)) = err.downcast_ref::<ExitStatus>() {
        return Ok(Some(*code));
    }
    if err.downcast_ref::<LiveRuntimeUnwind>().is_some() {
        return Ok(None);
    }
    Err(wasmtime::format_err!("{err:?}"))
}

fn try_read_ascii(mem: &SharedMem, ptr: i32, max_len: usize) -> Option<String> {
    let addr = ptr as u32 as usize;
    if addr < 1024 || addr >= mem.byte_len() {
        return None;
    }
    let avail = (mem.byte_len() - addr).min(max_len);
    let bytes = mem.read_bytes(ptr, avail).ok()?;
    let end = bytes.iter().position(|&b| b == 0)?;
    if end < 2 {
        return None;
    }
    let slice = &bytes[..end];
    if slice
        .iter()
        .all(|&b| (0x20..=0x7e).contains(&b) || b == b'\n' || b == b'\t')
    {
        Some(String::from_utf8_lossy(slice).into_owned())
    } else {
        None
    }
}

fn dump_wasm_exception(store: &mut Store<StoreState>, mem: &SharedMem) {
    let Some(exn) = store.take_pending_exception() else {
        eprintln!("[Exception] ThrownException without pending ExnRef");
        return;
    };

    for idx in 0..4 {
        let Ok(val) = exn.field(&mut *store, idx) else {
            break;
        };
        eprintln!("[Exception] field[{idx}] = {val:?}");

        if let wasmtime::Val::I32(obj_ptr) = val {
            if obj_ptr == 0 {
                continue;
            }
            for off in (-96..0).step_by(4) {
                if let Ok(ti_ptr) = mem.read_i32(obj_ptr.wrapping_add(off)) {
                    if let Ok(name_ptr) = mem.read_i32(ti_ptr.wrapping_add(4)) {
                        if let Some(ti_name) = try_read_ascii(mem, name_ptr, 256) {
                            eprintln!("[Exception] candidate type_info (off={off}): {ti_name}");
                        }
                    }
                }
            }
            for off in (0..96).step_by(4) {
                if let Ok(p) = mem.read_i32(obj_ptr.wrapping_add(off)) {
                    if let Some(s) = try_read_ascii(mem, p, 512) {
                        eprintln!("[Exception] obj+{off:#x} -> {s:?}");
                    }
                }
            }
        }
    }
}

fn init_rbx_vfs(
    store: &mut Store<StoreState>,
    instance: wasmtime::Instance,
    rt: &Arc<RuntimeState>,
) -> Result<()> {
    let engine_base = rt.config.engine_base_url.trim_end_matches('/');
    if let Ok(set_base) = instance.get_typed_func::<i32, ()>(&mut *store, "rbx_fs_set_base_url") {
        let base_ptr = instance_malloc(&mut *store, instance, true, engine_base.len() + 1)?;
        rt.memory
            .write_cstr(base_ptr, engine_base, engine_base.len() + 1)?;
        set_base.call(&mut *store, base_ptr)?;
        let _ = instance_free(&mut *store, instance, true, base_ptr);
    }

    let add_file = instance
        .get_typed_func::<(i32, i32, i32), i32>(&mut *store, "rbx_fs_add_file")
        .ok();
    let load_index = instance
        .get_typed_func::<(i32, i32), i32>(&mut *store, "rbx_fs_load_index")
        .ok();

    let fsindex_path = rt.config.resources_dir.join("WebClient.fsindex.txt");
    let fs_dir = &rt.config.fs_dir;
    if let Some(ref load_fn) = load_index {
        if let Ok(bytes) = std::fs::read(&fsindex_path) {
            let ptr = instance_malloc(&mut *store, instance, true, bytes.len())?;
            rt.memory.write_bytes(ptr, &bytes)?;
            let indexed = load_fn.call(&mut *store, (ptr, bytes.len() as i32))?;
            let _ = instance_free(&mut *store, instance, true, ptr);
            println!(
                "[VFS] Loaded {} (rbx_fs_load_index -> {indexed}, base={engine_base})",
                fsindex_path.display()
            );
        } else if fs_dir.is_dir() {
            let mut entries: Vec<(String, u64)> = Vec::new();
            collect_vfs_entries(fs_dir, fs_dir, &mut entries)?;
            entries.sort_by(|a, b| a.0.cmp(&b.0));

            let mut idx_buf = String::from("rbxfsindex 1\n");
            let mut prev = String::new();
            for (path, size) in &entries {
                let prefix_len = prev
                    .bytes()
                    .zip(path.bytes())
                    .take_while(|(a, b)| a == b)
                    .count();
                // Make sure prefix_len lands on UTF-8 char boundary
                let mut safe_prefix = prefix_len;
                while safe_prefix > 0 && !path.is_char_boundary(safe_prefix) {
                    safe_prefix -= 1;
                }
                let suffix = &path[safe_prefix..];
                idx_buf.push_str(&format!("{safe_prefix}\t{suffix}\t{size}\n"));
                prev = path.clone();
            }
            let bytes = idx_buf.as_bytes();
            let ptr = instance_malloc(&mut *store, instance, true, bytes.len())?;
            rt.memory.write_bytes(ptr, bytes)?;
            let indexed = load_fn.call(&mut *store, (ptr, bytes.len() as i32))?;
            let _ = instance_free(&mut *store, instance, true, ptr);
            println!(
                "[VFS] Indexed {} assets from {} (rbx_fs_load_index -> {indexed})",
                entries.len(),
                fs_dir.display()
            );
        } else {
            eprintln!("[VFS] Warning: {} does not exist", fs_dir.display());
        }
    }

    if let Some(ref add_fn) = add_file {
        let app_storage = serde_json::json!({
            "WebLogin": "2",
            "RobloxLocaleId": "en_us",
            "GameLocaleId": "en_us",
        })
        .to_string();
        for p in [
            "/appData/LocalStorage/appStorage.json",
            "/appData/LocalStorage/sitetest1.robloxlabs.com_appStorage.json",
            "/appData/LocalStorage/sitetest2.robloxlabs.com_appStorage.json",
            "/appData/LocalStorage/sitetest3.robloxlabs.com_appStorage.json",
        ] {
            vfs_add_bytes(store, instance, rt, add_fn, p, app_storage.as_bytes())?;
        }
    }

    Ok(())
}

fn collect_vfs_entries(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, u64)>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_vfs_entries(root, &path, out)?;
        } else if path.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let vfs_path = format!("/{}", rel.to_string_lossy().replace('\\', "/"));
            let size = entry.metadata()?.len();
            out.push((vfs_path, size));
        }
    }
    Ok(())
}

fn vfs_add_bytes(
    store: &mut Store<StoreState>,
    instance: wasmtime::Instance,
    rt: &Arc<RuntimeState>,
    add_fn: &wasmtime::TypedFunc<(i32, i32, i32), i32>,
    vfs_path: &str,
    bytes: &[u8],
) -> Result<()> {
    let p_ptr = instance_malloc(&mut *store, instance, true, vfs_path.len() + 1)?;
    rt.memory.write_cstr(p_ptr, vfs_path, vfs_path.len() + 1)?;
    let d_ptr = if bytes.is_empty() {
        0
    } else {
        let dp = instance_malloc(&mut *store, instance, true, bytes.len())?;
        rt.memory.write_bytes(dp, bytes)?;
        dp
    };
    let _ = add_fn.call(&mut *store, (p_ptr, d_ptr, bytes.len() as i32))?;
    let _ = instance_free(&mut *store, instance, true, p_ptr);
    if d_ptr != 0 {
        let _ = instance_free(&mut *store, instance, true, d_ptr);
    }
    Ok(())
}

fn init_embind_player(
    store: &mut Store<StoreState>,
    instance: wasmtime::Instance,
    rt: &Arc<RuntimeState>,
) -> Result<()> {
    let call_str = |store: &mut Store<StoreState>, name: &str, val: &str| {
        if has_function(rt, name) {
            if let Err(e) = call_function(store, instance, rt, name, &[EmbindArg::String(val.to_owned())]) {
                eprintln!("[Embind] {name} failed: {e}");
            }
        }
    };

    call_str(store, "set_roblox_base_url", &rt.config.roblox_base_url);
    if !rt.config.channel.is_empty() {
        call_str(store, "set_channel", &rt.config.channel);
    }
    call_str(store, "set_pretend_platform", "desktop");

    let mut app_settings = serde_json::Map::new();
    // 1. Fetch live flags like WasmPlayer.js Ue() (cached locally for fast restarts)
    let flags_cache_path = rt.config.opfs_dir.join("flags_cache.json");
    let live_flags_raw = match rt.http_backend.request(&crate::backends::HttpRequest {
        method: "GET".into(),
        url: "https://clientsettingscdn.roblox.com/v2/settings/application/WindowsWebClientApp"
            .into(),
        headers: Vec::new(),
        body: None,
        timeout_ms: 8000,
    }) {
        Ok(resp) if (200..300).contains(&resp.status) => {
            let _ = std::fs::create_dir_all(&rt.config.opfs_dir);
            let _ = std::fs::write(&flags_cache_path, &resp.body);
            Some(String::from_utf8_lossy(&resp.body).into_owned())
        }
        _ => std::fs::read_to_string(&flags_cache_path).ok(),
    };

    if let Some(raw) = live_flags_raw {
        if let Ok(serde_json::Value::Object(root)) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(serde_json::Value::Object(map)) = root.get("applicationSettings") {
                println!("[Flags] Loaded {} live flags from WindowsWebClientApp", map.len());
                app_settings.extend(map.clone());
            }
        }
    }

    let settings_path = rt.config.fs_dir.join("ClientSettings/ClientAppSettings.json");
    if let Ok(raw) = std::fs::read_to_string(&settings_path) {
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&raw) {
            app_settings.extend(map);
        }
    }
    let base_trimmed = rt.config.roblox_base_url.trim_end_matches('/');
    app_settings.insert(
        "BaseUrl".into(),
        serde_json::Value::String(base_trimmed.to_owned()),
    );
    app_settings.insert(
        "FFlagWasmPreferWebGPU".into(),
        serde_json::Value::String(if rt.config.enable_webgpu { "True" } else { "False" }.into()),
    );
    app_settings.insert(
        "FFlagHideCursorForNonMouseUsers".into(),
        serde_json::Value::String("False".into()),
    );
    for k in [
        "DFLogRbxTransportRnaExpConnection",
        "DFLogRbxTransportClientLog",
        "DFLogRbxTransportDummyClient",
        "DFLogRbxTransportWasmRna",
        "DFLogRbxTransportIntegration",
        "DFLogNetworkClient",
        "DFLogTransportParamsParser",
        "DFLogPlayerConfigurer",
        "FLogGameJoinUtil",
    ] {
        app_settings.insert(k.into(), serde_json::Value::String("12".into()));
    }

    let flags_json = serde_json::json!({
        "applicationSettings": app_settings
    })
    .to_string();
    call_str(store, "set_preloaded_flags", &flags_json);

    let ixp_cache = rt.config.opfs_dir.join("ixp_settings.json");
    if let Ok(ixp) = std::fs::read_to_string(ixp_cache) {
        if !ixp.is_empty() {
            call_str(store, "set_cached_ixp_settings", &ixp);
        }
    }

    let (rbx_locale, game_locale) = verify_auth_and_fetch_locales(rt);
    if has_function(rt, "set_account_locales") {
        let _ = call_function(
            &mut *store,
            instance,
            rt,
            "set_account_locales",
            &[
                EmbindArg::String(rbx_locale),
                EmbindArg::String(game_locale),
            ],
        );
    }

    if let Some(pid) = rt.config.place_id {
        if has_function(rt, "set_place_id") {
            let _ = call_function(
                &mut *store,
                instance,
                rt,
                "set_place_id",
                &[EmbindArg::BigInt(pid as i64)],
            );
        }
    }

    call_str(store, "set_browser_tracker_id", "");

    Ok(())
}

fn subdomain_url(base_url: &str, sub: &str, path: &str) -> String {
    let clean = base_url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let host = clean.split('/').next().unwrap_or("www.roblox.com");
    let new_host = if let Some(rest) = host.strip_prefix("www.") {
        format!("{sub}.{rest}")
    } else {
        format!("{sub}.{host}")
    };
    format!("https://{new_host}{path}")
}

fn verify_auth_and_fetch_locales(rt: &Arc<RuntimeState>) -> (String, String) {
    let Some(ref cookie) = rt.config.auth_cookie else {
        return ("en_us".into(), "en_us".into());
    };
    let clean = normalize_roblosecurity(cookie);
    if clean.is_empty() {
        return ("en_us".into(), "en_us".into());
    }
    let cookie_hdr = vec![
        ("Cookie".to_owned(), format!(".ROBLOSECURITY={clean}")),
        (
            "Origin".to_owned(),
            rt.config.roblox_base_url.trim_end_matches('/').to_owned(),
        ),
    ];

    let auth_url = subdomain_url(&rt.config.roblox_base_url, "users", "/v1/users/authenticated");
    match rt.http_backend.request(&HttpRequest {
        method: "GET".into(),
        url: auth_url,
        headers: cookie_hdr.clone(),
        body: None,
        timeout_ms: 6000,
    }) {
        Ok(resp) if (200..300).contains(&resp.status) => {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&resp.body) {
                let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
                let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("unknown");
                let disp = v
                    .get("displayName")
                    .and_then(|x| x.as_str())
                    .unwrap_or(name);
                println!("[Auth] Authenticated as {disp} (@{name}, userId={id})");
            }
        }
        Ok(resp) => {
            eprintln!(
                "[Auth] Warning: /v1/users/authenticated returned HTTP {} (cookie may be expired or invalid)",
                resp.status
            );
        }
        Err(e) => {
            eprintln!("[Auth] Warning: auth check failed: {e}");
        }
    }

    let locale_url = subdomain_url(
        &rt.config.roblox_base_url,
        "locale",
        "/v1/locales/user-localization-locus-supported-locales",
    );
    if let Ok(resp) = rt.http_backend.request(&HttpRequest {
        method: "GET".into(),
        url: locale_url,
        headers: cookie_hdr,
        body: None,
        timeout_ms: 5000,
    }) {
        if (200..300).contains(&resp.status) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&resp.body) {
                let rbx_loc = v
                    .get("generalExperience")
                    .and_then(|g| g.get("locale"))
                    .and_then(|l| l.as_str())
                    .unwrap_or("")
                    .trim();
                let ugc_loc = v
                    .get("ugc")
                    .and_then(|g| g.get("locale"))
                    .and_then(|l| l.as_str())
                    .unwrap_or("")
                    .trim();
                if !rbx_loc.is_empty() {
                    let game_loc = if ugc_loc.is_empty() { rbx_loc } else { ugc_loc };
                    println!("[Locale] Account locales: Roblox={rbx_loc}, UGC={game_loc}");
                    return (rbx_loc.to_owned(), game_loc.to_owned());
                }
            }
        }
    }

    ("en_us".into(), "en_us".into())
}

fn map_physical_key(pk: PhysicalKey) -> (String, i32) {
    let PhysicalKey::Code(kc) = pk else {
        return ("Unidentified".into(), 0);
    };
    let s = format!("{kc:?}");
    let dom_code = match kc {
        KeyCode::Escape => ("Escape", 27),
        KeyCode::Enter => ("Enter", 13),
        KeyCode::Tab => ("Tab", 9),
        KeyCode::Backspace => ("Backspace", 8),
        KeyCode::Space => ("Space", 32),
        KeyCode::CapsLock => ("CapsLock", 20),
        KeyCode::ShiftLeft => ("ShiftLeft", 16),
        KeyCode::ShiftRight => ("ShiftRight", 16),
        KeyCode::ControlLeft => ("ControlLeft", 17),
        KeyCode::ControlRight => ("ControlRight", 17),
        KeyCode::AltLeft => ("AltLeft", 18),
        KeyCode::AltRight => ("AltRight", 18),
        KeyCode::SuperLeft => ("MetaLeft", 91),
        KeyCode::SuperRight => ("MetaRight", 92),
        KeyCode::Insert => ("Insert", 45),
        KeyCode::Delete => ("Delete", 46),
        KeyCode::Home => ("Home", 36),
        KeyCode::End => ("End", 35),
        KeyCode::PageUp => ("PageUp", 33),
        KeyCode::PageDown => ("PageDown", 34),
        KeyCode::ArrowUp => ("ArrowUp", 38),
        KeyCode::ArrowDown => ("ArrowDown", 40),
        KeyCode::ArrowLeft => ("ArrowLeft", 37),
        KeyCode::ArrowRight => ("ArrowRight", 39),
        KeyCode::KeyA => ("KeyA", 65),
        KeyCode::KeyB => ("KeyB", 66),
        KeyCode::KeyC => ("KeyC", 67),
        KeyCode::KeyD => ("KeyD", 68),
        KeyCode::KeyE => ("KeyE", 69),
        KeyCode::KeyF => ("KeyF", 70),
        KeyCode::KeyG => ("KeyG", 71),
        KeyCode::KeyH => ("KeyH", 72),
        KeyCode::KeyI => ("KeyI", 73),
        KeyCode::KeyJ => ("KeyJ", 74),
        KeyCode::KeyK => ("KeyK", 75),
        KeyCode::KeyL => ("KeyL", 76),
        KeyCode::KeyM => ("KeyM", 77),
        KeyCode::KeyN => ("KeyN", 78),
        KeyCode::KeyO => ("KeyO", 79),
        KeyCode::KeyP => ("KeyP", 80),
        KeyCode::KeyQ => ("KeyQ", 81),
        KeyCode::KeyR => ("KeyR", 82),
        KeyCode::KeyS => ("KeyS", 83),
        KeyCode::KeyT => ("KeyT", 84),
        KeyCode::KeyU => ("KeyU", 85),
        KeyCode::KeyV => ("KeyV", 86),
        KeyCode::KeyW => ("KeyW", 87),
        KeyCode::KeyX => ("KeyX", 88),
        KeyCode::KeyY => ("KeyY", 89),
        KeyCode::KeyZ => ("KeyZ", 90),
        KeyCode::Digit0 => ("Digit0", 48),
        KeyCode::Digit1 => ("Digit1", 49),
        KeyCode::Digit2 => ("Digit2", 50),
        KeyCode::Digit3 => ("Digit3", 51),
        KeyCode::Digit4 => ("Digit4", 52),
        KeyCode::Digit5 => ("Digit5", 53),
        KeyCode::Digit6 => ("Digit6", 54),
        KeyCode::Digit7 => ("Digit7", 55),
        KeyCode::Digit8 => ("Digit8", 56),
        KeyCode::Digit9 => ("Digit9", 57),
        KeyCode::Minus => ("Minus", 189),
        KeyCode::Equal => ("Equal", 187),
        KeyCode::BracketLeft => ("BracketLeft", 219),
        KeyCode::BracketRight => ("BracketRight", 221),
        KeyCode::Backslash => ("Backslash", 220),
        KeyCode::Semicolon => ("Semicolon", 186),
        KeyCode::Quote => ("Quote", 222),
        KeyCode::Backquote => ("Backquote", 192),
        KeyCode::Comma => ("Comma", 188),
        KeyCode::Period => ("Period", 190),
        KeyCode::Slash => ("Slash", 191),
        KeyCode::F1 => ("F1", 112),
        KeyCode::F2 => ("F2", 113),
        KeyCode::F3 => ("F3", 114),
        KeyCode::F4 => ("F4", 115),
        KeyCode::F5 => ("F5", 116),
        KeyCode::F6 => ("F6", 117),
        KeyCode::F7 => ("F7", 118),
        KeyCode::F8 => ("F8", 119),
        KeyCode::F9 => ("F9", 120),
        KeyCode::F10 => ("F10", 121),
        KeyCode::F11 => ("F11", 122),
        KeyCode::F12 => ("F12", 123),
        KeyCode::Numpad0 => ("Numpad0", 96),
        KeyCode::Numpad1 => ("Numpad1", 97),
        KeyCode::Numpad2 => ("Numpad2", 98),
        KeyCode::Numpad3 => ("Numpad3", 99),
        KeyCode::Numpad4 => ("Numpad4", 100),
        KeyCode::Numpad5 => ("Numpad5", 101),
        KeyCode::Numpad6 => ("Numpad6", 102),
        KeyCode::Numpad7 => ("Numpad7", 103),
        KeyCode::Numpad8 => ("Numpad8", 104),
        KeyCode::Numpad9 => ("Numpad9", 105),
        KeyCode::NumpadMultiply => ("NumpadMultiply", 106),
        KeyCode::NumpadAdd => ("NumpadAdd", 107),
        KeyCode::NumpadSubtract => ("NumpadSubtract", 109),
        KeyCode::NumpadDecimal => ("NumpadDecimal", 110),
        KeyCode::NumpadDivide => ("NumpadDivide", 111),
        KeyCode::NumpadEnter => ("NumpadEnter", 13),
        _ => return (s, 0),
    };
    (dom_code.0.to_owned(), dom_code.1)
}
