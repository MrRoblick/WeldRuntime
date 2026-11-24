use std::sync::atomic::Ordering;
use wasmtime::{Caller, Linker, Result};

use crate::memory::SharedMem;
use crate::runtime::StoreState;
use crate::wasi;

#[derive(Debug, Clone, Copy)]
pub enum EmAsmArg {
    Int(i64),
    Float(f64),
}

impl EmAsmArg {
    pub fn as_i32(self) -> i32 {
        match self {
            EmAsmArg::Int(v) => v as i32,
            EmAsmArg::Float(v) => v as i32,
        }
    }
}

/// Декодер C varargs по строке сигнатуры Emscripten (`'i'`, `'p'`, `'j'`, `'d'`, `'f'`).
pub fn read_em_asm_varargs(
    mem: &SharedMem,
    sig_ptr: i32,
    varargs_ptr: i32,
) -> Result<Vec<EmAsmArg>> {
    if sig_ptr == 0 {
        return Ok(Vec::new());
    }
    let sig = mem.read_cstr(sig_ptr)?;
    let mut cursor = varargs_ptr as u32;
    let mut args = Vec::with_capacity(sig.len());

    for b in sig.bytes() {
        let is_64_or_float = b != b'i' && b != b'p';
        if is_64_or_float && (cursor % 8 != 0) {
            cursor = cursor.wrapping_add(4);
        }
        match b {
            b'p' => {
                let v = mem.read_u32(cursor as i32)?;
                args.push(EmAsmArg::Int(v as i64));
            }
            b'i' => {
                let v = mem.read_i32(cursor as i32)?;
                args.push(EmAsmArg::Int(v as i64));
            }
            b'j' => {
                let v = mem.read_i64(cursor as i32)?;
                args.push(EmAsmArg::Int(v));
            }
            _ => {
                let v = mem.read_f64(cursor as i32)?;
                args.push(EmAsmArg::Float(v));
            }
        }
        cursor = cursor.wrapping_add(if is_64_or_float { 8 } else { 4 });
    }
    Ok(args)
}

