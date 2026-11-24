use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use wasmtime::{AsContextMut, Caller, Error, Instance, Linker, Ref, Result, bail};

use crate::memory::{get_indirect_func, instance_malloc};
use crate::runtime::{
    EventListener, LiveRuntimeUnwind, MainThreadTask, PlatformInputEvent, RuntimeState, StoreState,
    WorkerTask,
};
use crate::wasi;

pub fn register(linker: &mut Linker<StoreState>) -> Result<()> {
    linker.func_wrap(
        "env",
        "emscripten_outn",
        |caller: Caller<'_, StoreState>, ptr: i32, len: i32| -> Result<()> {
            let s = caller
                .data()
                .runtime
                .memory
                .read_utf8(ptr, Some(len.max(0) as usize))?;
            println!("[WASM] {s}");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_errn",
        |caller: Caller<'_, StoreState>, ptr: i32, len: i32| -> Result<()> {
            let s = caller
                .data()
                .runtime
                .memory
                .read_utf8(ptr, Some(len.max(0) as usize))?;
            eprintln!("[WASM:ERR] {s}");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_console_log",
        |caller: Caller<'_, StoreState>, ptr: i32| -> Result<()> {
            let s = caller.data().runtime.memory.read_cstr(ptr)?;
            println!("[WASM:LOG] {s}");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_console_warn",
        |caller: Caller<'_, StoreState>, ptr: i32| -> Result<()> {
            let s = caller.data().runtime.memory.read_cstr(ptr)?;
            eprintln!("[WASM:WARN] {s}");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_console_error",
        |caller: Caller<'_, StoreState>, ptr: i32| -> Result<()> {
            let s = caller.data().runtime.memory.read_cstr(ptr)?;
            eprintln!("[WASM:ERROR] {s}");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "_emscripten_log_formatted",
        |caller: Caller<'_, StoreState>, flags: i32, ptr: i32| -> Result<()> {
            let s = caller.data().runtime.memory.read_cstr(ptr)?;
            if (flags & 6) != 0 {
                eprintln!("[WASM:LOG] {s}");
            } else {
                println!("[WASM:LOG] {s}");
            }
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "__assert_fail",
        |caller: Caller<'_, StoreState>,
         cond_ptr: i32,
         file_ptr: i32,
         line: i32,
         func_ptr: i32|
         -> Result<()> {
            let m = &caller.data().runtime.memory;
            let cond = m.read_cstr(cond_ptr).unwrap_or_default();
            let file = m.read_cstr(file_ptr).unwrap_or_else(|_| "unknown".into());
            let func = m.read_cstr(func_ptr).unwrap_or_else(|_| "unknown".into());
            bail!("Assertion failed: {cond}, at: {file}:{line} in {func}");
        },
    )?;

    linker.func_wrap("env", "_abort_js", || -> Result<()> {
        bail!("Aborted() via _abort_js");
    })?;

    linker.func_wrap(
        "env",
        "exit",
        |caller: Caller<'_, StoreState>, code: i32| -> Result<()> {
            wasi::proc_exit_impl(&caller.data().runtime, code)
        },
    )?;

    linker.func_wrap(
        "env",
        "schedule_main_loop",
        |caller: Caller<'_, StoreState>| {
            caller
                .data()
                .runtime
                .main_loop_scheduled
                .store(true, Ordering::SeqCst);
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_async_call",
        |mut caller: Caller<'_, StoreState>, fn_idx: i32, arg: i32, millis: i32| {
            let due = Instant::now() + Duration::from_millis(millis.max(0) as u64);
            if caller.data().is_main_thread {
                let rt = &caller.data().runtime;
                rt.keepalive_count.fetch_add(1, Ordering::SeqCst);
                rt.push_main_task(MainThreadTask::AsyncCall { fn_idx, arg, due });
            } else {
                let pthread_ptr = caller.data().pthread_ptr;
                let rt = caller.data().runtime.clone();
                caller.data_mut().keepalive += 1;
                rt.push_worker_task(pthread_ptr, WorkerTask::AsyncCall { fn_idx, arg, due });
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_runtime_keepalive_push",
        |mut caller: Caller<'_, StoreState>| {
            if caller.data().is_main_thread {
                caller
                    .data()
                    .runtime
                    .keepalive_count
                    .fetch_add(1, Ordering::SeqCst);
            } else {
                caller.data_mut().keepalive += 1;
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "_emscripten_runtime_keepalive_clear",
        |mut caller: Caller<'_, StoreState>| {
            if caller.data().is_main_thread {
                caller
                    .data()
                    .runtime
                    .keepalive_count
                    .store(0, Ordering::SeqCst);
            } else {
                caller.data_mut().keepalive = 0;
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_exit_with_live_runtime",
        |mut caller: Caller<'_, StoreState>| -> Result<()> {
            if caller.data().is_main_thread {
                caller
                    .data()
                    .runtime
                    .keepalive_count
                    .fetch_add(1, Ordering::SeqCst);
            } else {
                caller.data_mut().keepalive += 1;
            }
            Err(Error::new(LiveRuntimeUnwind))
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_unwind_to_js_event_loop",
        || -> Result<()> { Err(Error::new(LiveRuntimeUnwind)) },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_resize_heap",
        |caller: Caller<'_, StoreState>, requested_size: i32| -> i32 {
            i32::from(
                caller
                    .data()
                    .runtime
                    .memory
                    .grow_to(requested_size as u32),
            )
        },
    )?;

    linker.func_wrap("env", "emscripten_get_heap_max", || -> i32 {
        0xFFFF_0000_u32 as i32
    })?;

    linker.func_wrap(
        "env",
        "_msync_js",
        |_a: i32, _b: i32, _c: i32, _d: i32, _e: i32, _f: i64| -> i32 { 0 },
    )?;

    linker.func_wrap(
        "env",
        "__call_sighandler",
        |mut caller: Caller<'_, StoreState>, ptr: i32, sig: i32| -> Result<()> {
            let f = get_indirect_func(&mut caller, ptr)?;
            let typed = f.typed::<i32, ()>(&caller)?;
            typed.call(&mut caller, sig)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_get_canvas_element_size",
        |caller: Caller<'_, StoreState>, target: i32, w_ptr: i32, h_ptr: i32| -> Result<i32> {
            get_canvas_size_impl(&caller.data().runtime, target, w_ptr, h_ptr)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_set_canvas_element_size",
        |caller: Caller<'_, StoreState>, target: i32, w: i32, h: i32| -> i32 {
            set_canvas_size_impl(&caller.data().runtime, target, w, h)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_get_element_css_size",
        |caller: Caller<'_, StoreState>, target: i32, w_ptr: i32, h_ptr: i32| -> Result<i32> {
            get_css_size_impl(&caller.data().runtime, target, w_ptr, h_ptr)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_get_device_pixel_ratio",
        |caller: Caller<'_, StoreState>| -> f64 {
            caller.data().runtime.platform.lock().unwrap().device_pixel_ratio
        },
    )?;

    linker.func_wrap(
        "env",
        "get_effective_device_pixel_ratio",
        |caller: Caller<'_, StoreState>| -> f64 {
            caller.data().runtime.platform.lock().unwrap().device_pixel_ratio
        },
    )?;

    linker.func_wrap(
        "env",
        "applyPendingCanvasResize",
        |caller: Caller<'_, StoreState>, w_ptr: i32, h_ptr: i32| -> Result<i32> {
            let rt = &caller.data().runtime;
            let mut p = rt.platform.lock().unwrap();
            if p.pending_resize {
                p.pending_resize = false;
                let (w, h) = (p.width, p.height);
                drop(p);
                rt.webgpu_backend.resize_surface(w as u32, h as u32);
                rt.memory.write_i32(w_ptr, w)?;
                rt.memory.write_i32(h_ptr, h)?;
                Ok(1)
            } else {
                Ok(0)
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "applyPendingBrowserCanvasResize",
        |caller: Caller<'_, StoreState>| {
            let rt = &caller.data().runtime;
            let mut p = rt.platform.lock().unwrap();
            if p.pending_resize {
                p.pending_resize = false;
                let (w, h) = (p.width, p.height);
                drop(p);
                rt.webgpu_backend.resize_surface(w as u32, h as u32);
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_request_pointerlock",
        |caller: Caller<'_, StoreState>, target: i32, defer: i32| -> i32 {
            request_pointerlock_impl(&caller.data().runtime, target, defer)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_exit_pointerlock",
        |caller: Caller<'_, StoreState>| -> i32 { exit_pointerlock_impl(&caller.data().runtime) },
    )?;

    linker.func_wrap("env", "is_mobile_browser", || -> i32 { 0 })?;
    linker.func_wrap("env", "is_touch_mobile", || -> i32 { 0 })?;
    linker.func_wrap("env", "is_touch_screen_device", || -> i32 { 0 })?;
    linker.func_wrap("env", "has_keyboard", || -> i32 { 1 })?;
    linker.func_wrap("env", "has_mouse", || -> i32 { 1 })?;
    linker.func_wrap("env", "install_pointer_source_tracking", || {})?;
    linker.func_wrap(
        "env",
        "is_mouse_sourced_event",
        |caller: Caller<'_, StoreState>| -> i32 {
            let p = caller.data().runtime.platform.lock().unwrap();
            i32::from(p.last_pointer_type != "touch")
        },
    )?;
    linker.func_wrap(
        "env",
        "preallocate_emscripten_key_event_buffer",
        |caller: Caller<'_, StoreState>, ptr: i32| {
            let mut p = caller.data().runtime.platform.lock().unwrap();
            if p.key_event_buf == 0 {
                p.key_event_buf = ptr;
            }
        },
    )?;
    linker.func_wrap("env", "shift_canvas", |_dy: f32| {})?;

    macro_rules! reg_event_5 {
        ($name:literal, $ev_id:expr) => {
            linker.func_wrap(
                "env",
                $name,
                |caller: Caller<'_, StoreState>,
                 target: i32,
                 user_data: i32,
                 use_capture: i32,
                 cb: i32,
                 thread: i32|
                 -> i32 {
                    let resolved = resolve_target_thread(&caller, thread);
                    register_listener_impl(
                        &caller.data().runtime,
                        target,
                        $ev_id,
                        user_data,
                        use_capture != 0,
                        cb,
                        resolved,
                    )
                },
            )?;
        };
    }
    reg_event_5!("emscripten_set_keydown_callback_on_thread", 2);
    reg_event_5!("emscripten_set_keyup_callback_on_thread", 3);
    reg_event_5!("emscripten_set_mousedown_callback_on_thread", 5);
    reg_event_5!("emscripten_set_mouseup_callback_on_thread", 6);
    reg_event_5!("emscripten_set_mousemove_callback_on_thread", 8);
    reg_event_5!("emscripten_set_wheel_callback_on_thread", 9);
    reg_event_5!("emscripten_set_blur_callback_on_thread", 12);
    reg_event_5!("emscripten_set_focus_callback_on_thread", 13);
    reg_event_5!("emscripten_set_touchstart_callback_on_thread", 22);
    reg_event_5!("emscripten_set_touchend_callback_on_thread", 23);
    reg_event_5!("emscripten_set_touchmove_callback_on_thread", 24);
    reg_event_5!("emscripten_set_touchcancel_callback_on_thread", 25);
    reg_event_5!("emscripten_set_mouseenter_callback_on_thread", 33);
    reg_event_5!("emscripten_set_mouseleave_callback_on_thread", 34);
    reg_event_5!("emscripten_set_pointerlockerror_callback_on_thread", 38);

    linker.func_wrap("env", "emscripten_sample_gamepad_data", || -> i32 { 0 })?;
    linker.func_wrap("env", "emscripten_get_num_gamepads", || -> i32 { 0 })?;
    linker.func_wrap(
        "env",
        "emscripten_get_gamepad_status",
        |_idx: i32, _out_ptr: i32| -> i32 { -5 },
    )?;
    linker.func_wrap(
        "env",
        "emscripten_set_gamepadconnected_callback_on_thread",
        |caller: Caller<'_, StoreState>,
         user_data: i32,
         use_capture: i32,
         cb: i32,
         thread: i32|
         -> i32 {
            let resolved = resolve_target_thread(&caller, thread);
            register_listener_impl(
                &caller.data().runtime,
                2,
                26,
                user_data,
                use_capture != 0,
                cb,
                resolved,
            )
        },
    )?;
    linker.func_wrap(
        "env",
        "emscripten_set_gamepaddisconnected_callback_on_thread",
        |caller: Caller<'_, StoreState>,
         user_data: i32,
         use_capture: i32,
         cb: i32,
         thread: i32|
         -> i32 {
            let resolved = resolve_target_thread(&caller, thread);
            register_listener_impl(
                &caller.data().runtime,
                2,
                27,
                user_data,
                use_capture != 0,
                cb,
                resolved,
            )
        },
    )?;

    linker.func_wrap(
        "env",
        "js_request_keyboard_layout",
        |caller: Caller<'_, StoreState>| {
            caller
                .data()
                .runtime
                .platform
                .lock()
                .unwrap()
                .keyboard_layout_status = 1;
        },
    )?;
    linker.func_wrap(
        "env",
        "js_keyboard_layout_status",
        |caller: Caller<'_, StoreState>| -> i32 {
            caller
                .data()
                .runtime
                .platform
                .lock()
                .unwrap()
                .keyboard_layout_status
        },
    )?;
    linker.func_wrap(
        "env",
        "js_copy_keyboard_layout_key",
        |caller: Caller<'_, StoreState>, code_ptr: i32, out_ptr: i32, max_len: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            let code = rt.memory.read_cstr(code_ptr)?;
            let mapped = {
                let p = rt.platform.lock().unwrap();
                p.keyboard_layout_map
                    .get(&code)
                    .cloned()
                    .unwrap_or_default()
            };
            let upper = mapped.to_uppercase();
            let text = if upper.len() == mapped.len() { &upper } else { &mapped };
            rt.memory.write_cstr(out_ptr, text, max_len.max(0) as usize)?;
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "sync_text_and_pos",
        |caller: Caller<'_, StoreState>, text_ptr: i32, _start: i32, _end: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            let val = rt.memory.read_cstr(text_ptr)?;
            rt.platform.lock().unwrap().text_value = val;
            Ok(())
        },
    )?;
    linker.func_wrap(
        "env",
        "setup_font",
        |_a: i32, _b: i32, _c: i32, _d: f32, _e: i32, _f: i32| {},
    )?;
    linker.func_wrap("env", "set_text_box_horiz_alignment", |_align: i32| {})?;
    linker.func_wrap("env", "set_text_box_vert_alignment", |_align: i32| {})?;
    linker.func_wrap("env", "set_text_color", |_r: f32, _g: f32, _b: f32, _a: f32| {})?;
    linker.func_wrap("env", "blur_text_input", || {})?;
    linker.func_wrap("env", "reset_current_text_input", || {})?;
    linker.func_wrap("env", "focus_text_input", || {})?;
    linker.func_wrap("env", "set_text_box_overlay", |_overlay: i32| {})?;
    linker.func_wrap(
        "env",
        "update_text_box_rect",
        |_x: f32, _y: f32, _w: f32, _h: f32| {},
    )?;
    linker.func_wrap(
        "env",
        "choose_current_text_input",
        |caller: Caller<'_, StoreState>, kind_ptr: i32, _ml: i32, _b: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            let kind = rt.memory.read_cstr(kind_ptr)?;
            rt.platform.lock().unwrap().text_input_kind = kind;
            Ok(())
        },
    )?;
    linker.func_wrap(
        "env",
        "copy_pending_text",
        |caller: Caller<'_, StoreState>, dst_ptr: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            let bytes = rt.platform.lock().unwrap().pending_text.clone();
            if !bytes.is_empty() {
                rt.memory.write_bytes(dst_ptr, &bytes)?;
            }
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "read_host_info_int",
        |caller: Caller<'_, StoreState>, key_ptr: i32| -> Result<i32> {
            let rt = &caller.data().runtime;
            let key = rt.memory.read_cstr(key_ptr)?;
            let val = rt
                .config
                .host_info
                .get(&key)
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);
            println!("[HostInfo] int({key:?}) -> {val}");
            Ok(val)
        },
    )?;
    linker.func_wrap(
        "env",
        "read_host_info_string",
        |caller: Caller<'_, StoreState>, key_ptr: i32, out_ptr: i32, max_len: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            let key = rt.memory.read_cstr(key_ptr)?;
            let val = rt
                .config
                .host_info
                .get(&key)
                .map(String::as_str)
                .unwrap_or("");
            rt.memory.write_cstr(out_ptr, val, max_len.max(0) as usize)?;
            println!("[HostInfo] string({key:?}) -> {val:?}");
            Ok(())
        },
    )?;
    linker.func_wrap(
        "env",
        "js_web_client_page_visible",
        |caller: Caller<'_, StoreState>| -> i32 {
            i32::from(caller.data().runtime.platform.lock().unwrap().page_visible)
        },
    )?;
    linker.func_wrap(
        "env",
        "js_web_client_ms_since_navigation",
        |caller: Caller<'_, StoreState>| -> f64 { caller.data().runtime.ms_since_start() },
    )?;
    linker.func_wrap("env", "js_web_client_wasm_cache_state", || -> i32 { 0 })?;

    linker.func_wrap(
        "env",
        "js_linking_open_url",
        |caller: Caller<'_, StoreState>, url_ptr: i32, _req_id: i32| -> Result<i32> {
            let url = caller.data().runtime.memory.read_cstr(url_ptr)?;
            println!("[Linking] openURL requested: {url}");
            Ok(0)
        },
    )?;
    linker.func_wrap(
        "env",
        "js_external_content_sharing_deliver",
        |caller: Caller<'_, StoreState>, text_ptr: i32, _kind: i32| -> Result<()> {
            let text = caller.data().runtime.memory.read_cstr(text_ptr)?;
            println!("[Sharing] deliver: {text}");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "set_crash_attribute",
        |caller: Caller<'_, StoreState>, key_ptr: i32, val_ptr: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            let k = rt.memory.read_cstr(key_ptr)?;
            let v = rt.memory.read_cstr(val_ptr)?;
            rt.platform.lock().unwrap().crash_attributes.insert(k, v);
            Ok(())
        },
    )?;
    linker.func_wrap(
        "env",
        "report_crash",
        |caller: Caller<'_, StoreState>,
         err_type: i32,
         msg: i32,
         _stack: i32,
         lang: i32,
         thread: i32|
         -> Result<()> {
            let m = &caller.data().runtime.memory;
            let t = m.read_cstr(err_type).unwrap_or_default();
            let msg = m.read_cstr(msg).unwrap_or_default();
            let lang = m.read_cstr(lang).unwrap_or_default();
            let th = m.read_cstr(thread).unwrap_or_default();
            eprintln!("[CrashReporter] type={t} lang={lang} thread={th}: {msg}");
            Ok(())
        },
    )?;
    linker.func_wrap(
        "env",
        "RBX_setFullWasmStackTrace",
        |caller: Caller<'_, StoreState>| -> f64 {
            let mut p = caller.data().runtime.platform.lock().unwrap();
            let prev = p.wasm_stack_trace_limit;
            p.wasm_stack_trace_limit = f64::INFINITY;
            prev
        },
    )?;
    linker.func_wrap(
        "env",
        "RBX_restoreWasmStackTraceLimit",
        |caller: Caller<'_, StoreState>, prev: f64| {
            caller
                .data()
                .runtime
                .platform
                .lock()
                .unwrap()
                .wasm_stack_trace_limit = prev;
        },
    )?;
    linker.func_wrap("env", "emscripten_stack_snapshot", || -> i32 { 0 })?;
    linker.func_wrap(
        "env",
        "emscripten_stack_unwind_buffer",
        |_addr: i32, _buf: i32, _count: i32| -> i32 { 0 },
    )?;
    linker.func_wrap("env", "HaveOffsetConverter", || -> i32 { 0 })?;
    linker.func_wrap("env", "emscripten_pc_get_function", |_pc: i32| -> i32 { 0 })?;

    Ok(())
}

