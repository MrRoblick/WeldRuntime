use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use wasmtime::{AsContextMut, Caller, Instance, Linker, Ref, Result, Store, WaitResult, format_err};

use crate::audio::AudioMixMode;
use crate::em_asm::{self, EmAsmArg};
use crate::embind;
use crate::memory::{get_caller_instance, get_export_func, instance_free, instance_malloc};
use crate::network;
use crate::platform;
use crate::rna;
use crate::runtime::{
    ExitStatus, LiveRuntimeUnwind, MainThreadTask, RuntimeState, StoreState, WorkerTask,
};
use crate::wasi;

pub fn register(linker: &mut Linker<StoreState>) -> Result<()> {
    linker.func_wrap(
        "env",
        "_emscripten_init_main_thread_js",
        |mut caller: Caller<'_, StoreState>, pthread_ptr: i32| -> Result<()> {
            let rt = caller.data().runtime.clone();
            rt.main_pthread_ptr.store(pthread_ptr, Ordering::SeqCst);
            caller.data_mut().is_main_thread = true;
            caller.data_mut().pthread_ptr = pthread_ptr;

            let thread_init = get_export_func(&mut caller, "_emscripten_thread_init")?
                .typed::<(i32, i32, i32, i32, i32, i32), ()>(&caller)?;
            thread_init.call(&mut caller, (pthread_ptr, 1, 1, 1, 262144, 0))?;

            let tls_init = get_export_func(&mut caller, "_emscripten_tls_init")?
                .typed::<(), i32>(&caller)?;
            let tls_base = tls_init.call(&mut caller, ())?;
            if pthread_ptr != 0 && tls_base != 0 {
                rt.pthread_tls.lock().unwrap().insert(pthread_ptr, tls_base);
            }
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "__pthread_create_js",
        |caller: Caller<'_, StoreState>,
         pthread_ptr: i32,
         attr: i32,
         start_routine: i32,
         arg: i32|
         -> Result<i32> {
            pthread_create_impl(&caller.data().runtime, pthread_ptr, attr, start_routine, arg)
        },
    )?;

    linker.func_wrap(
        "env",
        "_emscripten_thread_cleanup",
        |caller: Caller<'_, StoreState>, pthread_ptr: i32| {
            let rt = &caller.data().runtime;
            rt.thread_mailboxes.lock().unwrap().remove(&pthread_ptr);
            rt.pthread_tls.lock().unwrap().remove(&pthread_ptr);
            rt.push_main_task(MainThreadTask::FreeThreadData(pthread_ptr));
        },
    )?;

    linker.func_wrap(
        "env",
        "_emscripten_thread_set_strongref",
        |_pthread_ptr: i32| {},
    )?;

    linker.func_wrap(
        "env",
        "_emscripten_thread_mailbox_await",
        |_caller: Caller<'_, StoreState>, _pthread_ptr: i32| -> Result<()> {
            // Keep waiting_async (pthread_ptr + 112) == 0 so Emscripten's C runtime
            // deterministically invokes `_emscripten_notify_mailbox_postmessage` on every post.
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "_emscripten_notify_mailbox_postmessage",
        |caller: Caller<'_, StoreState>, target_thread: i32, _curr_thread: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            let main_ptr = rt.main_pthread_ptr.load(Ordering::SeqCst);
            if target_thread == 0 || target_thread == main_ptr {
                rt.push_main_task(MainThreadTask::CheckMailbox);
            } else {
                rt.push_worker_task(target_thread, WorkerTask::CheckMailbox);
                let _ = rt.memory.atomic_notify(target_thread, 1);
            }
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "_emscripten_receive_on_main_thread_js",
        |mut caller: Caller<'_, StoreState>,
         func_id: i32,
         em_asm_addr: i32,
         caller_thread: i32,
         arg_bytes: i32,
         args_ptr: i32,
         done_ctx: i32,
         done_flag: i32|
         -> Result<f64> {
            receive_on_main_thread_impl(
                &mut caller,
                func_id,
                em_asm_addr,
                caller_thread,
                arg_bytes,
                args_ptr,
                done_ctx,
                done_flag,
            )
        },
    )?;

    linker.func_wrap(
        "env",
        "__asyncjs__rbxFutexWaitMainJspi",
        |mut caller: Caller<'_, StoreState>,
         addr: i32,
         expected: i32,
         timeout_ms: f64|
         -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let inst = get_caller_instance(&mut caller)?;
            let deadline = if timeout_ms.is_finite() && timeout_ms >= 0.0 {
                Some(Instant::now() + Duration::from_secs_f64(timeout_ms / 1000.0))
            } else {
                None
            };

            loop {
                pump_main_tasks(&mut caller, inst)?;

                let slice_timeout = match deadline {
                    Some(dl) => {
                        let now = Instant::now();
                        if now >= dl {
                            return Ok(2);
                        }
                        (dl - now).min(Duration::from_millis(2))
                    }
                    None => Duration::from_millis(2),
                };

                match rt.memory.atomic_wait32(addr, expected, Some(slice_timeout))? {
                    WaitResult::Ok => return Ok(0),
                    WaitResult::Mismatch => return Ok(1),
                    WaitResult::TimedOut => {
                        if let Some(dl) = deadline {
                            if Instant::now() >= dl {
                                return Ok(2);
                            }
                        }
                        if rt.should_exit.load(Ordering::SeqCst) {
                            return Ok(2);
                        }
                    }
                }
            }
        },
    )?;

    linker.func_wrap("env", "emscripten_check_blocking_allowed", || {})?;

    linker.func_wrap("env", "emscripten_num_logical_cores", || -> i32 {
        thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(4)
    })?;

    linker.func_wrap("env", "get_hardware_concurrency", || -> i32 {
        thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(4)
    })?;

    Ok(())
}

