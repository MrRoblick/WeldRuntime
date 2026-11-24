use std::sync::Arc;
use std::thread;
use wasmtime::{Caller, Linker, Result, Store, format_err};

use crate::runtime::{FiberSlot, LiveRuntimeUnwind, StoreState, WASM_TLS_SIZE};

pub fn register(linker: &mut Linker<StoreState>) -> Result<()> {
    linker.func_wrap(
        "env",
        "__asyncjs__create_suspended_fiber_js",
        |caller: Caller<'_, StoreState>,
         caller_fiber_id: i32,
         stack_base: i32,
         stack_end: i32,
         fn_ptr: i32,
         arg: i32|
         -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let is_main = caller.data().is_main_thread;
            let pthread_ptr = caller.data().pthread_ptr;

            let slot = {
                let s = {
                    let mut map = rt.fibers.lock().unwrap();
                    map.entry(caller_fiber_id)
                        .or_insert_with(|| Arc::new(FiberSlot::new()))
                        .clone()
                };
                {
                    let mut ctrl = s.state.lock().unwrap();
                    ctrl.ready_to_run = false;
                    ctrl.destroy_requested = false;
                    ctrl.resume_value = 0;
                }
                s
            };

            let instance_pre = rt
                .instance_pre
                .get()
                .ok_or_else(|| format_err!("InstancePre not initialized before fiber spawn"))?
                .clone();

            let rt_child = rt.clone();
            thread::Builder::new()
                .name(format!("rbx-fiber-{caller_fiber_id:#x}"))
                .stack_size(16 * 1024 * 1024)
                .spawn(move || {
                    let mut store = Store::new(
                        &rt_child.engine,
                        StoreState::fiber(rt_child.clone(), is_main, pthread_ptr),
                    );
                    let instance = match instance_pre.instantiate(&mut store) {
                        Ok(i) => i,
                        Err(e) => {
                            eprintln!("[Fiber] failed to instantiate module: {e:?}");
                            return;
                        }
                    };
                    store.data_mut().instance = Some(instance);

                    if pthread_ptr != 0 {
                        if let Ok(init_fn) = instance
                            .get_typed_func::<(i32, i32, i32, i32, i32, i32), ()>(
                                &mut store,
                                "_emscripten_thread_init",
                            )
                        {
                            let _ = init_fn.call(
                                &mut store,
                                (pthread_ptr, i32::from(is_main), i32::from(is_main), 1, 262144, 0),
                            );
                        }

                        let existing_tls = rt_child
                            .pthread_tls
                            .lock()
                            .unwrap()
                            .get(&pthread_ptr)
                            .copied()
                            .unwrap_or(0);

                        if existing_tls != 0 {
                            let saved = rt_child
                                .memory
                                .read_bytes(existing_tls, WASM_TLS_SIZE)
                                .ok();
                            let _ = rt_child.memory.write_i32(pthread_ptr + 100, existing_tls);
                            if let Ok(tls_fn) =
                                instance.get_typed_func::<(), i32>(&mut store, "_emscripten_tls_init")
                            {
                                let _ = tls_fn.call(&mut store, ());
                            }
                            if let Some(ref bytes) = saved {
                                let _ = rt_child.memory.write_bytes(existing_tls, bytes);
                            }
                        } else {
                            if is_main {
                                if let Some(enter) =
                                    instance.get_func(&mut store, "rbxJspiJsAllocatorEnter")
                                {
                                    if let Ok(f) = enter.typed::<(), ()>(&store) {
                                        let _ = f.call(&mut store, ());
                                    }
                                }
                            }
                            if let Ok(tls_fn) =
                                instance.get_typed_func::<(), i32>(&mut store, "_emscripten_tls_init")
                            {
                                if let Ok(tb) = tls_fn.call(&mut store, ()) {
                                    if tb != 0 {
                                        rt_child.pthread_tls.lock().unwrap().insert(pthread_ptr, tb);
                                    }
                                }
                            }
                            if is_main {
                                if let Some(leave) =
                                    instance.get_func(&mut store, "rbxJspiJsAllocatorLeave")
                                {
                                    if let Ok(f) = leave.typed::<(), ()>(&store) {
                                        let _ = f.call(&mut store, ());
                                    }
                                }
                            }
                        }
                    }

                    if let Ok(set_limits) =
                        instance.get_typed_func::<(i32, i32), ()>(&mut store, "set_stack_limits")
                    {
                        let _ = set_limits.call(&mut store, (stack_base, stack_end));
                    }
                    if let Ok(set_em_limits) = instance
                        .get_typed_func::<(i32, i32), ()>(&mut store, "emscripten_stack_set_limits")
                    {
                        let _ = set_em_limits.call(&mut store, (stack_base, stack_end));
                    }
                    if let Ok(set_sp) =
                        instance.get_typed_func::<i32, ()>(&mut store, "set_stack_ptr")
                    {
                        let _ = set_sp.call(&mut store, stack_base);
                    }

                    if let Ok(entry) =
                        instance.get_typed_func::<(i32, i32, i32), ()>(&mut store, "fiber_entry")
                    {
                        if let Err(e) = entry.call(&mut store, (caller_fiber_id, fn_ptr, arg)) {
                            if e.downcast_ref::<LiveRuntimeUnwind>().is_none() {
                                eprintln!("[Fiber {caller_fiber_id:#x}] exited with error: {e:?}");
                            }
                        }
                    }
                })
                .map_err(|e| format_err!("failed to spawn fiber thread: {e}"))?;

            let mut ctrl = slot.state.lock().unwrap();
            while !ctrl.ready_to_run {
                ctrl = slot.cvar.wait(ctrl).unwrap();
            }
            ctrl.ready_to_run = false;
            Ok(ctrl.resume_value)
        },
    )?;

    linker.func_wrap(
        "env",
        "__asyncjs__fiber_transfer_js",
        |caller: Caller<'_, StoreState>, from_fiber: i32, to_fiber: i32| -> Result<i32> {
            if from_fiber == to_fiber {
                return Ok(0);
            }
            let rt = &caller.data().runtime;

            let my_slot = if from_fiber != 0 {
                let s = {
                    let mut map = rt.fibers.lock().unwrap();
                    map.entry(from_fiber)
                        .or_insert_with(|| Arc::new(FiberSlot::new()))
                        .clone()
                };
                {
                    let mut ctrl = s.state.lock().unwrap();
                    ctrl.ready_to_run = false;
                    ctrl.destroy_requested = false;
                    ctrl.resume_value = 0;
                }
                Some(s)
            } else {
                None
            };

            let target_slot = rt
                .fibers
                .lock()
                .unwrap()
                .get(&to_fiber)
                .cloned()
                .ok_or_else(|| format_err!("fiber_transfer_js: target fiber {to_fiber:#x} not found"))?;

            {
                let mut t_ctrl = target_slot.state.lock().unwrap();
                t_ctrl.resume_value = from_fiber;
                t_ctrl.ready_to_run = true;
                target_slot.cvar.notify_one();
            }

            if let Some(slot) = my_slot {
                let mut ctrl = slot.state.lock().unwrap();
                while !ctrl.ready_to_run {
                    ctrl = slot.cvar.wait(ctrl).unwrap();
                }
                ctrl.ready_to_run = false;
                if ctrl.destroy_requested {
                    return Err(wasmtime::Error::new(LiveRuntimeUnwind));
                }
                Ok(ctrl.resume_value)
            } else {
                Ok(0)
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "fiber_destroy_js",
        |caller: Caller<'_, StoreState>, fiber_id: i32| {
            let slot = caller.data().runtime.fibers.lock().unwrap().remove(&fiber_id);
            if let Some(slot) = slot {
                let mut ctrl = slot.state.lock().unwrap();
                ctrl.destroy_requested = true;
                ctrl.resume_value = 0;
                ctrl.ready_to_run = true;
                slot.cvar.notify_one();
            }
        },
    )?;

    Ok(())
}