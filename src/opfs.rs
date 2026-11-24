use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use wasmtime::{Caller, Linker, Result};

use crate::memory::{guest_free, guest_malloc};
use crate::runtime::{OpfsCachedHandle, RuntimeState, StoreState};

fn sanitize_opfs_name(name: &str) -> Option<String> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return None;
    }
    Some(name.to_owned())
}

fn opfs_file_path(rt: &RuntimeState, name: &str) -> Option<PathBuf> {
    sanitize_opfs_name(name).map(|safe| rt.config.opfs_dir.join(safe))
}

pub fn register(linker: &mut Linker<StoreState>) -> Result<()> {
    linker.func_wrap(
        "env",
        "rbx_opfs_install_helpers",
        |caller: Caller<'_, StoreState>, max_entries: i32, keep_pct: i32| {
            let rt = &caller.data().runtime;
            {
                let mut opfs = rt.opfs.lock().unwrap();
                opfs.max_handles = max_entries;
                opfs.keep_pct = keep_pct;
            }
            let _ = fs::create_dir_all(&rt.config.opfs_dir);
        },
    )?;

    linker.func_wrap(
        "env",
        "__asyncjs__rbx_opfs_open",
        |caller: Caller<'_, StoreState>, name_ptr: i32, no_sync: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            let name = rt.memory.read_cstr(name_ptr)?;
            let mut opfs = rt.opfs.lock().unwrap();
            if let Some(entry) = opfs.sync_handles.get_mut(&name) {
                entry.ref_count += 1;
                return Ok(());
            }
            let file = if no_sync == 0 {
                opfs_file_path(rt, &name).and_then(|p| fs::File::open(p).ok())
            } else {
                None
            };
            if file.is_some() || no_sync != 0 {
                opfs.sync_handles.insert(
                    name,
                    OpfsCachedHandle {
                        file,
                        ref_count: 1,
                    },
                );
            }
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "rbx_opfs_close",
        |caller: Caller<'_, StoreState>, name_ptr: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            let name = rt.memory.read_cstr(name_ptr)?;
            let mut opfs = rt.opfs.lock().unwrap();
            let remove = if let Some(entry) = opfs.sync_handles.get_mut(&name) {
                entry.ref_count = entry.ref_count.saturating_sub(1);
                entry.ref_count == 0
            } else {
                false
            };
            if remove {
                opfs.sync_handles.remove(&name);
            }
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "rbx_opfs_read_sync_if_cached",
        |caller: Caller<'_, StoreState>,
         name_ptr: i32,
         dst_ptr: i32,
         offset: i32,
         len: i32|
         -> Result<i32> {
            let rt = &caller.data().runtime;
            let name = rt.memory.read_cstr(name_ptr)?;
            if len <= 0 || offset < 0 {
                return Ok(0);
            }
            let mut opfs = rt.opfs.lock().unwrap();
            let Some(entry) = opfs.sync_handles.get_mut(&name) else {
                return Ok(-1);
            };
            let Some(file) = entry.file.as_mut() else {
                return Ok(-1);
            };
            let size = match file.metadata() {
                Ok(m) => m.len(),
                Err(_) => {
                    entry.file = None;
                    return Ok(-1);
                }
            };
            let off = offset as u64;
            if off >= size {
                return Ok(0);
            }
            if file.seek(SeekFrom::Start(off)).is_err() {
                entry.file = None;
                return Ok(-1);
            }
            let to_read = (len as u32 as usize).min((size - off) as usize);
            let mut buf = vec![0u8; to_read];
            match file.read(&mut buf) {
                Ok(n) => {
                    rt.memory.write_bytes(dst_ptr, &buf[..n])?;
                    Ok(n as i32)
                }
                Err(_) => {
                    entry.file = None;
                    Ok(-1)
                }
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "rbx_opfs_free",
        |mut caller: Caller<'_, StoreState>, ptr: i32| -> Result<()> {
            guest_free(&mut caller, ptr)
        },
    )?;

    linker.func_wrap(
        "env",
        "__asyncjs__rbx_opfs_clear",
        |caller: Caller<'_, StoreState>| -> i32 {
            let rt = &caller.data().runtime;
            rt.opfs.lock().unwrap().sync_handles.clear();
            let _ = fs::remove_dir_all(&rt.config.opfs_dir);
            1
        },
    )?;

    linker.func_wrap(
        "env",
        "__asyncjs__rbx_opfs_contains",
        |caller: Caller<'_, StoreState>, name_ptr: i32| -> Result<i32> {
            let rt = &caller.data().runtime;
            let name = rt.memory.read_cstr(name_ptr)?;
            let exists = opfs_file_path(rt, &name)
                .and_then(|p| fs::metadata(p).ok())
                .map(|m| m.is_file() && m.len() > 0)
                .unwrap_or(false);
            Ok(i32::from(exists))
        },
    )?;

    linker.func_wrap(
        "env",
        "__asyncjs__rbx_opfs_file_size",
        |caller: Caller<'_, StoreState>, name_ptr: i32| -> Result<i32> {
            let rt = &caller.data().runtime;
            let name = rt.memory.read_cstr(name_ptr)?;
            let size = opfs_file_path(rt, &name)
                .and_then(|p| fs::metadata(p).ok())
                .map(|m| m.len() as i32)
                .unwrap_or(0);
            Ok(size)
        },
    )?;

    linker.func_wrap(
        "env",
        "__asyncjs__rbx_opfs_set",
        |caller: Caller<'_, StoreState>, name_ptr: i32, src_ptr: i32, len: i32| -> Result<i32> {
            let rt = &caller.data().runtime;
            if len <= 0 || src_ptr == 0 {
                return Ok(0);
            }
            let name = rt.memory.read_cstr(name_ptr)?;
            let Some(path) = opfs_file_path(rt, &name) else {
                return Ok(0);
            };
            if fs::create_dir_all(&rt.config.opfs_dir).is_err() {
                return Ok(0);
            }
            let data = rt.memory.read_bytes(src_ptr, len as u32 as usize)?;
            let tmp_path = path.with_extension("tmp");
            let write_ok = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)
                .and_then(|mut f| {
                    f.write_all(&data)?;
                    f.flush()
                })
                .and_then(|_| fs::rename(&tmp_path, &path))
                .is_ok();
            Ok(i32::from(write_ok))
        },
    )?;

    linker.func_wrap(
        "env",
        "__asyncjs__rbx_opfs_get",
        |mut caller: Caller<'_, StoreState>, name_ptr: i32, out_len_ptr: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            rt.memory.write_i32(out_len_ptr, 0)?;
            let name = rt.memory.read_cstr(name_ptr)?;
            println!("[OPFS:get] {name}");
            let Some(path) = opfs_file_path(&rt, &name) else {
                return Ok(0);
            };
            let Ok(data) = fs::read(path) else {
                return Ok(0);
            };
            if data.is_empty() {
                return Ok(0);
            }
            let ptr = guest_malloc(&mut caller, data.len())?;
            if ptr == 0 {
                return Ok(0);
            }
            if rt.memory.write_bytes(ptr, &data).is_err() {
                let _ = guest_free(&mut caller, ptr);
                return Ok(0);
            }
            rt.memory.write_i32(out_len_ptr, data.len() as i32)?;
            Ok(ptr)
        },
    )?;

    linker.func_wrap(
        "env",
        "__asyncjs__rbx_opfs_get_offset",
        |mut caller: Caller<'_, StoreState>,
         name_ptr: i32,
         out_len_ptr: i32,
         offset: i32,
         len: i32|
         -> Result<i32> {
            let rt = caller.data().runtime.clone();
            rt.memory.write_i32(out_len_ptr, 0)?;
            if offset < 0 || len <= 0 {
                return Ok(0);
            }
            let name = rt.memory.read_cstr(name_ptr)?;
            let Some(path) = opfs_file_path(&rt, &name) else {
                return Ok(0);
            };
            let Ok(mut f) = fs::File::open(path) else {
                return Ok(0);
            };
            let Ok(meta) = f.metadata() else {
                return Ok(0);
            };
            let size = meta.len();
            let off = offset as u64;
            if off >= size {
                return Ok(0);
            }
            let to_read = (len as u32 as usize).min((size - off) as usize);
            if to_read == 0 || f.seek(SeekFrom::Start(off)).is_err() {
                return Ok(0);
            }
            let mut buf = vec![0u8; to_read];
            let Ok(n) = f.read(&mut buf) else {
                return Ok(0);
            };
            if n == 0 {
                return Ok(0);
            }
            let ptr = guest_malloc(&mut caller, n)?;
            if ptr == 0 {
                return Ok(0);
            }
            if rt.memory.write_bytes(ptr, &buf[..n]).is_err() {
                let _ = guest_free(&mut caller, ptr);
                return Ok(0);
            }
            rt.memory.write_i32(out_len_ptr, n as i32)?;
            Ok(ptr)
        },
    )?;

    linker.func_wrap(
        "env",
        "__asyncjs__rbx_opfs_get_buffer",
        |caller: Caller<'_, StoreState>, name_ptr: i32, dst_ptr: i32, max_len: i32| -> Result<i32> {
            let rt = &caller.data().runtime;
            if max_len <= 0 {
                return Ok(0);
            }
            let name = rt.memory.read_cstr(name_ptr)?;
            let Some(path) = opfs_file_path(rt, &name) else {
                return Ok(-1);
            };
            let Ok(mut f) = fs::File::open(path) else {
                return Ok(-1);
            };
            let mut buf = vec![0u8; max_len as u32 as usize];
            match f.read(&mut buf) {
                Ok(n) => {
                    rt.memory.write_bytes(dst_ptr, &buf[..n])?;
                    Ok(n as i32)
                }
                Err(_) => Ok(-1),
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "__asyncjs__rbx_opfs_get_buffer_offset",
        |caller: Caller<'_, StoreState>,
         name_ptr: i32,
         dst_ptr: i32,
         buf_len: i32,
         offset: i32,
         len: i32|
         -> Result<i32> {
            let rt = &caller.data().runtime;
            let count = buf_len.min(len);
            if count <= 0 || offset < 0 {
                return Ok(0);
            }
            let name = rt.memory.read_cstr(name_ptr)?;
            let Some(path) = opfs_file_path(rt, &name) else {
                return Ok(-1);
            };
            let Ok(mut f) = fs::File::open(path) else {
                return Ok(-1);
            };
            let Ok(meta) = f.metadata() else {
                return Ok(-1);
            };
            let off = offset as u64;
            if off >= meta.len() {
                return Ok(0);
            }
            if f.seek(SeekFrom::Start(off)).is_err() {
                return Ok(-1);
            }
            let to_read = (count as u32 as usize).min((meta.len() - off) as usize);
            let mut buf = vec![0u8; to_read];
            match f.read(&mut buf) {
                Ok(n) => {
                    rt.memory.write_bytes(dst_ptr, &buf[..n])?;
                    Ok(n as i32)
                }
                Err(_) => Ok(-1),
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "__asyncjs__rbx_opfs_remove",
        |caller: Caller<'_, StoreState>, name_ptr: i32| -> Result<i32> {
            let rt = &caller.data().runtime;
            let name = rt.memory.read_cstr(name_ptr)?;
            let Some(path) = opfs_file_path(rt, &name) else {
                return Ok(0);
            };
            Ok(i32::from(fs::remove_file(path).is_ok()))
        },
    )?;

    linker.func_wrap(
        "env",
        "__asyncjs__rbx_opfs_remove_batch",
        |caller: Caller<'_, StoreState>, list_ptr: i32| -> Result<i32> {
            let rt = &caller.data().runtime;
            let raw = rt.memory.read_cstr(list_ptr)?;
            let mut count = 0;
            for line in raw.lines() {
                if let Some(path) = opfs_file_path(rt, line) {
                    if fs::remove_file(path).is_ok() {
                        count += 1;
                    }
                }
            }
            Ok(count)
        },
    )?;

    linker.func_wrap(
        "env",
        "__asyncjs__rbx_opfs_list",
        |mut caller: Caller<'_, StoreState>, out_total_bytes_ptr: i32, out_count_ptr: i32| -> Result<i32> {
            opfs_list_impl(&mut caller, out_total_bytes_ptr, out_count_ptr, false)
        },
    )?;

    linker.func_wrap(
        "env",
        "__asyncjs__rbx_opfs_list_sized",
        |mut caller: Caller<'_, StoreState>, out_total_bytes_ptr: i32, out_count_ptr: i32| -> Result<i32> {
            opfs_list_impl(&mut caller, out_total_bytes_ptr, out_count_ptr, true)
        },
    )?;

    Ok(())
}

