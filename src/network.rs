use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use wasmtime::{AsContextMut, Caller, Instance, Linker, Ref, Result};

use crate::backends::{HttpRequest, HttpResponse, RnaConnectParams, normalize_roblosecurity};
use crate::memory::{
    get_caller_instance, guest_free, guest_malloc, instance_malloc, instance_realloc,
};
use crate::runtime::{
    CompletedFetch, FetchEntry, MainThreadTask, RnaConnEntry, RuntimeState, StoreState,
    WebSocketEntry, WorkerTask,
};

pub fn register(linker: &mut Linker<StoreState>) -> Result<()> {
    linker.func_wrap(
        "env",
        "js_fetch_file",
        |caller: Caller<'_, StoreState>, url_ptr: i32, dst_ptr: i32, expected_len: i32| -> Result<i32> {
            let rt = &caller.data().runtime;
            let url = rt.memory.read_cstr(url_ptr)?;
            let expected = expected_len.max(0) as usize;

            if let Some(local_bytes) = try_read_local_asset(rt, &url) {
                if local_bytes.len() == expected {
                    println!("[FS:Local] {url} ({expected} bytes)");
                    rt.memory.write_bytes(dst_ptr, &local_bytes)?;
                    return Ok(0);
                }
                eprintln!(
                    "[FS] local size mismatch for {url}: expected={expected}, got={} (falling back to CDN)",
                    local_bytes.len()
                );
            }

            let fetch_url = resolve_vfs_fetch_url(rt, &url);
            let Some(fetch_url) = fetch_url else {
                eprintln!("[FS] local asset missing and no engine_base_url: {url} (expected={expected})");
                return Ok(-1);
            };

            println!("[FS:CDN] {fetch_url} (expected={expected})");
            let req = HttpRequest {
                method: "GET".into(),
                url: fetch_url.clone(),
                headers: Vec::new(),
                body: None,
                timeout_ms: 30_000,
            };
            match rt.http_backend.request(&req) {
                Ok(resp) if (200..300).contains(&resp.status) => {
                    if resp.body.len() != expected {
                        eprintln!(
                            "[FS] size mismatch for {fetch_url}: expected={expected}, got={}",
                            resp.body.len()
                        );
                        return Ok(-1);
                    }
                    cache_local_asset(rt, &url, &resp.body);
                    rt.memory.write_bytes(dst_ptr, &resp.body)?;
                    Ok(0)
                }
                Ok(resp) => {
                    eprintln!("[FS] fetch {fetch_url} HTTP {}", resp.status);
                    Ok(-1)
                }
                Err(err) => {
                    eprintln!("[FS] fetch failed {fetch_url}: {err}");
                    Ok(-1)
                }
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_start_fetch",
        |mut caller: Caller<'_, StoreState>, fetch_ptr: i32| -> Result<()> {
            start_fetch_impl(&mut caller, fetch_ptr)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_fetch_free",
        |caller: Caller<'_, StoreState>, id: i32| {
            let rt = &caller.data().runtime;
            rt.active_fetches.lock().unwrap().remove(&id);
            rt.fetches.lock().unwrap().remove(&id);
        },
    )?;

    linker.func_wrap(
        "env",
        "_emscripten_fetch_get_response_headers_length",
        |caller: Caller<'_, StoreState>, id: i32| -> i32 {
            let map = caller.data().runtime.fetches.lock().unwrap();
            map.get(&id)
                .map(|e| format_headers(&e.headers).len() as i32)
                .unwrap_or(0)
        },
    )?;

    linker.func_wrap(
        "env",
        "_emscripten_fetch_get_response_headers",
        |caller: Caller<'_, StoreState>, id: i32, dst_ptr: i32, dst_size: i32| -> Result<i32> {
            let rt = &caller.data().runtime;
            let formatted = {
                let map = rt.fetches.lock().unwrap();
                map.get(&id)
                    .map(|e| format_headers(&e.headers))
                    .unwrap_or_default()
            };
            let written = rt
                .memory
                .write_cstr(dst_ptr, &formatted, dst_size.max(0) as usize)?;
            Ok((written + 1) as i32)
        },
    )?;

    // WebSocket
    linker.func_wrap(
        "env",
        "emscripten_websocket_new",
        |mut caller: Caller<'_, StoreState>, attr_ptr: i32| -> Result<i32> {
            ws_new_impl(&mut caller, attr_ptr)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_websocket_delete",
        |mut caller: Caller<'_, StoreState>, ws_id: i32| -> Result<i32> {
            ws_delete_impl(&mut caller, ws_id)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_websocket_close",
        |caller: Caller<'_, StoreState>, ws_id: i32, code: i32, reason_ptr: i32| -> Result<i32> {
            ws_close_impl(&caller.data().runtime, ws_id, code, reason_ptr)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_websocket_send_binary",
        |caller: Caller<'_, StoreState>, ws_id: i32, data_ptr: i32, len: i32| -> Result<i32> {
            ws_send_binary_impl(&caller.data().runtime, ws_id, data_ptr, len)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_websocket_send_utf8_text",
        |caller: Caller<'_, StoreState>, ws_id: i32, text_ptr: i32| -> Result<i32> {
            ws_send_text_impl(&caller.data().runtime, ws_id, text_ptr)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_websocket_set_onopen_callback_on_thread",
        |caller: Caller<'_, StoreState>, ws_id: i32, user_data: i32, cb: i32, thread: i32| -> i32 {
            ws_set_cb_impl(&caller.data().runtime, ws_id, user_data, cb, thread, 0)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_websocket_set_onmessage_callback_on_thread",
        |caller: Caller<'_, StoreState>, ws_id: i32, user_data: i32, cb: i32, thread: i32| -> i32 {
            ws_set_cb_impl(&caller.data().runtime, ws_id, user_data, cb, thread, 1)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_websocket_set_onerror_callback_on_thread",
        |caller: Caller<'_, StoreState>, ws_id: i32, user_data: i32, cb: i32, thread: i32| -> i32 {
            ws_set_cb_impl(&caller.data().runtime, ws_id, user_data, cb, thread, 2)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_websocket_set_onclose_callback_on_thread",
        |caller: Caller<'_, StoreState>, ws_id: i32, user_data: i32, cb: i32, thread: i32| -> i32 {
            ws_set_cb_impl(&caller.data().runtime, ws_id, user_data, cb, thread, 3)
        },
    )?;

    // RNA / WebTransport
    linker.func_wrap(
        "env",
        "rbx_wasm_rna_is_supported_js",
        |caller: Caller<'_, StoreState>| -> i32 {
            i32::from(caller.data().runtime.rna_backend.is_supported())
        },
    )?;

    linker.func_wrap(
        "env",
        "rbx_wasm_rna_connect_js",
        |caller: Caller<'_, StoreState>,
         conn_id: i32,
         url_ptr: i32,
         cert_hashes_ptr: i32,
         cert_count: i32,
         rx_data_ptr: i32,
         rx_head_ptr: i32,
         rx_tail_ptr: i32,
         rx_slot_size: i32,
         rx_slot_count: i32,
         use_stream_err_code: i32,
         fix_conn_ownership: i32,
         datagram_compat: i32,
         report_handshake: i32,
         stats_interval_ms: i32|
         -> Result<()> {
            let owner_thread = if caller.data().is_main_thread {
                0
            } else {
                caller.data().pthread_ptr
            };
            let rt = &caller.data().runtime;
            let url = rt.memory.read_cstr(url_ptr)?;
            let mut hashes = Vec::new();
            for i in 0..cert_count.max(0) {
                let p = rt.memory.read_i32(cert_hashes_ptr.wrapping_add(i * 4))?;
                hashes.push(rt.memory.read_cstr(p)?);
            }
            let id = conn_id as u32;
            rt.rna_conns.lock().unwrap().insert(
                id,
                RnaConnEntry {
                    url: url.clone(),
                    receive_data_ptr: rx_data_ptr,
                    receive_head_ptr: rx_head_ptr,
                    receive_tail_ptr: rx_tail_ptr,
                    receive_slot_size: rx_slot_size as u32,
                    receive_slot_count: rx_slot_count as u32,
                    closed: false,
                },
            );
            rt.rna_backend.connect(RnaConnectParams {
                conn_id: id,
                owner_thread,
                url,
                cert_hashes: hashes,
                rx_data_ptr,
                rx_head_ptr,
                rx_tail_ptr,
                rx_slot_size: rx_slot_size.max(0) as u32,
                rx_slot_count: rx_slot_count.max(0) as u32,
                use_stream_err_code: use_stream_err_code != 0,
                fix_conn_ownership: fix_conn_ownership != 0,
                datagram_compat: datagram_compat != 0,
                report_handshake: report_handshake != 0,
                stats_interval_ms: stats_interval_ms.max(0) as u32,
            });
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "rbx_wasm_rna_open_stream_js",
        |caller: Caller<'_, StoreState>, conn_id: i32, stream_id: i32| {
            caller
                .data()
                .runtime
                .rna_backend
                .open_stream(conn_id as u32, stream_id);
        },
    )?;

    linker.func_wrap(
        "env",
        "rbx_wasm_rna_write_stream_js",
        |caller: Caller<'_, StoreState>,
         conn_id: i32,
         stream_id: i32,
         ptr: i32,
         len: i32|
         -> Result<()> {
            let rt = &caller.data().runtime;
            let bytes = rt.memory.read_bytes(ptr, len.max(0) as usize)?;
            rt.rna_backend
                .write_stream(conn_id as u32, stream_id, &bytes);
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "rbx_wasm_rna_shutdown_stream_write_js",
        |caller: Caller<'_, StoreState>, conn_id: i32, stream_id: i32| {
            caller
                .data()
                .runtime
                .rna_backend
                .shutdown_stream_write(conn_id as u32, stream_id);
        },
    )?;

    linker.func_wrap(
        "env",
        "rbx_wasm_rna_shutdown_stream_read_js",
        |caller: Caller<'_, StoreState>, conn_id: i32, stream_id: i32| {
            caller
                .data()
                .runtime
                .rna_backend
                .shutdown_stream_read(conn_id as u32, stream_id);
        },
    )?;

    linker.func_wrap(
        "env",
        "rbx_wasm_rna_pause_stream_read_js",
        |caller: Caller<'_, StoreState>, conn_id: i32, stream_id: i32| {
            caller
                .data()
                .runtime
                .rna_backend
                .pause_stream_read(conn_id as u32, stream_id);
        },
    )?;

    linker.func_wrap(
        "env",
        "rbx_wasm_rna_resume_stream_read_js",
        |caller: Caller<'_, StoreState>, conn_id: i32, stream_id: i32| {
            caller
                .data()
                .runtime
                .rna_backend
                .resume_stream_read(conn_id as u32, stream_id);
        },
    )?;

    linker.func_wrap(
        "env",
        "rbx_wasm_rna_receive_ring_consumed_js",
        |caller: Caller<'_, StoreState>, conn_id: i32, kind: i32, stream_id: i32| {
            caller
                .data()
                .runtime
                .rna_backend
                .receive_ring_consumed(conn_id as u32, kind, stream_id);
        },
    )?;

    linker.func_wrap(
        "env",
        "rbx_wasm_rna_send_datagram_js",
        |caller: Caller<'_, StoreState>,
         conn_id: i32,
         ptr: i32,
         len: i32,
         hi: i32,
         lo: i32|
         -> Result<()> {
            let rt = &caller.data().runtime;
            let bytes = rt.memory.read_bytes(ptr, len.max(0) as usize)?;
            rt.rna_backend
                .send_datagram(conn_id as u32, &bytes, hi, lo);
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "rbx_wasm_rna_send_connection_close_js",
        |caller: Caller<'_, StoreState>, conn_id: i32| {
            let rt = &caller.data().runtime;
            if let Some(entry) = rt.rna_conns.lock().unwrap().get_mut(&(conn_id as u32)) {
                entry.closed = true;
            }
            rt.rna_backend.send_connection_close(conn_id as u32);
        },
    )?;

    linker.func_wrap(
        "env",
        "rbx_wasm_rna_close_js",
        |caller: Caller<'_, StoreState>, conn_id: i32| {
            let rt = &caller.data().runtime;
            rt.rna_conns.lock().unwrap().remove(&(conn_id as u32));
            rt.rna_backend.close(conn_id as u32);
        },
    )?;

    Ok(())
}

