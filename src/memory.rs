use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicI32, AtomicU8, AtomicU64, Ordering};
use std::time::Duration;
use wasmtime::{
    AsContext, AsContextMut, Caller, Extern, Func, Instance, Ref, Result, SharedMemory,
    WaitResult, bail, format_err,
};

use crate::runtime::StoreState;

/// Потокобезопасная обёртка над `SharedMemory` Wasmtime.
/// Все чтения/записи выполняются через `AtomicU8::from_ptr` / `AtomicI32::from_ptr`,
/// что исключает UB от гонок данных в Rust 2024 при работе с многопоточным WASM.
#[derive(Clone)]
pub struct SharedMem {
    inner: SharedMemory,
}

#[allow(dead_code)]
impl SharedMem {
    pub fn new(inner: SharedMemory) -> Self {
        Self { inner }
    }

    pub fn raw(&self) -> &SharedMemory {
        &self.inner
    }

    pub fn byte_len(&self) -> usize {
        self.inner.data_size()
    }

    pub fn grow_to(&self, requested_bytes: u32) -> bool {
        let cur = self.inner.data_size();
        let req = requested_bytes as usize;
        if req <= cur {
            return true;
        }
        let delta_bytes = req - cur;
        let page_size = self.inner.page_size() as usize;
        let delta_pages = ((delta_bytes + page_size - 1) / page_size) as u64;
        self.inner.grow(delta_pages).is_ok()
    }

    fn slice(&self) -> &[UnsafeCell<u8>] {
        self.inner.data()
    }

    pub fn read_bytes(&self, ptr: i32, len: usize) -> Result<Vec<u8>> {
        let start = ptr as u32 as usize;
        let slice = self.slice();
        let end = start
            .checked_add(len)
            .ok_or_else(|| format_err!("memory read overflow: ptr={start:#x}, len={len}"))?;
        if end > slice.len() {
            bail!(
                "memory read out of bounds: {start:#x}..{end:#x} > {:#x}",
                slice.len()
            );
        }
        let mut out = Vec::with_capacity(len);
        let mut cur = start;
        let base_addr = slice.as_ptr() as usize;

        while cur < end && ((base_addr + cur) & 7) != 0 {
            // SAFETY: `slice[cur].get()` указывает на валидный байт в SharedMemory.
            let b = unsafe { AtomicU8::from_ptr(slice[cur].get()) }.load(Ordering::Relaxed);
            out.push(b);
            cur += 1;
        }
        while cur + 8 <= end {
            // SAFETY: `base_addr + cur` выровнен на 8 байт и `cur..cur+8` в пределах SharedMemory.
            let word = unsafe { AtomicU64::from_ptr(slice[cur].get().cast::<u64>()) }
                .load(Ordering::Relaxed);
            out.extend_from_slice(&word.to_le_bytes());
            cur += 8;
        }
        while cur < end {
            // SAFETY: `slice[cur].get()` указывает на валидный байт в SharedMemory.
            let b = unsafe { AtomicU8::from_ptr(slice[cur].get()) }.load(Ordering::Relaxed);
            out.push(b);
            cur += 1;
        }
        Ok(out)
    }

    pub fn write_bytes(&self, ptr: i32, bytes: &[u8]) -> Result<()> {
        let start = ptr as u32 as usize;
        let slice = self.slice();
        let end = start.checked_add(bytes.len()).ok_or_else(|| {
            format_err!(
                "memory write overflow: ptr={start:#x}, len={}",
                bytes.len()
            )
        })?;
        if end > slice.len() {
            bail!(
                "memory write out of bounds: {start:#x}..{end:#x} > {:#x}",
                slice.len()
            );
        }
        let mut cur = start;
        let mut src_idx = 0usize;
        let base_addr = slice.as_ptr() as usize;

        while cur < end && ((base_addr + cur) & 7) != 0 {
            // SAFETY: `slice[cur].get()` указывает на валидный байт в SharedMemory.
            unsafe { AtomicU8::from_ptr(slice[cur].get()) }
                .store(bytes[src_idx], Ordering::Relaxed);
            cur += 1;
            src_idx += 1;
        }
        while cur + 8 <= end {
            let mut chunk = [0u8; 8];
            chunk.copy_from_slice(&bytes[src_idx..src_idx + 8]);
            let word = u64::from_le_bytes(chunk);
            // SAFETY: `base_addr + cur` выровнен на 8 байт и `cur..cur+8` в пределах SharedMemory.
            unsafe { AtomicU64::from_ptr(slice[cur].get().cast::<u64>()) }
                .store(word, Ordering::Relaxed);
            cur += 8;
            src_idx += 8;
        }
        while cur < end {
            // SAFETY: `slice[cur].get()` указывает на валидный байт в SharedMemory.
            unsafe { AtomicU8::from_ptr(slice[cur].get()) }
                .store(bytes[src_idx], Ordering::Relaxed);
            cur += 1;
            src_idx += 1;
        }
        Ok(())
    }