fn opfs_list_impl(
    caller: &mut Caller<'_, StoreState>,
    out_total_bytes_ptr: i32,
    out_count_ptr: i32,
    with_size: bool,
) -> Result<i32> {
    let rt = caller.data().runtime.clone();
    rt.memory.write_f64(out_total_bytes_ptr, 0.0)?;
    rt.memory.write_i32(out_count_ptr, 0)?;
    let Ok(entries) = fs::read_dir(&rt.config.opfs_dir) else {
        return Ok(0);
    };
    let mut items = Vec::new();
    let mut total_size: u64 = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() || meta.len() == 0 {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        total_size += meta.len();
        if with_size {
            items.push(format!("{}\t{}", meta.len(), name));
        } else {
            items.push(name);
        }
    }
    if items.is_empty() {
        return Ok(0);
    }
    let joined = items.join("\n");
    let ptr = guest_malloc(caller, joined.len() + 1)?;
    if ptr == 0 {
        return Ok(0);
    }
    if rt
        .memory
        .write_cstr(ptr, &joined, joined.len() + 1)
        .is_err()
    {
        let _ = guest_free(caller, ptr);
        return Ok(0);
    }
    rt.memory.write_f64(out_total_bytes_ptr, total_size as f64)?;
    rt.memory.write_i32(out_count_ptr, items.len() as i32)?;
    Ok(ptr)
}