fn vfs_relative_path<'a>(rt: &RuntimeState, url: &'a str) -> Option<&'a str> {
    let base = rt.config.engine_base_url.trim_end_matches('/');
    let rel = if !base.is_empty() {
        if let Some(stripped) = url.strip_prefix(base) {
            stripped
        } else if url.contains("://") {
            return None;
        } else {
            url
        }
    } else if url.contains("://") {
        return None;
    } else {
        url
    };
    let clean = rel.trim_start_matches("./").trim_start_matches('/');
    if clean.is_empty()
        || clean
            .split(['/', '\\'])
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return None;
    }
    Some(clean)
}

fn resolve_vfs_fetch_url(rt: &RuntimeState, url: &str) -> Option<String> {
    if url.contains("://") {
        return Some(url.to_owned());
    }
    let base = rt.config.engine_base_url.trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    let clean = url.trim_start_matches("./").trim_start_matches('/');
    Some(format!("{base}/{clean}"))
}

fn cache_local_asset(rt: &RuntimeState, url: &str, bytes: &[u8]) {
    let Some(rel) = vfs_relative_path(rt, url) else {
        return;
    };
    let dst = rt.config.fs_dir.join(rel);
    if let Some(parent) = dst.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(dst, bytes);
}

fn try_read_local_asset(rt: &RuntimeState, url: &str) -> Option<Vec<u8>> {
    if let Some(path_str) = url.strip_prefix("file://") {
        return fs::read(path_str).ok();
    }
    let clean = vfs_relative_path(rt, url)?;
    let fs_candidate = PathBuf::from(&rt.config.fs_dir).join(clean);
    if let Ok(bytes) = fs::read(&fs_candidate) {
        return Some(bytes);
    }
    let res_candidate = PathBuf::from(&rt.config.resources_dir).join(clean);
    fs::read(res_candidate).ok()
}