pub fn dispatch_em_asm(
    caller: &mut Caller<'_, StoreState>,
    code_ptr: i32,
    args: &[EmAsmArg],
) -> Result<f64> {
    let rt = caller.data().runtime.clone();
    let arg_i32 = |idx: usize| args.get(idx).copied().map(EmAsmArg::as_i32).unwrap_or(0);

    match code_ptr {
        // navigator.webdriver ? 1 : 0
        9098847 => Ok(0.0),
        // [WASM-TIMING] main() entered
        9098887 => {
            println!("[WASM-TIMING] main() entered: +{:.1}ms", rt.ms_since_start());
            Ok(0.0)
        }
        // __onExperienceLeft
        9099052 => {
            println!("[Roblox] Experience left (status={})", arg_i32(0));
            Ok(0.0)
        }
        // __onEngineLoadingStatus(isFailure, source, msgPtr, errorCode, statusCode)
        9099118 => {
            let is_failure = arg_i32(0) != 0;
            let src = match arg_i32(1) {
                1 => "join",
                2 => "teleport",
                _ => "gui-service",
            };
            let msg = rt.memory.read_cstr(arg_i32(2)).unwrap_or_default();
            println!(
                "[Roblox][LoadingStatus] kind={} source={} msg={:?} err={} status={}",
                if is_failure { "failure" } else { "status" },
                src,
                msg,
                arg_i32(3),
                arg_i32(4)
            );
            Ok(0.0)
        }
        // __resolveGameExit: native_exit
        9099404 | 9099461 => {
            println!("[Roblox] Game exit: native_exit");
            rt.should_exit.store(true, Ordering::SeqCst);
            Ok(0.0)
        }
        // teleport_failed
        9099518 => {
            println!("[Roblox] Game exit: teleport_failed ({})", arg_i32(0));
            rt.should_exit.store(true, Ordering::SeqCst);
            Ok(0.0)
        }
        // __onDefaultLoadingScreenRemoved
        9099595 => {
            println!("[Roblox] Default loading screen removed ({})", arg_i32(0));
            Ok(0.0)
        }
        // performance.now()
        9099687 => Ok(rt.ms_since_start()),
        // join_failed
        9099717 => {
            println!("[Roblox] Game exit: join_failed ({})", arg_i32(0));
            rt.should_exit.store(true, Ordering::SeqCst);
            Ok(0.0)
        }
        // window.__wasmPreInitTime
        9099790 => Ok(0.0),
        // __onExperienceJoined
        9099832 => {
            println!("[Roblox] Experience joined ({})", arg_i32(0));
            Ok(0.0)
        }
        // leave
        9099902 => {
            println!("[Roblox] Game exit: leave");
            rt.should_exit.store(true, Ordering::SeqCst);
            Ok(0.0)
        }
        // TabPrompt / WebView UI / Fullscreen
        9099953 | 9102932 | 9103046 | 9103637 | 9104828 | 9106623 | 9107053 | 9111537 => {
            Ok(0.0)
        }
        // crash exit
        9111863 => {
            let err_type = rt.memory.read_cstr(arg_i32(0)).unwrap_or_default();
            let err_msg = rt.memory.read_cstr(arg_i32(1)).unwrap_or_default();
            eprintln!("[Roblox][CrashExit] {err_type}: {err_msg}");
            rt.should_exit.store(true, Ordering::SeqCst);
            Ok(0.0)
        }
        // FMOD AudioWorklet / ScriptProcessor
        // 9111979: DSP buffer size override (Android check -> 0 on desktop)
        9111979 => Ok(0.0),
        // 9112441: Probe AudioContext + AudioWorkletNode + audioWorklet.addModule
        9112441 => Ok(i32::from(rt.audio_backend.worklet_supported()) as f64),
        // 9112730: mContext.resume() -> addModule -> new AudioWorkletNode -> postMessage(mFastpathPayload)
        9112730 => {
            rt.audio_backend.worklet_create_node();
            Ok(0.0)
        }
        // 9113734: waitForAudioWorklet -> mWorkletNode.connect(mContext.destination)
        9113734 => {
            rt.audio_backend.worklet_connect();
            Ok(0.0)
        }
        // 9113952: Define window.waitForAudioWorklet helper
        9113952 => Ok(0.0),
        // 9114224: (mContext && mContext.resume(), mWorkletNode) — returns non-zero if worklet node already exists
        9114224 => Ok(rt.audio_backend.worklet_resume_and_check_node() as f64),
        // 9114286: Create AudioContext and return destination.maxChannelCount
        9114286 => Ok(rt.audio_backend.worklet_open_context() as f64),
        // 9114701: Bind FMOD_JS_Mixer{Slowpath,Fastpath}Function and return mContext.sampleRate
        9114701 => Ok(rt.audio_backend.worklet_get_sample_rate(arg_i32(0) != 0) as f64),
        // 9115094: AudioWorklet slowpath planar -> interleaved copy (ptr, channels, frames)
        9115094 => {
            rt.audio_backend.worklet_write_slowpath(
                &rt.memory,
                arg_i32(0),
                arg_i32(1),
                arg_i32(2),
            );
            Ok(0.0)
        }
        // 9115514: Temporary AudioContext probe -> sampleRate
        9115514 => Ok(rt.audio_backend.sample_rate() as f64),
        // 9115715: Store mFastpathPayload { dataOffset, syncOffset, channelCount, bufferSize }
        9115715 => {
            rt.audio_backend.worklet_set_fastpath_payload(
                arg_i32(0),
                arg_i32(1),
                arg_i32(2),
                arg_i32(3),
            );
            Ok(0.0)
        }
        // 9115926: Close AudioWorkletNode and AudioContext
        9115926 => {
            rt.audio_backend.worklet_close();
            Ok(0.0)
        }
        // 9116344: AudioWorklet slowpath sliced copy (ptr, channels, frames, sliceIndex)
        9116344 => {
            rt.audio_backend.worklet_write_slowpath_slice(
                &rt.memory,
                arg_i32(0),
                arg_i32(1),
                arg_i32(2),
                arg_i32(3),
            );
            Ok(0.0)
        }
        // 9116868: Check SharedArrayBuffer && crossOriginIsolated && audioProcessorUrl (speakerChannels, bufferSize)
        9116868 => Ok(rt.audio_backend.worklet_prepare(arg_i32(0), arg_i32(1)) as f64),
        // 9117251: mContext.suspend()
        9117251 => {
            rt.audio_backend.worklet_suspend();
            Ok(0.0)
        }
        // 9117293: Stop/disconnect AudioWorkletNode
        9117293 => {
            rt.audio_backend.worklet_disconnect();
            Ok(0.0)
        }
        // 9117579: _as_script_node.disconnect(context.destination)
        9117579 => {
            rt.audio_backend.script_disconnect();
            Ok(0.0)
        }
        // 9117632: ScriptProcessor context.suspend()
        9117632 => {
            rt.audio_backend.script_suspend();
            Ok(0.0)
        }
        // 9117655: ScriptProcessor temporary AudioContext probe -> sampleRate
        9117655 => Ok(rt.audio_backend.sample_rate() as f64),
        // 9117856: context.createScriptProcessor(bufferSize, 0, channels) -> args: (channels, bufferSize)
        9117856 => {
            rt.audio_backend.script_create_processor(arg_i32(0), arg_i32(1));
            Ok(0.0)
        }
        // 9117920: Copy interleaved FMOD mix block into _as_output_buffer (srcPtr, dstOffset, frames, channels)
        9117920 => {
            rt.audio_backend.script_write_interleaved(
                &rt.memory,
                arg_i32(0),
                arg_i32(1),
                arg_i32(2),
                arg_i32(3),
            );
            Ok(0.0)
        }
        // 9118195: ScriptProcessor context.resume()
        9118195 => {
            rt.audio_backend.script_resume();
            Ok(0.0)
        }
        // 9118217: Create ScriptProcessor AudioContext + bind FMOD_JS_MixFunction -> sampleRate
        9118217 => Ok(rt.audio_backend.script_open_context() as f64),
        // 9118423: _as_script_node.connect(context.destination) & start onaudioprocess callbacks
        9118423 => {
            rt.audio_backend.script_connect();
            Ok(0.0)
        }
        // WebGPU preinitialized device check
        9118661 => Ok(i32::from(rt.config.enable_webgpu) as f64),
        9118718 | 9118945 => Ok(0.0),
        // IXP settings save
        9119019 => {
            let len = arg_i32(1).max(0) as usize;
            if let Ok(s) = rt.memory.read_utf8(arg_i32(0), Some(len)) {
                let _ = std::fs::create_dir_all(&rt.config.opfs_dir);
                let _ = std::fs::write(rt.config.opfs_dir.join("ixp_settings.json"), s.as_bytes());
                println!("[IXP] settings saved ({} bytes)", s.len());
            }
            Ok(0.0)
        }
        // non-allowlisted header warning
        9119097 => {
            if let Ok(h) = rt.memory.read_cstr(arg_i32(0)) {
                eprintln!("[wasm-cors] non-allowlisted header: {h}");
            }
            Ok(0.0)
        }
        // MicroProfiler dump
        9119194 => {
            let name = rt
                .memory
                .read_cstr(arg_i32(0))
                .unwrap_or_else(|_| "MicroProfilerDump.html".into());
            let ptr = arg_i32(1);
            let len = arg_i32(2).max(0) as usize;
            if let Ok(data) = rt.memory.read_bytes(ptr, len) {
                let _ = std::fs::write(&name, data);
                println!("[MicroProfile] dumped {len} bytes to {name}");
            }
            Ok(0.0)
        }
        // wasmOffsetConverter
        9119603 => Ok(0.0),
        // CSPRNG init & getRandomValue()
        9119660 => Ok(0.0),
        9120436 => Ok(wasi::next_random_u32(&rt) as f64),
        other => {
            let snippet = rt.memory.read_cstr(other).unwrap_or_default();
            if !snippet.is_empty() {
                eprintln!("[EM_ASM] unhandled id={other} code={snippet:?}");
            }
            Ok(0.0)
        }
    }
}

