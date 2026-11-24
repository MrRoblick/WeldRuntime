use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};
use wasmtime::{Caller, Error, Linker, Result};

use crate::memory::SharedMem;
use crate::runtime::{ExitStatus, RuntimeState, StoreState};

pub fn register(linker: &mut Linker<StoreState>) -> Result<()> {
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "proc_exit",
        |caller: Caller<'_, StoreState>, code: i32| -> Result<()> {
            proc_exit_impl(&caller.data().runtime, code)
        },
    )?;

    linker.func_wrap(
        "wasi_snapshot_preview1",
        "environ_sizes_get",
        |caller: Caller<'_, StoreState>, count_ptr: i32, buf_size_ptr: i32| -> Result<i32> {
            environ_sizes_get_impl(&caller.data().runtime, count_ptr, buf_size_ptr)
        },
    )?;

    linker.func_wrap(
        "wasi_snapshot_preview1",
        "environ_get",
        |caller: Caller<'_, StoreState>, environ_ptr: i32, buf_ptr: i32| -> Result<i32> {
            environ_get_impl(&caller.data().runtime, environ_ptr, buf_ptr)
        },
    )?;

    linker.func_wrap(
        "wasi_snapshot_preview1",
        "clock_time_get",
        |caller: Caller<'_, StoreState>, clock_id: i32, _precision: i64, out_ptr: i32| -> Result<i32> {
            let rt = &caller.data().runtime;
            let ns: u64 = match clock_id {
                0 => SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64,
                1..=3 => rt.start_time.elapsed().as_nanos() as u64,
                _ => return Ok(28), // EINVAL
            };
            rt.memory.write_u64(out_ptr, ns)?;
            Ok(0)
        },
    )?;

    linker.func_wrap(
        "wasi_snapshot_preview1",
        "random_get",
        |caller: Caller<'_, StoreState>, buf_ptr: i32, len: i32| -> Result<i32> {
            let n = len as u32 as usize;
            let mut buf = vec![0u8; n];
            fill_random_bytes(&caller.data().runtime, &mut buf);
            caller.data().runtime.memory.write_bytes(buf_ptr, &buf)?;
            Ok(0)
        },
    )?;

    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_pread",
        |_fd: i32, _iovs: i32, _iovs_len: i32, _offset: i64, _nread: i32| -> i32 { 52 },
    )?;

    linker.func_wrap("env", "__syscall_getegid32", || -> i32 { 0 })?;
    linker.func_wrap("env", "__syscall_geteuid32", || -> i32 { 0 })?;
    linker.func_wrap("env", "__syscall_getgid32", || -> i32 { 0 })?;
    linker.func_wrap("env", "__syscall_getuid32", || -> i32 { 0 })?;

    linker.func_wrap(
        "env",
        "emscripten_get_now",
        |caller: Caller<'_, StoreState>| -> f64 { caller.data().runtime.ms_since_start() },
    )?;

    linker.func_wrap("env", "emscripten_date_now", || -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            * 1000.0
    })?;

    linker.func_wrap(
        "env",
        "_tzset_js",
        |caller: Caller<'_, StoreState>, tz_ptr: i32, dst_ptr: i32, std_ptr: i32, dst_name_ptr: i32| -> Result<()> {
            let mem = &caller.data().runtime.memory;
            mem.write_i32(tz_ptr, 0)?;
            mem.write_i32(dst_ptr, 0)?;
            mem.write_cstr(std_ptr, "UTC+0000", 17)?;
            mem.write_cstr(dst_name_ptr, "UTC+0000", 17)?;
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "_gmtime_js",
        |caller: Caller<'_, StoreState>, time_sec: i64, tm_ptr: i32| -> Result<i32> {
            write_tm_utc(&caller.data().runtime.memory, time_sec, tm_ptr, false)
        },
    )?;

    linker.func_wrap(
        "env",
        "_localtime_js",
        |caller: Caller<'_, StoreState>, time_sec: i64, tm_ptr: i32| -> Result<i32> {
            write_tm_utc(&caller.data().runtime.memory, time_sec, tm_ptr, true)
        },
    )?;

    linker.func_wrap(
        "env",
        "_mktime_js",
        |caller: Caller<'_, StoreState>, tm_ptr: i32| -> Result<i64> {
            read_mktime_utc(&caller.data().runtime.memory, tm_ptr)
        },
    )?;

    Ok(())
}

pub fn proc_exit_impl(rt: &RuntimeState, code: i32) -> Result<()> {
    rt.exit_code.store(code, Ordering::SeqCst);
    rt.should_exit.store(true, Ordering::SeqCst);
    Err(Error::new(ExitStatus(code)))
}