fn format_headers(headers: &[(String, String)]) -> String {
    let mut out = String::new();
    for (k, v) in headers {
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v);
        out.push_str("\r\n");
    }
    out
}

fn start_fetch_impl(caller: &mut Caller<'_, StoreState>, fetch_ptr: i32) -> Result<()> {
    let rt = caller.data().runtime.clone();
    let is_main = caller.data().is_main_thread;
    let pthread_ptr = caller.data().pthread_ptr;

    let url_ptr = rt.memory.read_i32(fetch_ptr + 8)?;
    let attr_base = fetch_ptr + 108;
    let onsuccess = rt.memory.read_i32(attr_base + 36)?;
    let onerror = rt.memory.read_i32(attr_base + 40)?;
    let onreadystatechange = rt.memory.read_i32(attr_base + 48)?;
    let attrs = rt.memory.read_u32(attr_base + 52)?;
    let load_to_mem = (attrs & 1) != 0;
    let synchronous = (attrs & 64) != 0;

    let fetch_id = rt.next_fetch_id.fetch_add(1, Ordering::Relaxed);
    rt.memory.write_i32(fetch_ptr, fetch_id)?;
    rt.active_fetches.lock().unwrap().insert(fetch_id);

    if url_ptr == 0 {
        let done = CompletedFetch {
            fetch_id,
            fetch_ptr,
            onsuccess,
            onerror,
            onreadystatechange,
            load_to_mem,
            result: Err("no url specified".into()),
        };
        if synchronous {
            let inst = get_caller_instance(caller)?;
            deliver_completed_fetch(caller, inst, &rt, is_main, done)?;
        } else {
            enqueue_fetch_completion(caller, &rt, is_main, pthread_ptr, done);
        }
        return Ok(());
    }

    let url = rt.memory.read_cstr(url_ptr)?;
    let method_raw = rt.memory.read_cstr(attr_base)?;
    let method = if method_raw.is_empty() {
        "GET".to_owned()
    } else {
        method_raw
    };
    let timeout_ms = rt.memory.read_u32(attr_base + 56)?;
    let req_headers_ptr = rt.memory.read_i32(attr_base + 76)?;
    let req_data_ptr = rt.memory.read_i32(attr_base + 84)?;
    let req_data_len = rt.memory.read_u32(attr_base + 88)? as usize;

    let mut headers = Vec::new();
    if req_headers_ptr != 0 {
        let mut cur = req_headers_ptr;
        loop {
            let k_ptr = rt.memory.read_i32(cur)?;
            if k_ptr == 0 {
                break;
            }
            let v_ptr = rt.memory.read_i32(cur + 4)?;
            if v_ptr == 0 {
                break;
            }
            headers.push((rt.memory.read_cstr(k_ptr)?, rt.memory.read_cstr(v_ptr)?));
            cur += 8;
        }
    }

    if url.contains("roblox.com") || url.contains("robloxlabs.com") {
        let origin = rt.config.roblox_base_url.trim_end_matches('/').to_owned();
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("origin")) {
            headers.push(("Origin".into(), origin));
        }
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("referer")) {
            headers.push(("Referer".into(), rt.config.roblox_base_url.clone()));
        }
        if let Some(ref cookie) = rt.config.auth_cookie {
            let clean = normalize_roblosecurity(cookie);
            if !clean.is_empty() && !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("cookie")) {
                headers.push(("Cookie".into(), format!(".ROBLOSECURITY={clean}")));
            }
        }
    }

    let body = if req_data_ptr != 0 && req_data_len > 0 {
        Some(rt.memory.read_bytes(req_data_ptr, req_data_len)?)
    } else {
        None
    };

    if method == "GET" {
        if let Some(bytes) = try_read_local_asset(&rt, &url) {
            println!("[Fetch:Local] {} -> {} bytes", url, bytes.len());
            let done = CompletedFetch {
                fetch_id,
                fetch_ptr,
                onsuccess,
                onerror,
                onreadystatechange,
                load_to_mem,
                result: Ok(HttpResponse {
                    status: 200,
                    status_text: "OK".into(),
                    headers: Vec::new(),
                    body: bytes,
                    final_url: url,
                }),
            };
            if synchronous {
                let inst = get_caller_instance(caller)?;
                deliver_completed_fetch(caller, inst, &rt, is_main, done)?;
            } else {
                enqueue_fetch_completion(caller, &rt, is_main, pthread_ptr, done);
            }
            return Ok(());
        }
    }

    let req = HttpRequest {
        method: method.clone(),
        url: url.clone(),
        headers,
        body,
        timeout_ms,
    };

    if synchronous {
        println!("[Fetch:Sync] {} {}", method, url);
        let res = rt.http_backend.request(&req);
        match &res {
            Ok(r) => println!(
                "[Fetch:Sync] {} -> HTTP {} ({} bytes)",
                url,
                r.status,
                r.body.len()
            ),
            Err(e) => eprintln!("[Fetch:Sync] {} -> ERR: {}", url, e),
        }
        let inst = get_caller_instance(caller)?;
        deliver_completed_fetch(
            caller,
            inst,
            &rt,
            is_main,
            CompletedFetch {
                fetch_id,
                fetch_ptr,
                onsuccess,
                onerror,
                onreadystatechange,
                load_to_mem,
                result: res,
            },
        )?;
    } else {
        if is_main {
            rt.keepalive_count.fetch_add(1, Ordering::SeqCst);
        } else {
            caller.data_mut().keepalive += 1;
        }
        let rt_bg = rt.clone();
        thread::Builder::new()
            .name(format!("rbx-fetch-{fetch_id}"))
            .spawn(move || {
                println!("[Fetch:HTTP] {} {}", req.method, req.url);
                let res = rt_bg.http_backend.request(&req);
                match &res {
                    Ok(r) => println!(
                        "[Fetch:HTTP] {} -> HTTP {} ({} bytes)",
                        req.url,
                        r.status,
                        r.body.len()
                    ),
                    Err(e) => eprintln!("[Fetch:HTTP] {} -> ERR: {}", req.url, e),
                }
                let done = CompletedFetch {
                    fetch_id,
                    fetch_ptr,
                    onsuccess,
                    onerror,
                    onreadystatechange,
                    load_to_mem,
                    result: res,
                };
                if is_main || !rt_bg.push_worker_task(pthread_ptr, WorkerTask::FetchComplete(done.clone())) {
                    rt_bg.push_main_task(MainThreadTask::FetchComplete(done));
                }
            })
            .map_err(|e| wasmtime::format_err!("failed to spawn fetch thread: {e}"))?;
    }
    Ok(())
}