pub fn register(linker: &mut Linker<StoreState>) -> Result<()> {
    linker.func_wrap(
        "env",
        "emscripten_asm_const_int",
        |mut caller: Caller<'_, StoreState>,
         code_ptr: i32,
         sig_ptr: i32,
         varargs_ptr: i32|
         -> Result<i32> {
            let mem = caller.data().runtime.memory.clone();
            let args = read_em_asm_varargs(&mem, sig_ptr, varargs_ptr)?;
            let res = dispatch_em_asm(&mut caller, code_ptr, &args)?;
            Ok(res as i64 as i32)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_asm_const_double",
        |mut caller: Caller<'_, StoreState>,
         code_ptr: i32,
         sig_ptr: i32,
         varargs_ptr: i32|
         -> Result<f64> {
            let mem = caller.data().runtime.memory.clone();
            let args = read_em_asm_varargs(&mem, sig_ptr, varargs_ptr)?;
            dispatch_em_asm(&mut caller, code_ptr, &args)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_asm_const_async_on_main_thread",
        |mut caller: Caller<'_, StoreState>,
         code_ptr: i32,
         sig_ptr: i32,
         varargs_ptr: i32|
         -> Result<()> {
            let mem = caller.data().runtime.memory.clone();
            let args = read_em_asm_varargs(&mem, sig_ptr, varargs_ptr)?;
            let _ = dispatch_em_asm(&mut caller, code_ptr, &args)?;
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_asm_const_int_sync_on_main_thread",
        |mut caller: Caller<'_, StoreState>,
         code_ptr: i32,
         sig_ptr: i32,
         varargs_ptr: i32|
         -> Result<i32> {
            let mem = caller.data().runtime.memory.clone();
            let args = read_em_asm_varargs(&mem, sig_ptr, varargs_ptr)?;
            let res = dispatch_em_asm(&mut caller, code_ptr, &args)?;
            Ok(res as i64 as i32)
        },
    )?;

    Ok(())
}