pub fn pthread_create_impl(
    rt: &Arc<RuntimeState>,
    pthread_ptr: i32,
    _attr: i32,
    start_routine: i32,
    arg: i32,
) -> Result<i32> {
    let Some(instance_pre) = rt.instance_pre.get().cloned() else {
        return Ok(6); // EAGAIN
    };
    let rt_child = rt.clone();
    let (tx, rx) = mpsc::channel();
    rt.thread_mailboxes.lock().unwrap().insert(pthread_ptr, tx);

    let spawn_res = thread::Builder::new()
        .name(format!("rbx-pthread-{pthread_ptr:#x}"))
        .spawn(move || {
            let mut store = Store::new(
                &rt_child.engine,
                StoreState::worker_thread(rt_child.clone(), pthread_ptr),
            );
            let instance = match instance_pre.instantiate(&mut store) {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("[PThread {pthread_ptr:#x}] instantiate failed: {e:?}");
                    rt_child.thread_mailboxes.lock().unwrap().remove(&pthread_ptr);
                    return;
                }
            };
            store.data_mut().instance = Some(instance);

            if let (Ok(stack_base), Ok(stack_size)) = (
                rt_child.memory.read_u32(pthread_ptr + 48),
                rt_child.memory.read_u32(pthread_ptr + 52),
            ) {
                let stack_end = stack_base.wrapping_sub(stack_size);
                if let Ok(set_limits) = instance
                    .get_typed_func::<(i32, i32), ()>(&mut store, "emscripten_stack_set_limits")
                {
                    let _ = set_limits.call(&mut store, (stack_base as i32, stack_end as i32));
                }
                if let Ok(restore) =
                    instance.get_typed_func::<i32, ()>(&mut store, "_emscripten_stack_restore")
                {
                    let _ = restore.call(&mut store, stack_base as i32);
                }
            }

            if let Ok(init_fn) = instance.get_typed_func::<(i32, i32, i32, i32, i32, i32), ()>(
                &mut store,
                "_emscripten_thread_init",
            ) {
                let _ = init_fn.call(&mut store, (pthread_ptr, 0, 0, 1, 0, 0));
            }
            if let Ok(tls_init) =
                instance.get_typed_func::<(), i32>(&mut store, "_emscripten_tls_init")
            {
                if let Ok(tls_base) = tls_init.call(&mut store, ()) {
                    if pthread_ptr != 0 && tls_base != 0 {
                        rt_child
                            .pthread_tls
                            .lock()
                            .unwrap()
                            .insert(pthread_ptr, tls_base);
                    }
                }
            }

            if let Ok(embind_init) =
                instance.get_typed_func::<(), ()>(&mut store, "_embind_initialize_bindings")
            {
                let _ = embind_init.call(&mut store, ());
            }

            let routine_res = (|| -> Result<i32> {
                let table = instance
                    .get_table(&mut store, "__indirect_function_table")
                    .ok_or_else(|| format_err!("missing __indirect_function_table"))?;
                let elem = table
                    .get(&mut store, start_routine as u32 as u64)
                    .ok_or_else(|| format_err!("start_routine {start_routine} out of bounds"))?;
                let Ref::Func(Some(func)) = elem else {
                    return Err(format_err!("start_routine {start_routine} is null"));
                };
                let typed = func.typed::<i32, i32>(&store)?;
                typed.call(&mut store, arg)
            })();

            match routine_res {
                Ok(exit_val) => {
                    if store.data().keepalive > 0 {
                        run_worker_event_loop(
                            &mut store,
                            instance,
                            &rt_child,
                            pthread_ptr,
                            &rx,
                            Some(exit_val),
                        );
                    } else if let Ok(exit_fn) = instance
                        .get_typed_func::<i32, ()>(&mut store, "_emscripten_thread_exit")
                    {
                        let _ = exit_fn.call(&mut store, exit_val);
                    }
                }
                Err(e) => {
                    if e.downcast_ref::<LiveRuntimeUnwind>().is_some() {
                        run_worker_event_loop(
                            &mut store,
                            instance,
                            &rt_child,
                            pthread_ptr,
                            &rx,
                            None,
                        );
                    } else if e.downcast_ref::<ExitStatus>().is_some() {
                        // Process exit triggered from worker
                    } else {
                        eprintln!("[PThread {pthread_ptr:#x}] crashed: {e:?}");
                        if let Ok(crash_fn) = instance
                            .get_typed_func::<(), ()>(&mut store, "_emscripten_thread_crashed")
                        {
                            let _ = crash_fn.call(&mut store, ());
                        }
                    }
                }
            }
            rt_child.thread_mailboxes.lock().unwrap().remove(&pthread_ptr);
        });

    match spawn_res {
        Ok(_) => Ok(0),
        Err(e) => {
            rt.thread_mailboxes.lock().unwrap().remove(&pthread_ptr);
            eprintln!("[PThread] spawn error: {e}");
            Ok(6)
        }
    }
}

