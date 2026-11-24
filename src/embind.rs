use wasmtime::{
    AsContextMut, Caller, Instance, Linker, Ref, Result, Val, ValType, bail, format_err,
};

use crate::memory::{instance_free, instance_malloc};
use crate::runtime::{
    EmbindFunctionInfo, EmbindTypeInfo, ExitStatus, LiveRuntimeUnwind, RuntimeState, StoreState,
};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum EmbindArg {
    Bool(bool),
    Int(i32),
    UInt(u32),
    BigInt(i64),
    UBigInt(u64),
    Float(f64),
    String(String),
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum EmbindRet {
    Void,
    Bool(bool),
    Int(i32),
    BigInt(i64),
    Float(f64),
    String(String),
}

pub fn has_function(rt: &RuntimeState, name: &str) -> bool {
    rt.embind.lock().unwrap().functions.contains_key(name)
}

pub fn call_function(
    mut store: impl AsContextMut<Data = StoreState>,
    instance: Instance,
    rt: &RuntimeState,
    name: &str,
    args: &[EmbindArg],
) -> Result<EmbindRet> {
    let (info, ret_kind, arg_metas) = {
        let reg = rt.embind.lock().unwrap();
        let overloads = reg
            .functions
            .get(name)
            .ok_or_else(|| format_err!("Embind function `{name}` is not registered"))?;
        let info = overloads
            .iter()
            .find(|f| f.arg_count == args.len())
            .cloned()
            .ok_or_else(|| {
                format_err!(
                    "Embind function `{name}` has no overload taking {} arg(s)",
                    args.len()
                )
            })?;
        let ret_kind = info
            .arg_types
            .first()
            .and_then(|id| reg.types.get(id))
            .map(|t| t.kind)
            .unwrap_or("void");
        let arg_metas: Vec<(&'static str, usize, bool)> = info
            .arg_types
            .iter()
            .skip(1)
            .map(|id| {
                reg.types
                    .get(id)
                    .map(|t| (t.kind, t.char_size, t.is_unsigned))
                    .unwrap_or(("unknown", 1, false))
            })
            .collect();
        (info, ret_kind, arg_metas)
    };

    let table = instance
        .get_table(&mut store, "__indirect_function_table")
        .ok_or_else(|| format_err!("missing `__indirect_function_table` export"))?;
    let elem = table
        .get(&mut store, info.raw_func_index as u32 as u64)
        .ok_or_else(|| format_err!("Embind func index {} out of bounds", info.raw_func_index))?;
    let Ref::Func(Some(func)) = elem else {
        bail!("Embind func index {} is null", info.raw_func_index);
    };

    let fn_ty = func.ty(store.as_context());
    let param_types: Vec<ValType> = fn_ty.params().collect();
    let res_len = fn_ty.results().len();

    let is_main = store.as_context().data().is_main_thread;
    let mut cleanup_ptrs: Vec<i32> = Vec::new();
    let mut wasm_args: Vec<Val> = Vec::with_capacity(1 + args.len());
    wasm_args.push(Val::I32(info.context_ptr));

    let sig_bytes = info.signature.as_bytes();
    for (idx, arg) in args.iter().enumerate() {
        let (kind, char_size, _is_unsigned) = arg_metas
            .get(idx)
            .copied()
            .unwrap_or(("unknown", 1, false));
        let sig_ch = sig_bytes.get(2 + idx).copied().unwrap_or(b'i');
        let expected_vt = param_types.get(1 + idx).cloned();

        match (kind, arg) {
            ("std::wstring", EmbindArg::String(s)) => {
                let ptr = if char_size == 2 {
                    let units: Vec<u16> = s.encode_utf16().collect();
                    let byte_len = units.len() * 2;
                    let p = instance_malloc(&mut store, instance, is_main, 4 + byte_len + 2)?;
                    if p == 0 {
                        for cp in cleanup_ptrs {
                            let _ = instance_free(&mut store, instance, is_main, cp);
                        }
                        bail!("OOM marshaling UTF-16 std::wstring for Embind `{name}`");
                    }
                    rt.memory.write_i32(p, units.len() as i32)?;
                    for (i, u) in units.iter().enumerate() {
                        rt.memory.write_u16(p + 4 + (i as i32 * 2), *u)?;
                    }
                    rt.memory.write_u16(p + 4 + byte_len as i32, 0)?;
                    p
                } else {
                    let cps: Vec<u32> = s.chars().map(|c| c as u32).collect();
                    let byte_len = cps.len() * 4;
                    let p = instance_malloc(&mut store, instance, is_main, 4 + byte_len + 4)?;
                    if p == 0 {
                        for cp in cleanup_ptrs {
                            let _ = instance_free(&mut store, instance, is_main, cp);
                        }
                        bail!("OOM marshaling UTF-32 std::wstring for Embind `{name}`");
                    }
                    rt.memory.write_i32(p, cps.len() as i32)?;
                    for (i, cp) in cps.iter().enumerate() {
                        rt.memory.write_u32(p + 4 + (i as i32 * 4), *cp)?;
                    }
                    rt.memory.write_u32(p + 4 + byte_len as i32, 0)?;
                    p
                };
                cleanup_ptrs.push(ptr);
                wasm_args.push(Val::I32(ptr));
            }
            ("std::string", EmbindArg::String(s)) | (_, EmbindArg::String(s)) => {
                let bytes = s.as_bytes();
                let ptr = instance_malloc(&mut store, instance, is_main, 4 + bytes.len() + 1)?;
                if ptr == 0 {
                    for p in cleanup_ptrs {
                        let _ = instance_free(&mut store, instance, is_main, p);
                    }
                    bail!("OOM marshaling std::string for Embind `{name}`");
                }
                rt.memory.write_i32(ptr, bytes.len() as i32)?;
                if !bytes.is_empty() {
                    rt.memory.write_bytes(ptr + 4, bytes)?;
                }
                rt.memory.write_u8(ptr + 4 + bytes.len() as i32, 0)?;
                cleanup_ptrs.push(ptr);
                wasm_args.push(Val::I32(ptr));
            }
            (_, other_arg) => {
                wasm_args.push(coerce_numeric_arg(other_arg, expected_vt.as_ref(), sig_ch));
            }
        }
    }

    let mut results = vec![Val::I32(0); res_len];
    let call_res = func.call(store.as_context_mut(), &wasm_args, &mut results);

    for p in cleanup_ptrs {
        let _ = instance_free(&mut store, instance, is_main, p);
    }
    if let Err(err) = call_res {
        if err.downcast_ref::<LiveRuntimeUnwind>().is_some()
            || err.downcast_ref::<ExitStatus>().is_some()
        {
            return Ok(EmbindRet::Void);
        }
        return Err(err);
    }

    if results.is_empty() || ret_kind == "void" {
        return Ok(EmbindRet::Void);
    }

    match (ret_kind, results[0]) {
        ("std::string", Val::I32(ptr)) => {
            if ptr == 0 {
                return Ok(EmbindRet::String(String::new()));
            }
            let len = rt.memory.read_i32(ptr)?.max(0) as usize;
            let bytes = if len > 0 {
                rt.memory.read_bytes(ptr + 4, len)?
            } else {
                Vec::new()
            };
            let _ = instance_free(&mut store, instance, is_main, ptr);
            Ok(EmbindRet::String(String::from_utf8_lossy(&bytes).into_owned()))
        }
        ("bool", Val::I32(v)) => Ok(EmbindRet::Bool(v != 0)),
        ("bigint", Val::I64(v)) => Ok(EmbindRet::BigInt(v)),
        ("float", Val::F32(bits)) => Ok(EmbindRet::Float(f32::from_bits(bits) as f64)),
        ("float", Val::F64(bits)) => Ok(EmbindRet::Float(f64::from_bits(bits))),
        (_, Val::I32(v)) => Ok(EmbindRet::Int(v)),
        (_, Val::I64(v)) => Ok(EmbindRet::BigInt(v)),
        (_, Val::F32(bits)) => Ok(EmbindRet::Float(f32::from_bits(bits) as f64)),
        (_, Val::F64(bits)) => Ok(EmbindRet::Float(f64::from_bits(bits))),
        _ => Ok(EmbindRet::Void),
    }
}

fn coerce_numeric_arg(arg: &EmbindArg, expected_vt: Option<&ValType>, sig_ch: u8) -> Val {
    match expected_vt {
        Some(ValType::I64) => match arg {
            EmbindArg::Bool(b) => Val::I64(i64::from(*b)),
            EmbindArg::Int(v) => Val::I64(*v as i64),
            EmbindArg::UInt(v) => Val::I64(*v as u64 as i64),
            EmbindArg::BigInt(v) => Val::I64(*v),
            EmbindArg::UBigInt(v) => Val::I64(*v as i64),
            EmbindArg::Float(v) => Val::I64(*v as i64),
            EmbindArg::String(_) => Val::I64(0),
        },
        Some(ValType::F64) => {
            let f = match arg {
                EmbindArg::Bool(b) => f64::from(u8::from(*b)),
                EmbindArg::Int(v) => *v as f64,
                EmbindArg::UInt(v) => *v as f64,
                EmbindArg::BigInt(v) => *v as f64,
                EmbindArg::UBigInt(v) => *v as f64,
                EmbindArg::Float(v) => *v,
                EmbindArg::String(_) => 0.0,
            };
            Val::F64(f.to_bits())
        }
        Some(ValType::F32) => {
            let f = match arg {
                EmbindArg::Bool(b) => f32::from(u8::from(*b)),
                EmbindArg::Int(v) => *v as f32,
                EmbindArg::UInt(v) => *v as f32,
                EmbindArg::BigInt(v) => *v as f32,
                EmbindArg::UBigInt(v) => *v as f32,
                EmbindArg::Float(v) => *v as f32,
                EmbindArg::String(_) => 0.0,
            };
            Val::F32(f.to_bits())
        }
        _ => match arg {
            EmbindArg::Bool(b) => Val::I32(i32::from(*b)),
            EmbindArg::Int(v) => {
                if sig_ch == b'j' {
                    Val::I64(*v as i64)
                } else {
                    Val::I32(*v)
                }
            }
            EmbindArg::UInt(v) => {
                if sig_ch == b'j' {
                    Val::I64(*v as u64 as i64)
                } else {
                    Val::I32(*v as i32)
                }
            }
            EmbindArg::BigInt(v) => Val::I64(*v),
            EmbindArg::UBigInt(v) => Val::I64(*v as i64),
            EmbindArg::Float(v) => {
                if sig_ch == b'f' {
                    Val::F32((*v as f32).to_bits())
                } else {
                    Val::F64(v.to_bits())
                }
            }
            EmbindArg::String(_) => Val::I32(0),
        },
    }
}

pub fn register(linker: &mut Linker<StoreState>) -> Result<()> {
    linker.func_wrap(
        "env",
        "_embind_register_void",
        |caller: Caller<'_, StoreState>, raw_type: i32, name_ptr: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            let name = rt.memory.read_cstr(name_ptr)?;
            rt.embind.lock().unwrap().types.insert(
                raw_type as u32,
                EmbindTypeInfo {
                    id: raw_type as u32,
                    name,
                    kind: "void",
                    char_size: 0,
                    is_unsigned: false,
                },
            );
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "_embind_register_bool",
        |caller: Caller<'_, StoreState>,
         raw_type: i32,
         name_ptr: i32,
         _true_val: i32,
         _false_val: i32|
         -> Result<()> {
            let rt = &caller.data().runtime;
            let name = rt.memory.read_cstr(name_ptr)?;
            rt.embind.lock().unwrap().types.insert(
                raw_type as u32,
                EmbindTypeInfo {
                    id: raw_type as u32,
                    name,
                    kind: "bool",
                    char_size: 1,
                    is_unsigned: true,
                },
            );
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "_embind_register_integer",
        |caller: Caller<'_, StoreState>,
         raw_type: i32,
         name_ptr: i32,
         size: i32,
         min_range: i32,
         _max_range: i32|
         -> Result<()> {
            let rt = &caller.data().runtime;
            let name = rt.memory.read_cstr(name_ptr)?;
            rt.embind.lock().unwrap().types.insert(
                raw_type as u32,
                EmbindTypeInfo {
                    id: raw_type as u32,
                    name,
                    kind: "integer",
                    char_size: size.max(1) as usize,
                    is_unsigned: min_range == 0,
                },
            );
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "_embind_register_bigint",
        |caller: Caller<'_, StoreState>,
         raw_type: i32,
         name_ptr: i32,
         size: i32,
         min_range: i64,
         _max_range: i64|
         -> Result<()> {
            let rt = &caller.data().runtime;
            let name = rt.memory.read_cstr(name_ptr)?;
            rt.embind.lock().unwrap().types.insert(
                raw_type as u32,
                EmbindTypeInfo {
                    id: raw_type as u32,
                    name,
                    kind: "bigint",
                    char_size: size.max(8) as usize,
                    is_unsigned: min_range == 0,
                },
            );
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "_embind_register_float",
        |caller: Caller<'_, StoreState>, raw_type: i32, name_ptr: i32, size: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            let name = rt.memory.read_cstr(name_ptr)?;
            rt.embind.lock().unwrap().types.insert(
                raw_type as u32,
                EmbindTypeInfo {
                    id: raw_type as u32,
                    name,
                    kind: "float",
                    char_size: size.max(4) as usize,
                    is_unsigned: false,
                },
            );
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "_embind_register_std_string",
        |caller: Caller<'_, StoreState>, raw_type: i32, name_ptr: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            let name = rt.memory.read_cstr(name_ptr)?;
            rt.embind.lock().unwrap().types.insert(
                raw_type as u32,
                EmbindTypeInfo {
                    id: raw_type as u32,
                    name,
                    kind: "std::string",
                    char_size: 1,
                    is_unsigned: true,
                },
            );
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "_embind_register_std_wstring",
        |caller: Caller<'_, StoreState>,
         raw_type: i32,
         char_size: i32,
         name_ptr: i32|
         -> Result<()> {
            let rt = &caller.data().runtime;
            let name = rt.memory.read_cstr(name_ptr)?;
            rt.embind.lock().unwrap().types.insert(
                raw_type as u32,
                EmbindTypeInfo {
                    id: raw_type as u32,
                    name,
                    kind: "std::wstring",
                    char_size: char_size.max(2) as usize,
                    is_unsigned: true,
                },
            );
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "_embind_register_emval",
        |caller: Caller<'_, StoreState>, raw_type: i32| {
            caller.data().runtime.embind.lock().unwrap().types.insert(
                raw_type as u32,
                EmbindTypeInfo {
                    id: raw_type as u32,
                    name: "emscripten::val".into(),
                    kind: "emval",
                    char_size: 4,
                    is_unsigned: true,
                },
            );
        },
    )?;

    linker.func_wrap(
        "env",
        "_embind_register_memory_view",
        |caller: Caller<'_, StoreState>,
         raw_type: i32,
         _typed_array_idx: i32,
         name_ptr: i32|
         -> Result<()> {
            let rt = &caller.data().runtime;
            let name = rt.memory.read_cstr(name_ptr)?;
            rt.embind.lock().unwrap().types.insert(
                raw_type as u32,
                EmbindTypeInfo {
                    id: raw_type as u32,
                    name,
                    kind: "memory_view",
                    char_size: 1,
                    is_unsigned: true,
                },
            );
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "_embind_register_function",
        |caller: Caller<'_, StoreState>,
         name_ptr: i32,
         arg_count: i32,
         arg_types_ptr: i32,
         sig_ptr: i32,
         raw_func_index: i32,
         context_ptr: i32,
         is_async: i32,
         _is_throw: i32|
         -> Result<()> {
            let rt = &caller.data().runtime;
            let raw_name = rt.memory.read_cstr(name_ptr)?;
            let name = raw_name.trim().split('(').next().unwrap_or("").to_owned();
            let signature = rt.memory.read_cstr(sig_ptr)?;
            let mut arg_types = Vec::with_capacity(arg_count.max(0) as usize);
            if arg_types_ptr != 0 {
                for i in 0..arg_count.max(0) {
                    arg_types.push(rt.memory.read_u32(arg_types_ptr + i * 4)?);
                }
            }
            let info = EmbindFunctionInfo {
                name: name.clone(),
                arg_count: (arg_count - 1).max(0) as usize,
                arg_types,
                signature,
                raw_func_index,
                context_ptr,
                is_async: is_async != 0,
            };
            let mut reg = rt.embind.lock().unwrap();
            let overloads = reg.functions.entry(name).or_default();
            if let Some(existing) = overloads.iter_mut().find(|f| f.arg_count == info.arg_count) {
                if caller.data().is_main_thread {
                    *existing = info;
                }
            } else {
                overloads.push(info);
            }
            Ok(())
        },
    )?;

    Ok(())
}