pub fn environ_sizes_get_impl(
    rt: &RuntimeState,
    count_ptr: i32,
    buf_size_ptr: i32,
) -> Result<i32> {
    let vars = &rt.config.env_vars;
    let total_bytes: usize = vars.iter().map(|(k, v)| k.len() + 1 + v.len() + 1).sum();
    rt.memory.write_u32(count_ptr, vars.len() as u32)?;
    rt.memory.write_u32(buf_size_ptr, total_bytes as u32)?;
    Ok(0)
}

pub fn environ_get_impl(rt: &RuntimeState, environ_ptr: i32, buf_ptr: i32) -> Result<i32> {
    let mut ptr_cursor = environ_ptr;
    let mut buf_cursor = buf_ptr;
    for (k, v) in &rt.config.env_vars {
        let entry = format!("{k}={v}");
        rt.memory.write_i32(ptr_cursor, buf_cursor)?;
        rt.memory.write_bytes(buf_cursor, entry.as_bytes())?;
        rt.memory
            .write_u8(buf_cursor.wrapping_add(entry.len() as i32), 0)?;
        ptr_cursor = ptr_cursor.wrapping_add(4);
        buf_cursor = buf_cursor.wrapping_add((entry.len() + 1) as i32);
    }
    Ok(0)
}

pub fn fill_random_bytes(rt: &RuntimeState, out: &mut [u8]) {
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            if f.read_exact(out).is_ok() {
                return;
            }
        }
    }
    let mut i = 0;
    while i < out.len() {
        let state = RandomState::new();
        let mut hasher = state.build_hasher();
        hasher.write_u32(rt.rng_counter.fetch_add(1, Ordering::Relaxed));
        hasher.write_usize(i);
        let val = hasher.finish().to_le_bytes();
        let take = (out.len() - i).min(8);
        out[i..i + take].copy_from_slice(&val[..take]);
        i += take;
    }
}

pub fn next_random_u32(rt: &RuntimeState) -> u32 {
    let mut b = [0u8; 4];
    fill_random_bytes(rt, &mut b);
    u32::from_le_bytes(b)
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year, m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn is_leap_year(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn day_of_year(y: i64, m1: i64, d: i64) -> i64 {
    const CUM_NORM: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    const CUM_LEAP: [i64; 12] = [0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335];
    let idx = (m1 - 1).clamp(0, 11) as usize;
    let base = if is_leap_year(y) {
        CUM_LEAP[idx]
    } else {
        CUM_NORM[idx]
    };
    base + d - 1
}

fn write_tm_utc(mem: &SharedMem, time_sec: i64, tm_ptr: i32, include_tz: bool) -> Result<i32> {
    let days = time_sec.div_euclid(86400);
    let rem = time_sec.rem_euclid(86400);
    let hour = (rem / 3600) as i32;
    let min = ((rem % 3600) / 60) as i32;
    let sec = (rem % 60) as i32;
    let (year, month, day) = civil_from_days(days);
    let wday = ((days + 4).rem_euclid(7)) as i32;
    let yday = day_of_year(year, month, day) as i32;

    mem.write_i32(tm_ptr, sec)?;
    mem.write_i32(tm_ptr + 4, min)?;
    mem.write_i32(tm_ptr + 8, hour)?;
    mem.write_i32(tm_ptr + 12, day as i32)?;
    mem.write_i32(tm_ptr + 16, (month - 1) as i32)?;
    mem.write_i32(tm_ptr + 20, (year - 1900) as i32)?;
    mem.write_i32(tm_ptr + 24, wday)?;
    mem.write_i32(tm_ptr + 28, yday)?;
    if include_tz {
        mem.write_i32(tm_ptr + 32, 0)?;
        mem.write_i32(tm_ptr + 36, 0)?;
    }
    Ok(0)
}

fn read_mktime_utc(mem: &SharedMem, tm_ptr: i32) -> Result<i64> {
    let sec = mem.read_i32(tm_ptr)? as i64;
    let min = mem.read_i32(tm_ptr + 4)? as i64;
    let hour = mem.read_i32(tm_ptr + 8)? as i64;
    let mday = mem.read_i32(tm_ptr + 12)? as i64;
    let mon0 = mem.read_i32(tm_ptr + 16)? as i64;
    let year1900 = mem.read_i32(tm_ptr + 20)? as i64;

    let extra_years = mon0.div_euclid(12);
    let mon_norm = mon0.rem_euclid(12) + 1;
    let year = 1900 + year1900 + extra_years;
    let total_days = days_from_civil(year, mon_norm, 1) + (mday - 1);
    let total_sec = total_days * 86400 + hour * 3600 + min * 60 + sec;

    let _ = write_tm_utc(mem, total_sec, tm_ptr, true);
    Ok(total_sec)
}