pub fn get_canvas_size_impl(
    rt: &RuntimeState,
    _target: i32,
    w_ptr: i32,
    h_ptr: i32,
) -> Result<i32> {
    let (w, h) = {
        let p = rt.platform.lock().unwrap();
        (p.width, p.height)
    };
    rt.memory.write_i32(w_ptr, w)?;
    rt.memory.write_i32(h_ptr, h)?;
    Ok(0)
}

pub fn set_canvas_size_impl(rt: &RuntimeState, _target: i32, w: i32, h: i32) -> i32 {
    let mut p = rt.platform.lock().unwrap();
    p.width = w.max(1);
    p.height = h.max(1);
    let (nw, nh) = (p.width, p.height);
    drop(p);
    rt.webgpu_backend.resize_surface(nw as u32, nh as u32);
    0
}

pub fn get_css_size_impl(
    rt: &RuntimeState,
    _target: i32,
    w_ptr: i32,
    h_ptr: i32,
) -> Result<i32> {
    let (w, h, dpr) = {
        let p = rt.platform.lock().unwrap();
        (p.width, p.height, p.device_pixel_ratio)
    };
    let dpr = if dpr > 0.0 { dpr } else { 1.0 };
    rt.memory.write_f64(w_ptr, (w as f64) / dpr)?;
    rt.memory.write_f64(h_ptr, (h as f64) / dpr)?;
    Ok(0)
}