fn enqueue_fetch_completion(
    caller: &mut Caller<'_, StoreState>,
    rt: &Arc<RuntimeState>,
    is_main: bool,
    pthread_ptr: i32,
    done: CompletedFetch,
) {
    if is_main {
        rt.keepalive_count.fetch_add(1, Ordering::SeqCst);
        rt.push_main_task(MainThreadTask::FetchComplete(done));
    } else {
        caller.data_mut().keepalive += 1;
        if !rt.push_worker_task(pthread_ptr, WorkerTask::FetchComplete(done.clone())) {
            rt.push_main_task(MainThreadTask::FetchComplete(done));
        }
    }
}

pub fn deliver_completed_fetch(
    mut store: impl AsContextMut<Data = StoreState>,
    inst: Instance,
    rt: &Arc<RuntimeState>,
    is_main: bool,
    done: CompletedFetch,
) -> Result<()> {
    if is_main {
        let prev = rt.keepalive_count.load(Ordering::SeqCst);
        if prev > 0 {
            rt.keepalive_count.fetch_sub(1, Ordering::SeqCst);
        }
    } else {
        let mut ctx = store.as_context_mut();
        let st = ctx.data_mut();
        st.keepalive = (st.keepalive - 1).max(0);
    }

    if !rt.active_fetches.lock().unwrap().remove(&done.fetch_id) {
        return Ok(());
    }

    let fetch_ptr = done.fetch_ptr;
    match done.result {
        Ok(resp) => {
            let total = resp.body.len();
            if done.load_to_mem && total > 0 {
                let prev = rt.memory.read_i32(fetch_ptr + 12)?;
                let dst = if prev == 0 {
                    instance_malloc(&mut store, inst, is_main, total)?
                } else {
                    instance_realloc(&mut store, inst, is_main, prev, total)?
                };
                if dst != 0 {
                    rt.memory.write_bytes(dst, &resp.body)?;
                }
                rt.memory.write_i32(fetch_ptr + 12, dst)?;
                rt.memory.write_u64(fetch_ptr + 16, total as u64)?;
            } else {
                rt.memory.write_u64(fetch_ptr + 16, 0)?;
            }
            rt.memory.write_u64(fetch_ptr + 24, 0)?;
            rt.memory.write_u64(fetch_ptr + 32, total as u64)?;
            rt.memory.write_u16(fetch_ptr + 40, 4)?;
            rt.memory.write_u16(fetch_ptr + 42, resp.status)?;
            rt.memory.write_cstr(fetch_ptr + 44, &resp.status_text, 64)?;

            let url_bytes = resp.final_url.as_bytes();
            let resp_url_ptr = instance_malloc(&mut store, inst, is_main, url_bytes.len() + 1)?;
            if resp_url_ptr != 0 {
                rt.memory.write_bytes(resp_url_ptr, url_bytes)?;
                rt.memory
                    .write_u8(resp_url_ptr.wrapping_add(url_bytes.len() as i32), 0)?;
            }
            rt.memory.write_i32(fetch_ptr + 200, resp_url_ptr)?;

            let ok = (200..300).contains(&resp.status);
            rt.fetches.lock().unwrap().insert(
                done.fetch_id,
                FetchEntry {
                    status: resp.status,
                    headers: resp.headers,
                },
            );

            if done.onreadystatechange != 0 {
                call_fetch_cb(&mut store, inst, done.onreadystatechange, fetch_ptr);
            }
            let cb = if ok { done.onsuccess } else { done.onerror };
            if cb != 0 {
                call_fetch_cb(&mut store, inst, cb, fetch_ptr);
            }
        }
        Err(_) => {
            rt.memory.write_u16(fetch_ptr + 40, 4)?;
            rt.memory.write_u16(fetch_ptr + 42, 0)?;
            if done.onreadystatechange != 0 {
                call_fetch_cb(&mut store, inst, done.onreadystatechange, fetch_ptr);
            }
            if done.onerror != 0 {
                call_fetch_cb(&mut store, inst, done.onerror, fetch_ptr);
            }
        }
    }
    Ok(())
}