    pub fn read_u8(&self, ptr: i32) -> Result<u8> {
        Ok(self.read_bytes(ptr, 1)?[0])
    }

    pub fn write_u8(&self, ptr: i32, val: u8) -> Result<()> {
        self.write_bytes(ptr, &[val])
    }

    pub fn read_u16(&self, ptr: i32) -> Result<u16> {
        let b = self.read_bytes(ptr, 2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn write_u16(&self, ptr: i32, val: u16) -> Result<()> {
        self.write_bytes(ptr, &val.to_le_bytes())
    }

    pub fn read_i32(&self, ptr: i32) -> Result<i32> {
        let b = self.read_bytes(ptr, 4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn read_u32(&self, ptr: i32) -> Result<u32> {
        Ok(self.read_i32(ptr)? as u32)
    }

    pub fn write_i32(&self, ptr: i32, val: i32) -> Result<()> {
        self.write_bytes(ptr, &val.to_le_bytes())
    }

    pub fn write_u32(&self, ptr: i32, val: u32) -> Result<()> {
        self.write_i32(ptr, val as i32)
    }

    pub fn read_i64(&self, ptr: i32) -> Result<i64> {
        let b = self.read_bytes(ptr, 8)?;
        Ok(i64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn read_u64(&self, ptr: i32) -> Result<u64> {
        Ok(self.read_i64(ptr)? as u64)
    }

    pub fn write_i64(&self, ptr: i32, val: i64) -> Result<()> {
        self.write_bytes(ptr, &val.to_le_bytes())
    }

    pub fn write_u64(&self, ptr: i32, val: u64) -> Result<()> {
        self.write_i64(ptr, val as i64)
    }

    pub fn read_f32(&self, ptr: i32) -> Result<f32> {
        let b = self.read_bytes(ptr, 4)?;
        Ok(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn write_f32(&self, ptr: i32, val: f32) -> Result<()> {
        self.write_bytes(ptr, &val.to_le_bytes())
    }

    pub fn read_f64(&self, ptr: i32) -> Result<f64> {
        let b = self.read_bytes(ptr, 8)?;
        Ok(f64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn write_f64(&self, ptr: i32, val: f64) -> Result<()> {
        self.write_bytes(ptr, &val.to_le_bytes())
    }

    pub fn atomic_load_i32(&self, ptr: i32) -> Result<i32> {
        let addr = ptr as u32 as usize;
        if addr % 4 != 0 {
            bail!("unaligned atomic_load_i32 at {addr:#x}");
        }
        let slice = self.slice();
        if addr + 4 > slice.len() {
            bail!("atomic_load_i32 out of bounds at {addr:#x}");
        }
        // SAFETY: адрес выровнен на 4 байта и находится в пределах SharedMemory.
        let atomic = unsafe { AtomicI32::from_ptr(slice[addr].get().cast::<i32>()) };
        Ok(atomic.load(Ordering::SeqCst))
    }

    pub fn atomic_store_i32(&self, ptr: i32, val: i32) -> Result<()> {
        let addr = ptr as u32 as usize;
        if addr % 4 != 0 {
            bail!("unaligned atomic_store_i32 at {addr:#x}");
        }
        let slice = self.slice();
        if addr + 4 > slice.len() {
            bail!("atomic_store_i32 out of bounds at {addr:#x}");
        }
        // SAFETY: адрес выровнен на 4 байта и находится в пределах SharedMemory.
        let atomic = unsafe { AtomicI32::from_ptr(slice[addr].get().cast::<i32>()) };
        atomic.store(val, Ordering::SeqCst);
        Ok(())
    }

    pub fn atomic_wait32(
        &self,
        ptr: i32,
        expected: i32,
        timeout: Option<Duration>,
    ) -> Result<WaitResult> {
        self.inner
            .atomic_wait32(ptr as u32 as u64, expected as u32, timeout)
            .map_err(|t| format_err!("atomic_wait32 trap: {t}"))
    }

    pub fn atomic_notify(&self, ptr: i32, count: u32) -> Result<u32> {
        self.inner
            .atomic_notify(ptr as u32 as u64, count)
            .map_err(|t| format_err!("atomic_notify trap: {t}"))
    }

    pub fn read_cstr(&self, ptr: i32) -> Result<String> {
        if ptr == 0 {
            return Ok(String::new());
        }
        let start = ptr as u32 as usize;
        let slice = self.slice();
        if start >= slice.len() {
            bail!("read_cstr pointer out of bounds: {start:#x}");
        }
        let mut bytes = Vec::new();
        for cell in &slice[start..] {
            // SAFETY: байтовый доступ через AtomicU8.
            let b = unsafe { AtomicU8::from_ptr(cell.get()) }.load(Ordering::Relaxed);
            if b == 0 {
                break;
            }
            bytes.push(b);
            if bytes.len() > 16 * 1024 * 1024 {
                bail!("read_cstr exceeded 16 MiB safety limit at {start:#x}");
            }
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn read_utf8(&self, ptr: i32, len: Option<usize>) -> Result<String> {
        if ptr == 0 {
            return Ok(String::new());
        }
        match len {
            Some(n) => {
                let bytes = self.read_bytes(ptr, n)?;
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            }
            None => self.read_cstr(ptr),
        }
    }

    pub fn write_cstr(&self, ptr: i32, s: &str, max_bytes: usize) -> Result<usize> {
        if ptr == 0 || max_bytes == 0 {
            return Ok(0);
        }
        let limit = max_bytes - 1;
        let bytes = s.as_bytes();
        let mut cut = bytes.len().min(limit);
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        self.write_bytes(ptr, &bytes[..cut])?;
        self.write_u8(ptr.wrapping_add(cut as i32), 0)?;
        Ok(cut)
    }
}

pub fn get_caller_instance(caller: &Caller<'_, StoreState>) -> Result<Instance> {
    caller
        .data()
        .instance
        .ok_or_else(|| format_err!("active instance is not yet attached to StoreState"))
}

pub fn get_export_func(caller: &mut Caller<'_, StoreState>, name: &str) -> Result<Func> {
    if let Some(Extern::Func(f)) = caller.get_export(name) {
        return Ok(f);
    }
    let inst = get_caller_instance(caller)?;
    inst.get_func(caller.as_context_mut(), name)
        .ok_or_else(|| format_err!("missing WASM export function `{name}`"))
}

pub fn get_indirect_func(caller: &mut Caller<'_, StoreState>, index: i32) -> Result<Func> {
    let table = match caller.get_export("__indirect_function_table") {
        Some(Extern::Table(t)) => t,
        _ => {
            let inst = get_caller_instance(caller)?;
            inst.get_table(caller.as_context_mut(), "__indirect_function_table")
                .ok_or_else(|| format_err!("missing `__indirect_function_table` export"))?
        }
    };
    let elem = table
        .get(caller.as_context_mut(), index as u32 as u64)
        .ok_or_else(|| format_err!("indirect table index {index} out of bounds"))?;
    match elem {
        Ref::Func(Some(func)) => Ok(func),
        _ => bail!("indirect table entry {index} is null or not a function"),
    }
}

/// Оборачивает аллокацию на главном потоке в `rbxJspiJsAllocatorEnter` / `Leave`.
pub fn with_jspi_allocator<R>(
    caller: &mut Caller<'_, StoreState>,
    f: impl FnOnce(&mut Caller<'_, StoreState>) -> Result<R>,
) -> Result<R> {
    let is_main = caller.data().is_main_thread;
    if is_main {
        if let Ok(enter) = get_export_func(caller, "rbxJspiJsAllocatorEnter") {
            let typed = enter.typed::<(), ()>(caller.as_context())?;
            let _ = typed.call(caller.as_context_mut(), ());
        }
    }
    let res = f(caller);
    if is_main {
        if let Ok(leave) = get_export_func(caller, "rbxJspiJsAllocatorLeave") {
            let typed = leave.typed::<(), ()>(caller.as_context())?;
            let _ = typed.call(caller.as_context_mut(), ());
        }
    }
    res
}

pub fn guest_malloc(caller: &mut Caller<'_, StoreState>, size: usize) -> Result<i32> {
    with_jspi_allocator(caller, |c| {
        let f = get_export_func(c, "malloc")?.typed::<i32, i32>(c.as_context())?;
        f.call(c.as_context_mut(), size as i32)
    })
}

#[allow(dead_code)]
pub fn guest_realloc(caller: &mut Caller<'_, StoreState>, ptr: i32, size: usize) -> Result<i32> {
    with_jspi_allocator(caller, |c| {
        let f = get_export_func(c, "realloc")?.typed::<(i32, i32), i32>(c.as_context())?;
        f.call(c.as_context_mut(), (ptr, size as i32))
    })
}

pub fn guest_free(caller: &mut Caller<'_, StoreState>, ptr: i32) -> Result<()> {
    if ptr == 0 {
        return Ok(());
    }
    with_jspi_allocator(caller, |c| {
        let f = get_export_func(c, "free")?.typed::<i32, ()>(c.as_context())?;
        f.call(c.as_context_mut(), ptr)
    })
}

pub fn guest_alloc_cstr(
    caller: &mut Caller<'_, StoreState>,
    mem: &SharedMem,
    s: &str,
) -> Result<i32> {
    let bytes = s.as_bytes();
    let ptr = guest_malloc(caller, bytes.len() + 1)?;
    if ptr != 0 {
        mem.write_bytes(ptr, bytes)?;
        mem.write_u8(ptr.wrapping_add(bytes.len() as i32), 0)?;
    }
    Ok(ptr)
}

pub fn instance_malloc(
    mut store: impl AsContextMut<Data = StoreState>,
    inst: Instance,
    is_main: bool,
    size: usize,
) -> Result<i32> {
    if is_main {
        if let Some(enter) = inst.get_func(&mut store, "rbxJspiJsAllocatorEnter") {
            let typed = enter.typed::<(), ()>(store.as_context())?;
            let _ = typed.call(&mut store, ());
        }
    }
    let f = inst.get_typed_func::<i32, i32>(&mut store, "malloc")?;
    let res = f.call(&mut store, size as i32);
    if is_main {
        if let Some(leave) = inst.get_func(&mut store, "rbxJspiJsAllocatorLeave") {
            let typed = leave.typed::<(), ()>(store.as_context())?;
            let _ = typed.call(&mut store, ());
        }
    }
    res
}

pub fn instance_free(
    mut store: impl AsContextMut<Data = StoreState>,
    inst: Instance,
    is_main: bool,
    ptr: i32,
) -> Result<()> {
    if ptr == 0 {
        return Ok(());
    }
    if is_main {
        if let Some(enter) = inst.get_func(&mut store, "rbxJspiJsAllocatorEnter") {
            let typed = enter.typed::<(), ()>(store.as_context())?;
            let _ = typed.call(&mut store, ());
        }
    }
    let f = inst.get_typed_func::<i32, ()>(&mut store, "free")?;
    let res = f.call(&mut store, ptr);
    if is_main {
        if let Some(leave) = inst.get_func(&mut store, "rbxJspiJsAllocatorLeave") {
            let typed = leave.typed::<(), ()>(store.as_context())?;
            let _ = typed.call(&mut store, ());
        }
    }
    res
}

#[allow(dead_code)]
pub fn instance_realloc(
    mut store: impl AsContextMut<Data = StoreState>,
    inst: Instance,
    is_main: bool,
    ptr: i32,
    size: usize,
) -> Result<i32> {
    if is_main {
        if let Some(enter) = inst.get_func(&mut store, "rbxJspiJsAllocatorEnter") {
            let typed = enter.typed::<(), ()>(store.as_context())?;
            let _ = typed.call(&mut store, ());
        }
    }
    let f = inst.get_typed_func::<(i32, i32), i32>(&mut store, "realloc")?;
    let res = f.call(&mut store, (ptr, size as i32));
    if is_main {
        if let Some(leave) = inst.get_func(&mut store, "rbxJspiJsAllocatorLeave") {
            let typed = leave.typed::<(), ()>(store.as_context())?;
            let _ = typed.call(&mut store, ());
        }
    }
    res
}