pub fn request_pointerlock_impl(rt: &RuntimeState, _target: i32, _defer: i32) -> i32 {
    let mut p = rt.platform.lock().unwrap();
    if !p.pointer_locked {
        p.pointer_locked = true;
        p.pointer_lock_dirty = true;
    }
    0
}

pub fn exit_pointerlock_impl(rt: &RuntimeState) -> i32 {
    let mut p = rt.platform.lock().unwrap();
    if p.pointer_locked {
        p.pointer_locked = false;
        p.pointer_lock_dirty = true;
    }
    0
}

pub fn resolve_target_thread(caller: &Caller<'_, StoreState>, raw_target_thread: i32) -> i32 {
    match raw_target_thread {
        1 => 0,
        2 => {
            let proxied = caller.data().proxied_caller_thread;
            if proxied != 0 {
                proxied
            } else if caller.data().is_main_thread {
                0
            } else {
                caller.data().pthread_ptr
            }
        }
        other => other,
    }
}

pub fn register_listener_impl(
    rt: &RuntimeState,
    target: i32,
    event_type_id: i32,
    user_data: i32,
    use_capture: bool,
    callback: i32,
    target_thread: i32,
) -> i32 {
    let mut p = rt.platform.lock().unwrap();
    if callback != 0 {
        p.listeners.push(EventListener {
            target,
            event_type_id,
            user_data,
            callback,
            use_capture,
            target_thread,
        });
    } else {
        p.listeners
            .retain(|l| !(l.target == target && l.event_type_id == event_type_id));
    }
    0
}