fn call_fetch_cb(
    mut store: impl AsContextMut<Data = StoreState>,
    inst: Instance,
    fn_idx: i32,
    fetch_ptr: i32,
) {
    if let Some(table) = inst.get_table(&mut store, "__indirect_function_table") {
        if let Some(Ref::Func(Some(func))) = table.get(&mut store, fn_idx as u32 as u64) {
            if let Ok(typed) = func.typed::<i32, ()>(&store) {
                let _ = typed.call(&mut store, fetch_ptr);
            }
        }
    }
}

pub fn ws_new_impl(caller: &mut Caller<'_, StoreState>, attr_ptr: i32) -> Result<i32> {
    let rt = caller.data().runtime.clone();
    if !rt.ws_backend.is_supported() {
        return Ok(-1);
    }
    if attr_ptr == 0 {
        return Ok(-5);
    }
    let url_ptr = rt.memory.read_i32(attr_ptr)?;
    let proto_ptr = rt.memory.read_i32(attr_ptr + 4)?;
    let url = rt.memory.read_cstr(url_ptr)?;
    let protocols = if proto_ptr != 0 {
        rt.memory
            .read_cstr(proto_ptr)?
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        Vec::new()
    };

    let id = rt.next_ws_id.fetch_add(1, Ordering::Relaxed);
    let event_ptr = guest_malloc(caller, 520)?;
    rt.memory.write_i32(event_ptr, id)?;
    let rc = rt.ws_backend.connect(id, &url, &protocols);
    if rc < 0 {
        let _ = guest_free(caller, event_ptr);
        return Ok(rc);
    }

    rt.websockets.lock().unwrap().insert(
        id,
        WebSocketEntry {
            url,
            protocols,
            on_open: None,
            on_message: None,
            on_error: None,
            on_close: None,
            event_ptr,
            connected: false,
        },
    );
    Ok(id)
}