fn run_worker_event_loop(
    store: &mut Store<StoreState>,
    instance: Instance,
    rt: &Arc<RuntimeState>,
    pthread_ptr: i32,
    rx: &mpsc::Receiver<WorkerTask>,
    deferred_exit_val: Option<i32>,
) {
    let check_mb = instance
        .get_typed_func::<(), ()>(&mut *store, "_emscripten_check_mailbox")
        .ok();
    let mut delayed: VecDeque<WorkerTask> = VecDeque::new();

    while !rt.should_exit.load(Ordering::SeqCst) {
        let now = Instant::now();
        let next_due = delayed
            .iter()
            .filter_map(|t| match t {
                WorkerTask::AsyncCall { due, .. } => Some(*due),
                _ => None,
            })
            .min();

        let wait_dur = match next_due {
            Some(due) if due <= now => Duration::ZERO,
            Some(due) => (due - now).min(Duration::from_millis(10)),
            None => Duration::from_millis(10),
        };

        let mut batch = Vec::new();
        if wait_dur.is_zero() {
            while let Ok(t) = rx.try_recv() {
                batch.push(t);
            }
        } else {
            match rx.recv_timeout(wait_dur) {
                Ok(first) => {
                    batch.push(first);
                    while let Ok(t) = rx.try_recv() {
                        batch.push(t);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        let now = Instant::now();
        let mut prev_delayed = std::mem::take(&mut delayed);
        while let Some(t) = prev_delayed.pop_front() {
            match &t {
                WorkerTask::AsyncCall { due, .. } if *due > now => delayed.push_back(t),
                _ => batch.push(t),
            }
        }

        let mut checked_mb = false;
        for task in batch {
            match task {
                WorkerTask::CheckMailbox => {
                    if !checked_mb {
                        checked_mb = true;
                        if let Some(ref f) = check_mb {
                            if let Err(err) = f.call(&mut *store, ()) {
                                if err.downcast_ref::<LiveRuntimeUnwind>().is_none()
                                    && err.downcast_ref::<ExitStatus>().is_none()
                                {
                                    eprintln!("[PThread {pthread_ptr:#x}] mailbox err: {err:?}");
                                    return;
                                }
                            }
                        }
                    }
                }
                WorkerTask::AsyncCall { fn_idx, arg, due } => {
                    if due > Instant::now() {
                        delayed.push_back(WorkerTask::AsyncCall { fn_idx, arg, due });
                    } else {
                        store.data_mut().keepalive = (store.data().keepalive - 1).max(0);
                        call_indirect_i32_void(&mut *store, instance, fn_idx, arg);
                    }
                }
                WorkerTask::FetchComplete(done) => {
                    let _ = network::deliver_completed_fetch(
                        &mut *store,
                        instance,
                        rt,
                        false,
                        done,
                    );
                }
                WorkerTask::RnaEvent(ev) => {
                    if let Err(err) = rna::dispatch_rna_event(&mut *store, instance, rt, ev) {
                        if err.downcast_ref::<LiveRuntimeUnwind>().is_none()
                            && err.downcast_ref::<ExitStatus>().is_none()
                        {
                            eprintln!("[PThread {pthread_ptr:#x}] rna callback err: {err:?}");
                        }
                    }
                }
            }
        }

        if store.data().keepalive <= 0 && delayed.is_empty() {
            if let Some(code) = deferred_exit_val {
                if let Ok(exit_fn) =
                    instance.get_typed_func::<i32, ()>(&mut *store, "_emscripten_thread_exit")
                {
                    let _ = exit_fn.call(&mut *store, code);
                }
            }
            break;
        }
    }
}

pub fn pump_main_tasks(
    mut store: impl AsContextMut<Data = StoreState>,
    inst: Instance,
) -> Result<()> {
    let rt = store.as_context().data().runtime.clone();
    let tasks: Vec<MainThreadTask> = {
        let mut q = rt.main_queue.lock().unwrap();
        let mut remaining = VecDeque::new();
        let mut ready = Vec::new();
        let now = Instant::now();
        while let Some(t) = q.pop_front() {
            match &t {
                MainThreadTask::AsyncCall { due, .. } if *due > now => remaining.push_back(t),
                _ => ready.push(t),
            }
        }
        *q = remaining;
        ready
    };

    let mut checked_mailbox = false;
    for task in tasks {
        match task {
            MainThreadTask::CheckMailbox => {
                if !checked_mailbox {
                    checked_mailbox = true;
                    if let Ok(f) =
                        inst.get_typed_func::<(), ()>(&mut store, "_emscripten_check_mailbox")
                    {
                        let _ = f.call(&mut store, ());
                    }
                }
            }
            MainThreadTask::AsyncCall { fn_idx, arg, .. } => {
                let prev = rt.keepalive_count.load(Ordering::SeqCst);
                if prev > 0 {
                    rt.keepalive_count.fetch_sub(1, Ordering::SeqCst);
                }
                call_indirect_i32_void(&mut store, inst, fn_idx, arg);
            }
            MainThreadTask::FreeThreadData(pthread_ptr) => {
                if let Ok(f) =
                    inst.get_typed_func::<i32, ()>(&mut store, "_emscripten_thread_free_data")
                {
                    let _ = f.call(&mut store, pthread_ptr);
                }
            }
            MainThreadTask::FetchComplete(done) => {
                network::deliver_completed_fetch(&mut store, inst, &rt, true, done)?;
            }
            MainThreadTask::WebSocketEvent {
                ws_id,
                kind,
                data,
                is_text,
                was_clean,
                close_code,
                close_reason,
            } => {
                dispatch_ws_event(
                    &mut store,
                    inst,
                    &rt,
                    ws_id,
                    kind,
                    &data,
                    is_text,
                    was_clean,
                    close_code,
                    &close_reason,
                )?;
            }
            MainThreadTask::RnaEvent(ev) => {
                rna::dispatch_rna_event(&mut store, inst, &rt, ev)?;
            }
            MainThreadTask::InputEvent(ev) => {
                let is_user_gesture = matches!(
                    ev,
                    crate::runtime::PlatformInputEvent::Key { is_down: true, .. }
                        | crate::runtime::PlatformInputEvent::MouseButton { is_down: true, .. }
                );
                platform::dispatch_input_event(&mut store, inst, ev)?;
                if is_user_gesture
                    && !rt.audio_resumed.swap(true, Ordering::SeqCst)
                    && embind::has_function(&rt, "resume_audio")
                {
                    let _ = embind::call_function(&mut store, inst, &rt, "resume_audio", &[]);
                }
            }
            MainThreadTask::AudioMix(mode) => match mode {
                AudioMixMode::ScriptProcessor { frames } => {
                    if let Ok(mix_fn) =
                        inst.get_typed_func::<i32, ()>(&mut store, "FMOD_JS_MixFunction")
                    {
                        let _ = mix_fn.call(&mut store, frames);
                        rt.audio_backend.script_commit_mix_block();
                    }
                }
                AudioMixMode::WorkletSlowpath => {
                    if let Ok(slow_fn) =
                        inst.get_typed_func::<(), ()>(&mut store, "FMOD_JS_MixerSlowpathFunction")
                    {
                        let _ = slow_fn.call(&mut store, ());
                    }
                }
            },
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn dispatch_ws_event(
    mut store: impl AsContextMut<Data = StoreState>,
    inst: Instance,
    rt: &Arc<RuntimeState>,
    ws_id: i32,
    kind: u8,
    data: &[u8],
    is_text: bool,
    was_clean: bool,
    close_code: u16,
    close_reason: &str,
) -> Result<()> {
    let entry_opt = {
        let mut map = rt.websockets.lock().unwrap();
        if let Some(e) = map.get_mut(&ws_id) {
            if kind == 0 {
                e.connected = true;
            }
            let cb = match kind {
                0 => e.on_open,
                1 => e.on_message,
                2 => e.on_error,
                _ => e.on_close,
            };
            Some((cb, e.event_ptr))
        } else {
            None
        }
    };
    let Some((Some((callback_fn, user_data, _target_thread)), event_ptr)) = entry_opt else {
        return Ok(());
    };

    let mut temp_data_ptr = 0;
    match kind {
        0 | 2 => {}
        1 => {
            let alloc_len = data.len() + usize::from(is_text);
            let p = instance_malloc(&mut store, inst, true, alloc_len.max(1))?;
            if p != 0 && !data.is_empty() {
                rt.memory.write_bytes(p, data)?;
            }
            if p != 0 && is_text {
                rt.memory.write_u8(p + data.len() as i32, 0)?;
            }
            temp_data_ptr = p;
            rt.memory.write_i32(event_ptr + 4, p)?;
            rt.memory.write_u32(event_ptr + 8, alloc_len as u32)?;
            rt.memory.write_u8(event_ptr + 12, u8::from(is_text))?;
        }
        _ => {
            rt.memory.write_u8(event_ptr + 4, u8::from(was_clean))?;
            rt.memory.write_u16(event_ptr + 6, close_code)?;
            rt.memory.write_cstr(event_ptr + 8, close_reason, 512)?;
        }
    }

    if let Some(table) = inst.get_table(&mut store, "__indirect_function_table") {
        if let Some(Ref::Func(Some(func))) = table.get(&mut store, callback_fn as u32 as u64) {
            if let Ok(typed) = func.typed::<(i32, i32, i32), i32>(&store) {
                let _ = typed.call(&mut store, (0, event_ptr, user_data));
            }
        }
    }

    if temp_data_ptr != 0 {
        let _ = instance_free(&mut store, inst, true, temp_data_ptr);
    }
    Ok(())
}

fn call_indirect_i32_void(
    mut store: impl AsContextMut<Data = StoreState>,
    inst: Instance,
    fn_idx: i32,
    arg: i32,
) {
    if let Some(table) = inst.get_table(&mut store, "__indirect_function_table") {
        if let Some(Ref::Func(Some(func))) = table.get(&mut store, fn_idx as u32 as u64) {
            if let Ok(typed) = func.typed::<i32, ()>(&store) {
                let _ = typed.call(&mut store, arg);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn receive_on_main_thread_impl(
    caller: &mut Caller<'_, StoreState>,
    func_id: i32,
    em_asm_addr: i32,
    caller_thread: i32,
    arg_bytes: i32,
    args_ptr: i32,
    done_ctx: i32,
    done_flag: i32,
) -> Result<f64> {
    let rt = caller.data().runtime.clone();
    let count = (arg_bytes.max(0) as usize) / 16;
    let mut args = Vec::with_capacity(count);
    for i in 0..count {
        let base = args_ptr.wrapping_add((i * 16) as i32);
        let is_int = rt.memory.read_i64(base)? != 0;
        if is_int {
            args.push(EmAsmArg::Int(rt.memory.read_i64(base + 8)?));
        } else {
            args.push(EmAsmArg::Float(rt.memory.read_f64(base + 8)?));
        }
    }

    caller.data_mut().proxied_caller_thread = caller_thread;
    let res = if em_asm_addr != 0 {
        em_asm::dispatch_em_asm(caller, em_asm_addr, &args)?
    } else {
        dispatch_proxied_table(caller, func_id, &args)?
    };
    caller.data_mut().proxied_caller_thread = 0;

    if done_ctx != 0 {
        if let Ok(done_fn) = get_export_func(caller, "_emscripten_run_js_on_main_thread_done") {
            if let Ok(typed) = done_fn.typed::<(i32, i32, f64), ()>(&*caller) {
                let _ = typed.call(&mut *caller, (done_ctx, done_flag, res));
            }
        }
    }
    Ok(res)
}

fn dispatch_proxied_table(
    caller: &mut Caller<'_, StoreState>,
    func_id: i32,
    args: &[EmAsmArg],
) -> Result<f64> {
    let rt = caller.data().runtime.clone();
    let i = |idx: usize| args.get(idx).copied().map(EmAsmArg::as_i32).unwrap_or(0);
    match func_id {
        0 | 1 => {
            wasi::proc_exit_impl(&rt, i(0))?;
            Ok(0.0)
        }
        2 => Ok(pthread_create_impl(&rt, i(0), i(1), i(2), i(3))? as f64),
        3 | 4 | 5 | 6 | 7 => Ok(0.0),
        8 => Ok(platform::exit_pointerlock_impl(&rt) as f64),
        9 => Ok(platform::get_canvas_size_impl(&rt, i(0), i(1), i(2))? as f64),
        10 => Ok(rt.platform.lock().unwrap().device_pixel_ratio),
        11 => Ok(platform::get_css_size_impl(&rt, i(0), i(1), i(2))? as f64),
        12 => Ok(-5.0),
        13 => Ok(0.0),
        14 => Ok(platform::request_pointerlock_impl(&rt, i(0), i(1)) as f64),
        15 => Ok(0.0),
        16 => Ok(platform::register_listener_impl(&rt, i(0), 12, i(1), i(2) != 0, i(3), platform::resolve_target_thread(caller, i(4))) as f64),
        17 => Ok(platform::set_canvas_size_impl(&rt, i(0), i(1), i(2)) as f64),
        18 => Ok(platform::register_listener_impl(&rt, i(0), 13, i(1), i(2) != 0, i(3), platform::resolve_target_thread(caller, i(4))) as f64),
        19 => Ok(platform::register_listener_impl(&rt, 2, 26, i(0), i(1) != 0, i(2), platform::resolve_target_thread(caller, i(3))) as f64),
        20 => Ok(platform::register_listener_impl(&rt, 2, 27, i(0), i(1) != 0, i(2), platform::resolve_target_thread(caller, i(3))) as f64),
        21 => Ok(platform::register_listener_impl(&rt, i(0), 2, i(1), i(2) != 0, i(3), platform::resolve_target_thread(caller, i(4))) as f64),
        22 => Ok(platform::register_listener_impl(&rt, i(0), 3, i(1), i(2) != 0, i(3), platform::resolve_target_thread(caller, i(4))) as f64),
        23 => Ok(platform::register_listener_impl(&rt, i(0), 5, i(1), i(2) != 0, i(3), platform::resolve_target_thread(caller, i(4))) as f64),
        24 => Ok(platform::register_listener_impl(&rt, i(0), 33, i(1), i(2) != 0, i(3), platform::resolve_target_thread(caller, i(4))) as f64),
        25 => Ok(platform::register_listener_impl(&rt, i(0), 34, i(1), i(2) != 0, i(3), platform::resolve_target_thread(caller, i(4))) as f64),
        26 => Ok(platform::register_listener_impl(&rt, i(0), 8, i(1), i(2) != 0, i(3), platform::resolve_target_thread(caller, i(4))) as f64),
        27 => Ok(platform::register_listener_impl(&rt, i(0), 6, i(1), i(2) != 0, i(3), platform::resolve_target_thread(caller, i(4))) as f64),
        28 => Ok(platform::register_listener_impl(&rt, i(0), 38, i(1), i(2) != 0, i(3), platform::resolve_target_thread(caller, i(4))) as f64),
        29 => Ok(platform::register_listener_impl(&rt, i(0), 25, i(1), i(2) != 0, i(3), platform::resolve_target_thread(caller, i(4))) as f64),
        30 => Ok(platform::register_listener_impl(&rt, i(0), 23, i(1), i(2) != 0, i(3), platform::resolve_target_thread(caller, i(4))) as f64),
        31 => Ok(platform::register_listener_impl(&rt, i(0), 24, i(1), i(2) != 0, i(3), platform::resolve_target_thread(caller, i(4))) as f64),
        32 => Ok(platform::register_listener_impl(&rt, i(0), 22, i(1), i(2) != 0, i(3), platform::resolve_target_thread(caller, i(4))) as f64),
        33 => Ok(platform::register_listener_impl(&rt, i(0), 9, i(1), i(2) != 0, i(3), platform::resolve_target_thread(caller, i(4))) as f64),
        34 => Ok(network::ws_close_impl(&rt, i(0), i(1), i(2))? as f64),
        35 => Ok(network::ws_delete_impl(caller, i(0))? as f64),
        36 => Ok(network::ws_new_impl(caller, i(0))? as f64),
        37 => Ok(network::ws_send_binary_impl(&rt, i(0), i(1), i(2))? as f64),
        38 => Ok(network::ws_send_text_impl(&rt, i(0), i(1))? as f64),
        39 => Ok(network::ws_set_cb_impl(&rt, i(0), i(1), i(2), i(3), 3) as f64),
        40 => Ok(network::ws_set_cb_impl(&rt, i(0), i(1), i(2), i(3), 2) as f64),
        41 => Ok(network::ws_set_cb_impl(&rt, i(0), i(1), i(2), i(3), 1) as f64),
        42 => Ok(network::ws_set_cb_impl(&rt, i(0), i(1), i(2), i(3), 0) as f64),
        43 => Ok(wasi::environ_get_impl(&rt, i(0), i(1))? as f64),
        44 => Ok(wasi::environ_sizes_get_impl(&rt, i(0), i(1))? as f64),
        45 => Ok(52.0),
        _ => Ok(0.0),
    }
}