pub fn dispatch_input_event(
    mut store: impl AsContextMut<Data = StoreState>,
    inst: Instance,
    ev: PlatformInputEvent,
) -> Result<()> {
    let rt = store.as_context().data().runtime.clone();
    let ts = rt.ms_since_start();

    let event_type_id = match &ev {
        PlatformInputEvent::Key { is_down, .. } => {
            if *is_down { 2 } else { 3 }
        }
        PlatformInputEvent::MouseButton { is_down, .. } => {
            if *is_down { 5 } else { 6 }
        }
        PlatformInputEvent::MouseMove { .. } => 8,
        PlatformInputEvent::MouseWheel { .. } => 9,
        PlatformInputEvent::Focus { focused } => {
            if *focused { 13 } else { 12 }
        }
    };

    let listeners: Vec<EventListener> = {
        let p = rt.platform.lock().unwrap();
        p.listeners
            .iter()
            .filter(|l| l.event_type_id == event_type_id)
            .cloned()
            .collect()
    };
    if listeners.is_empty() {
        return Ok(());
    }

    let (ev_ptr, ev_size) = match ev {
        PlatformInputEvent::Key {
            repeat,
            key,
            code,
            key_code,
            char_code,
            location,
            ctrl,
            shift,
            alt,
            meta,
            ..
        } => {
            let buf = ensure_event_buf(&mut store, inst, &rt, EventBufKind::Key, 160)?;
            if buf == 0 {
                return Ok(());
            }
            let m = &rt.memory;
            m.write_f64(buf, ts)?;
            m.write_i32(buf + 8, location)?;
            m.write_u8(buf + 12, u8::from(ctrl))?;
            m.write_u8(buf + 13, u8::from(shift))?;
            m.write_u8(buf + 14, u8::from(alt))?;
            m.write_u8(buf + 15, u8::from(meta))?;
            m.write_u8(buf + 16, u8::from(repeat))?;
            m.write_i32(buf + 20, char_code)?;
            m.write_i32(buf + 24, key_code)?;
            m.write_i32(buf + 28, key_code)?;
            m.write_cstr(buf + 32, &key, 32)?;
            m.write_cstr(buf + 64, &code, 32)?;
            m.write_cstr(buf + 96, "", 32)?;
            m.write_cstr(buf + 128, "en-US", 32)?;
            (buf, 160)
        }
        PlatformInputEvent::MouseButton {
            button,
            buttons,
            x,
            y,
            ctrl,
            shift,
            alt,
            meta,
            ..
        } => {
            let buf = ensure_event_buf(&mut store, inst, &rt, EventBufKind::Mouse, 64)?;
            if buf == 0 {
                return Ok(());
            }
            write_mouse_event(&rt, buf, ts, x, y, 0, 0, button, buttons, ctrl, shift, alt, meta)?;
            (buf, 64)
        }
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
        } => {
            let buf = ensure_event_buf(&mut store, inst, &rt, EventBufKind::Mouse, 64)?;
            if buf == 0 {
                return Ok(());
            }
            write_mouse_event(&rt, buf, ts, x, y, dx, dy, 0, buttons, ctrl, shift, alt, meta)?;
            (buf, 64)
        }
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
        } => {
            let buf = ensure_event_buf(&mut store, inst, &rt, EventBufKind::Wheel, 96)?;
            if buf == 0 {
                return Ok(());
            }
            write_mouse_event(&rt, buf, ts, x, y, 0, 0, 0, buttons, ctrl, shift, alt, meta)?;
            let m = &rt.memory;
            m.write_f64(buf + 64, delta_x)?;
            m.write_f64(buf + 72, delta_y)?;
            m.write_f64(buf + 80, 0.0)?;
            m.write_i32(buf + 88, delta_mode)?;
            (buf, 96)
        }
        PlatformInputEvent::Focus { .. } => {
            let buf = ensure_event_buf(&mut store, inst, &rt, EventBufKind::Focus, 256)?;
            if buf == 0 {
                return Ok(());
            }
            rt.memory.write_cstr(buf, "CANVAS", 128)?;
            rt.memory.write_cstr(buf + 128, "canvas", 128)?;
            (buf, 256)
        }
    };

    let main_ptr = rt.main_pthread_ptr.load(Ordering::SeqCst);
    for l in listeners {
        if l.target_thread == 0 || l.target_thread == main_ptr {
            if let Some(table) = inst.get_table(&mut store, "__indirect_function_table") {
                if let Some(Ref::Func(Some(func))) =
                    table.get(&mut store, l.callback as u32 as u64)
                {
                    if let Ok(typed) = func.typed::<(i32, i32, i32), i32>(&store) {
                        let _ = typed.call(&mut store, (event_type_id, ev_ptr, l.user_data));
                    }
                }
            }
        } else if let Ok(run_cb) = inst
            .get_typed_func::<(i32, i32, i32, i32, i32, i32), ()>(
                &mut store,
                "_emscripten_run_callback_on_thread",
            )
        {
            let _ = run_cb.call(
                &mut store,
                (
                    l.target_thread,
                    l.callback,
                    event_type_id,
                    ev_ptr,
                    ev_size,
                    l.user_data,
                ),
            );
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_mouse_event(
    rt: &RuntimeState,
    buf: i32,
    ts: f64,
    x: i32,
    y: i32,
    dx: i32,
    dy: i32,
    button: i16,
    buttons: u16,
    ctrl: bool,
    shift: bool,
    alt: bool,
    meta: bool,
) -> Result<()> {
    let m = &rt.memory;
    m.write_f64(buf, ts)?;
    m.write_i32(buf + 8, x)?;
    m.write_i32(buf + 12, y)?;
    m.write_i32(buf + 16, x)?;
    m.write_i32(buf + 20, y)?;
    m.write_u8(buf + 24, u8::from(ctrl))?;
    m.write_u8(buf + 25, u8::from(shift))?;
    m.write_u8(buf + 26, u8::from(alt))?;
    m.write_u8(buf + 27, u8::from(meta))?;
    m.write_u16(buf + 28, button as u16)?;
    m.write_u16(buf + 30, buttons)?;
    m.write_i32(buf + 32, dx)?;
    m.write_i32(buf + 36, dy)?;
    m.write_i32(buf + 40, x)?;
    m.write_i32(buf + 44, y)?;
    m.write_i32(buf + 48, x)?;
    m.write_i32(buf + 52, y)?;
    m.write_i32(buf + 56, 0)?;
    Ok(())
}

enum EventBufKind {
    Key,
    Mouse,
    Wheel,
    Focus,
}

fn ensure_event_buf(
    mut store: impl AsContextMut<Data = StoreState>,
    inst: Instance,
    rt: &RuntimeState,
    kind: EventBufKind,
    size: usize,
) -> Result<i32> {
    {
        let p = rt.platform.lock().unwrap();
        let existing = match kind {
            EventBufKind::Key => p.key_event_buf,
            EventBufKind::Mouse => p.mouse_event_buf,
            EventBufKind::Wheel => p.wheel_event_buf,
            EventBufKind::Focus => p.focus_event_buf,
        };
        if existing != 0 {
            return Ok(existing);
        }
    }
    let ptr = instance_malloc(&mut store, inst, true, size)?;
    if ptr != 0 {
        let _ = rt.memory.write_bytes(ptr, &vec![0u8; size]);
        let mut p = rt.platform.lock().unwrap();
        match kind {
            EventBufKind::Key => p.key_event_buf = ptr,
            EventBufKind::Mouse => p.mouse_event_buf = ptr,
            EventBufKind::Wheel => p.wheel_event_buf = ptr,
            EventBufKind::Focus => p.focus_event_buf = ptr,
        }
    }
    Ok(ptr)
}