pub fn ws_delete_impl(caller: &mut Caller<'_, StoreState>, ws_id: i32) -> Result<i32> {
    let rt = caller.data().runtime.clone();
    let entry = rt.websockets.lock().unwrap().remove(&ws_id);
    if let Some(e) = entry {
        let _ = rt.ws_backend.close(ws_id, 1000, None);
        let _ = guest_free(caller, e.event_ptr);
        Ok(0)
    } else {
        Ok(-3)
    }
}

pub fn ws_close_impl(
    rt: &RuntimeState,
    ws_id: i32,
    code: i32,
    reason_ptr: i32,
) -> Result<i32> {
    if !rt.websockets.lock().unwrap().contains_key(&ws_id) {
        return Ok(-3);
    }
    let reason = if reason_ptr != 0 {
        Some(rt.memory.read_cstr(reason_ptr)?)
    } else {
        None
    };
    Ok(rt
        .ws_backend
        .close(ws_id, code as u16, reason.as_deref()))
}

pub fn ws_send_binary_impl(
    rt: &RuntimeState,
    ws_id: i32,
    data_ptr: i32,
    len: i32,
) -> Result<i32> {
    if !rt.websockets.lock().unwrap().contains_key(&ws_id) {
        return Ok(-3);
    }
    let bytes = rt.memory.read_bytes(data_ptr, len.max(0) as usize)?;
    Ok(rt.ws_backend.send_binary(ws_id, &bytes))
}

pub fn ws_send_text_impl(rt: &RuntimeState, ws_id: i32, text_ptr: i32) -> Result<i32> {
    if !rt.websockets.lock().unwrap().contains_key(&ws_id) {
        return Ok(-3);
    }
    let s = rt.memory.read_cstr(text_ptr)?;
    Ok(rt.ws_backend.send_text(ws_id, &s))
}

pub fn ws_set_cb_impl(
    rt: &RuntimeState,
    ws_id: i32,
    user_data: i32,
    cb: i32,
    thread: i32,
    kind: u8,
) -> i32 {
    let mut map = rt.websockets.lock().unwrap();
    let Some(entry) = map.get_mut(&ws_id) else {
        return -3;
    };
    let val = if cb != 0 {
        Some((cb, user_data, thread))
    } else {
        None
    };
    match kind {
        0 => entry.on_open = val,
        1 => entry.on_message = val,
        2 => entry.on_error = val,
        _ => entry.on_close = val,
    }
    0
}
