use wasmtime::{Caller, Linker, Result};

use crate::backends::{GlContextInfo, GlMappedBuffer};
use crate::memory::{get_export_func, guest_alloc_cstr, guest_free, guest_malloc};
use crate::runtime::StoreState;

macro_rules! gl_noop {
    ($linker:expr, $name:literal, ($($arg:ident : $ty:ty),*)) => {
        $linker.func_wrap("env", $name, |$($arg: $ty),*| {})?;
    };
}

fn fill_object_ids(caller: &Caller<'_, StoreState>, count: i32, out_ptr: i32) -> Result<()> {
    let rt = &caller.data().runtime;
    for i in 0..count.max(0) {
        let id = rt.webgl_backend.next_object_id();
        rt.memory.write_i32(out_ptr.wrapping_add(i * 4), id)?;
    }
    Ok(())
}

fn call_emwgpu_create(caller: &mut Caller<'_, StoreState>, export_name: &str) -> Result<i32> {
    let f = get_export_func(caller, export_name)?.typed::<i32, i32>(&*caller)?;
    f.call(caller, 0)
}

pub fn register(linker: &mut Linker<StoreState>) -> Result<()> {
    register_webgl(linker)?;
    register_webgpu(linker)?;
    Ok(())
}

fn register_webgl(linker: &mut Linker<StoreState>) -> Result<()> {
    linker.func_wrap(
        "env",
        "emscripten_webgl_create_context",
        |caller: Caller<'_, StoreState>, _target: i32, attr_ptr: i32| -> Result<i32> {
            let rt = &caller.data().runtime;
            let mem = &rt.memory;
            let info = if attr_ptr != 0 {
                GlContextInfo {
                    id: 0,
                    alpha: mem.read_u8(attr_ptr)? != 0,
                    depth: mem.read_u8(attr_ptr + 1)? != 0,
                    stencil: mem.read_u8(attr_ptr + 2)? != 0,
                    antialias: mem.read_u8(attr_ptr + 3)? != 0,
                    premultiplied_alpha: mem.read_u8(attr_ptr + 4)? != 0,
                    preserve_drawing_buffer: mem.read_u8(attr_ptr + 5)? != 0,
                    major_version: mem.read_i32(attr_ptr + 16)?,
                    minor_version: mem.read_i32(attr_ptr + 20)?,
                    enable_extensions_by_default: mem.read_u8(attr_ptr + 24)? != 0,
                }
            } else {
                GlContextInfo {
                    id: 0,
                    alpha: true,
                    depth: true,
                    stencil: false,
                    antialias: true,
                    premultiplied_alpha: true,
                    preserve_drawing_buffer: false,
                    major_version: 2,
                    minor_version: 0,
                    enable_extensions_by_default: true,
                }
            };
            Ok(rt.webgl_backend.create_context(info))
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_webgl_destroy_context",
        |caller: Caller<'_, StoreState>, ctx: i32| -> i32 {
            caller.data().runtime.webgl_backend.destroy_context(ctx)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_webgl_make_context_current",
        |caller: Caller<'_, StoreState>, ctx: i32| -> i32 {
            caller.data().runtime.webgl_backend.make_current(ctx)
        },
    )?;

    linker.func_wrap(
        "env",
        "emscripten_webgl_get_context_attributes",
        |caller: Caller<'_, StoreState>, ctx: i32, out_ptr: i32| -> Result<i32> {
            if out_ptr == 0 {
                return Ok(-5);
            }
            let rt = &caller.data().runtime;
            let Some(info) = rt.webgl_backend.get_context(ctx) else {
                return Ok(-3);
            };
            let m = &rt.memory;
            m.write_u8(out_ptr, u8::from(info.alpha))?;
            m.write_u8(out_ptr + 1, u8::from(info.depth))?;
            m.write_u8(out_ptr + 2, u8::from(info.stencil))?;
            m.write_u8(out_ptr + 3, u8::from(info.antialias))?;
            m.write_u8(out_ptr + 4, u8::from(info.premultiplied_alpha))?;
            m.write_u8(out_ptr + 5, u8::from(info.preserve_drawing_buffer))?;
            m.write_i32(out_ptr + 8, 0)?;
            m.write_u8(out_ptr + 12, 0)?;
            m.write_i32(out_ptr + 16, info.major_version)?;
            m.write_i32(out_ptr + 20, info.minor_version)?;
            m.write_u8(out_ptr + 24, u8::from(info.enable_extensions_by_default))?;
            m.write_u8(out_ptr + 33, 0)?;
            Ok(0)
        },
    )?;

    linker.func_wrap(
        "env",
        "glGenBuffers",
        |caller: Caller<'_, StoreState>, n: i32, out: i32| -> Result<()> {
            fill_object_ids(&caller, n, out)
        },
    )?;
    linker.func_wrap(
        "env",
        "glGenFramebuffers",
        |caller: Caller<'_, StoreState>, n: i32, out: i32| -> Result<()> {
            fill_object_ids(&caller, n, out)
        },
    )?;
    linker.func_wrap(
        "env",
        "glGenRenderbuffers",
        |caller: Caller<'_, StoreState>, n: i32, out: i32| -> Result<()> {
            fill_object_ids(&caller, n, out)
        },
    )?;
    linker.func_wrap(
        "env",
        "glGenTextures",
        |caller: Caller<'_, StoreState>, n: i32, out: i32| -> Result<()> {
            fill_object_ids(&caller, n, out)
        },
    )?;
    linker.func_wrap(
        "env",
        "glGenVertexArrays",
        |caller: Caller<'_, StoreState>, n: i32, out: i32| -> Result<()> {
            fill_object_ids(&caller, n, out)
        },
    )?;

    linker.func_wrap("env", "glCreateProgram", |caller: Caller<'_, StoreState>| -> i32 {
        caller.data().runtime.webgl_backend.next_object_id()
    })?;
    linker.func_wrap(
        "env",
        "glCreateShader",
        |caller: Caller<'_, StoreState>, _shader_type: i32| -> i32 {
            caller.data().runtime.webgl_backend.next_object_id()
        },
    )?;
    linker.func_wrap(
        "env",
        "glFenceSync",
        |caller: Caller<'_, StoreState>, _cond: i32, _flags: i32| -> i32 {
            caller.data().runtime.webgl_backend.next_object_id()
        },
    )?;
    linker.func_wrap(
        "env",
        "glClientWaitSync",
        |_sync: i32, _flags: i32, _timeout: i64| -> i32 { 0x911A },
    )?;
    linker.func_wrap("env", "glCheckFramebufferStatus", |_target: i32| -> i32 {
        0x8CD5
    })?;
    linker.func_wrap("env", "glGetError", |caller: Caller<'_, StoreState>| -> i32 {
        caller.data().runtime.webgl_backend.get_error()
    })?;

    linker.func_wrap(
        "env",
        "glGetString",
        |mut caller: Caller<'_, StoreState>, name: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            if let Some(cached) = rt.webgl_backend.get_string_cached(name) {
                return Ok(cached);
            }
            let (vendor, renderer) = rt.webgpu_backend.adapter_names();
            let text_owned: String;
            let text: &str = match name {
                7936 | 37445 => {
                    text_owned = vendor;
                    &text_owned
                }
                7937 | 37446 => {
                    text_owned = renderer;
                    &text_owned
                }
                7938 => "OpenGL ES 3.0 (WebGL 2.0)",
                35724 => "OpenGL ES GLSL ES 3.00 (WebGL GLSL ES 3.00)",
                7939 => "",
                _ => return Ok(0),
            };
            let ptr = guest_alloc_cstr(&mut caller, &rt.memory, text)?;
            rt.webgl_backend.set_string_cached(name, ptr);
            Ok(ptr)
        },
    )?;

    linker.func_wrap(
        "env",
        "glGetIntegerv",
        |caller: Caller<'_, StoreState>, pname: i32, out_ptr: i32| -> Result<()> {
            if out_ptr == 0 {
                return Ok(());
            }
            let rt = &caller.data().runtime;
            let (w, h) = {
                let p = rt.platform.lock().unwrap();
                (p.width, p.height)
            };
            match pname {
                0x0BA2 | 0x0C10 => {
                    rt.memory.write_i32(out_ptr, 0)?;
                    rt.memory.write_i32(out_ptr + 4, 0)?;
                    rt.memory.write_i32(out_ptr + 8, w)?;
                    rt.memory.write_i32(out_ptr + 12, h)?;
                }
                0x0D3A => {
                    rt.memory.write_i32(out_ptr, 4096)?;
                    rt.memory.write_i32(out_ptr + 4, 4096)?;
                }
                _ => {
                    let val = match pname {
                        0x0D33 | 0x851C | 0x84E8 => 4096,
                        0x8073 | 0x88FF => 2048,
                        0x8869 | 0x8B4C | 0x8872 => 16,
                        0x8B4D => 32,
                        0x8B4A | 0x8B49 => 1024,
                        0x8824 | 0x8CDF => 8,
                        0x8D57 => 4,
                        0x8A2E => 36,
                        0x8A30 => 65536,
                        0x8A34 => 256,
                        0x821B => 3,
                        0x821C => 0,
                        0x8B4E => 1,
                        _ => 0,
                    };
                    rt.memory.write_i32(out_ptr, val)?;
                }
            }
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "glGetShaderiv",
        |caller: Caller<'_, StoreState>, _shader: i32, pname: i32, out_ptr: i32| -> Result<()> {
            if out_ptr == 0 {
                return Ok(());
            }
            let val = match pname {
                0x8B81 | 0x8B84 | 0x8B88 => 1,
                _ => 0,
            };
            caller.data().runtime.memory.write_i32(out_ptr, val)
        },
    )?;

    linker.func_wrap(
        "env",
        "glGetProgramiv",
        |caller: Caller<'_, StoreState>, _program: i32, pname: i32, out_ptr: i32| -> Result<()> {
            if out_ptr == 0 {
                return Ok(());
            }
            let val = match pname {
                0x8B82 | 0x8B83 | 0x8B84 | 0x8B86 | 0x8B8A | 0x8A35 => 1,
                _ => 0,
            };
            caller.data().runtime.memory.write_i32(out_ptr, val)
        },
    )?;

    linker.func_wrap(
        "env",
        "glGetShaderInfoLog",
        |caller: Caller<'_, StoreState>, _s: i32, max_len: i32, len_ptr: i32, buf_ptr: i32| -> Result<()> {
            let m = &caller.data().runtime.memory;
            if max_len > 0 && buf_ptr != 0 {
                m.write_u8(buf_ptr, 0)?;
            }
            if len_ptr != 0 {
                m.write_i32(len_ptr, 0)?;
            }
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "glGetProgramInfoLog",
        |caller: Caller<'_, StoreState>, _p: i32, max_len: i32, len_ptr: i32, buf_ptr: i32| -> Result<()> {
            let m = &caller.data().runtime.memory;
            if max_len > 0 && buf_ptr != 0 {
                m.write_u8(buf_ptr, 0)?;
            }
            if len_ptr != 0 {
                m.write_i32(len_ptr, 0)?;
            }
            Ok(())
        },
    )?;

    linker.func_wrap("env", "glGetUniformLocation", |_program: i32, _name_ptr: i32| -> i32 { -1 })?;
    linker.func_wrap("env", "glGetUniformBlockIndex", |_program: i32, _name_ptr: i32| -> i32 { 0 })?;

    linker.func_wrap(
        "env",
        "glGetActiveUniformBlockiv",
        |caller: Caller<'_, StoreState>, _p: i32, _idx: i32, pname: i32, out_ptr: i32| -> Result<()> {
            if out_ptr == 0 {
                return Ok(());
            }
            let val = if pname == 35393 { 1 } else { 0 };
            caller.data().runtime.memory.write_i32(out_ptr, val)
        },
    )?;

    linker.func_wrap(
        "env",
        "glGetActiveUniform",
        |caller: Caller<'_, StoreState>,
         _p: i32,
         _idx: i32,
         buf_size: i32,
         len_ptr: i32,
         size_ptr: i32,
         type_ptr: i32,
         name_ptr: i32|
         -> Result<()> {
            let m = &caller.data().runtime.memory;
            if buf_size > 0 && name_ptr != 0 {
                m.write_u8(name_ptr, 0)?;
            }
            if len_ptr != 0 {
                m.write_i32(len_ptr, 0)?;
            }
            if size_ptr != 0 {
                m.write_i32(size_ptr, 1)?;
            }
            if type_ptr != 0 {
                m.write_i32(type_ptr, 0x1406)?;
            }
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "glMapBufferRange",
        |mut caller: Caller<'_, StoreState>,
         target: i32,
         offset: i32,
         length: i32,
         access: i32|
         -> Result<i32> {
            if length <= 0 || (access & 33) != 0 || (access & 2) == 0 {
                return Ok(0);
            }
            let ptr = guest_malloc(&mut caller, length as u32 as usize)?;
            if ptr != 0 {
                caller.data().runtime.webgl_backend.set_mapped_buffer(
                    target,
                    GlMappedBuffer {
                        target,
                        offset,
                        length,
                        guest_mem: ptr,
                        access,
                    },
                );
            }
            Ok(ptr)
        },
    )?;

    linker.func_wrap(
        "env",
        "glUnmapBuffer",
        |mut caller: Caller<'_, StoreState>, target: i32| -> Result<i32> {
            let buf = caller
                .data()
                .runtime
                .webgl_backend
                .take_mapped_buffer(target);
            if let Some(b) = buf {
                guest_free(&mut caller, b.guest_mem)?;
                Ok(1)
            } else {
                Ok(0)
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "glPixelStorei",
        |caller: Caller<'_, StoreState>, pname: i32, param: i32| {
            caller
                .data()
                .runtime
                .webgl_backend
                .set_pixel_store(pname, param);
        },
    )?;

    gl_noop!(linker, "glScissor", (_x: i32, _y: i32, _w: i32, _h: i32));
    gl_noop!(linker, "glInvalidateFramebuffer", (_t: i32, _n: i32, _a: i32));
    gl_noop!(linker, "glBindFramebuffer", (_t: i32, _fb: i32));
    gl_noop!(linker, "glActiveTexture", (_tex: i32));
    gl_noop!(linker, "glBindTexture", (_t: i32, _tex: i32));
    gl_noop!(linker, "glCopyTexSubImage2D", (_a: i32, _b: i32, _c: i32, _d: i32, _e: i32, _f: i32, _g: i32, _h: i32));
    gl_noop!(linker, "glBindBufferRange", (_a: i32, _b: i32, _c: i32, _d: i32, _e: i32));
    gl_noop!(linker, "glBlitFramebuffer", (_a: i32, _b: i32, _c: i32, _d: i32, _e: i32, _f: i32, _g: i32, _h: i32, _i: i32, _j: i32));
    gl_noop!(linker, "glDisable", (_cap: i32));
    gl_noop!(linker, "glEnable", (_cap: i32));
    gl_noop!(linker, "glCullFace", (_mode: i32));
    gl_noop!(linker, "glPolygonOffset", (_factor: f32, _units: f32));
    gl_noop!(linker, "glColorMask", (_r: i32, _g: i32, _b: i32, _a: i32));
    gl_noop!(linker, "glBlendFunc", (_s: i32, _d: i32));
    gl_noop!(linker, "glBlendFuncSeparate", (_a: i32, _b: i32, _c: i32, _d: i32));
    gl_noop!(linker, "glDepthFunc", (_f: i32));
    gl_noop!(linker, "glDepthMask", (_flag: i32));
    gl_noop!(linker, "glStencilFunc", (_f: i32, _r: i32, _m: i32));
    gl_noop!(linker, "glStencilOp", (_s: i32, _d: i32, _p: i32));
    gl_noop!(linker, "glUseProgram", (_p: i32));
    gl_noop!(linker, "glStencilMask", (_m: i32));
    gl_noop!(linker, "glClearBufferfv", (_b: i32, _d: i32, _v: i32));
    gl_noop!(linker, "glClearBufferfi", (_b: i32, _d: i32, _df: f32, _s: i32));
    gl_noop!(linker, "glClearBufferiv", (_b: i32, _d: i32, _v: i32));
    gl_noop!(linker, "glClearColor", (_r: f32, _g: f32, _b: f32, _a: f32));
    gl_noop!(linker, "glClearDepthf", (_d: f32));
    gl_noop!(linker, "glClearStencil", (_s: i32));
    gl_noop!(linker, "glClear", (_mask: i32));
    gl_noop!(linker, "glBindBufferBase", (_t: i32, _i: i32, _b: i32));
    gl_noop!(linker, "glViewport", (_x: i32, _y: i32, _w: i32, _h: i32));
    gl_noop!(linker, "glDrawBuffers", (_n: i32, _b: i32));
    gl_noop!(linker, "glGenerateMipmap", (_t: i32));
    gl_noop!(linker, "glFramebufferTextureLayer", (_a: i32, _b: i32, _c: i32, _d: i32, _e: i32));
    gl_noop!(linker, "glDeleteFramebuffers", (_n: i32, _p: i32));
    gl_noop!(linker, "glDeleteBuffers", (_n: i32, _p: i32));
    gl_noop!(linker, "glDeleteShader", (_s: i32));
    gl_noop!(linker, "glDeleteProgram", (_p: i32));
    gl_noop!(linker, "glDeleteTextures", (_n: i32, _p: i32));
    gl_noop!(linker, "glDeleteRenderbuffers", (_n: i32, _p: i32));
    gl_noop!(linker, "glBindBuffer", (_t: i32, _b: i32));
    gl_noop!(linker, "glBindVertexArray", (_v: i32));
    gl_noop!(linker, "glDeleteVertexArrays", (_n: i32, _p: i32));
    gl_noop!(linker, "glBufferSubData", (_t: i32, _o: i32, _s: i32, _d: i32));
    gl_noop!(linker, "glBufferData", (_t: i32, _s: i32, _d: i32, _u: i32));
    gl_noop!(linker, "glDeleteSync", (_s: i32));
    gl_noop!(linker, "glFramebufferTexture2D", (_a: i32, _b: i32, _c: i32, _d: i32, _e: i32));
    gl_noop!(linker, "glFramebufferRenderbuffer", (_a: i32, _b: i32, _c: i32, _d: i32));
    gl_noop!(linker, "glEnableVertexAttribArray", (_i: i32));
    gl_noop!(linker, "glVertexAttribPointer", (_a: i32, _b: i32, _c: i32, _d: i32, _e: i32, _f: i32));
    gl_noop!(linker, "glDisableVertexAttribArray", (_i: i32));
    gl_noop!(linker, "glDrawElements", (_m: i32, _c: i32, _t: i32, _i: i32));
    gl_noop!(linker, "glDrawArrays", (_m: i32, _f: i32, _c: i32));
    gl_noop!(linker, "glDrawElementsInstanced", (_m: i32, _c: i32, _t: i32, _i: i32, _pc: i32));
    gl_noop!(linker, "glDrawArraysInstanced", (_m: i32, _f: i32, _c: i32, _pc: i32));
    gl_noop!(linker, "glAttachShader", (_p: i32, _s: i32));
    gl_noop!(linker, "glBindAttribLocation", (_p: i32, _i: i32, _n: i32));
    gl_noop!(linker, "glLinkProgram", (_p: i32));
    gl_noop!(linker, "glUniform1i", (_l: i32, _v: i32));
    gl_noop!(linker, "glUniformBlockBinding", (_p: i32, _i: i32, _b: i32));
    gl_noop!(linker, "glShaderSource", (_s: i32, _c: i32, _st: i32, _ln: i32));
    gl_noop!(linker, "glCompileShader", (_s: i32));
    gl_noop!(linker, "glTexStorage3D", (_a: i32, _b: i32, _c: i32, _d: i32, _e: i32, _f: i32));
    gl_noop!(linker, "glTexStorage2D", (_a: i32, _b: i32, _c: i32, _d: i32, _e: i32));
    gl_noop!(linker, "glCompressedTexImage3D", (_a: i32, _b: i32, _c: i32, _d: i32, _e: i32, _f: i32, _g: i32, _h: i32, _i: i32));
    gl_noop!(linker, "glCompressedTexImage2D", (_a: i32, _b: i32, _c: i32, _d: i32, _e: i32, _f: i32, _g: i32, _h: i32));
    gl_noop!(linker, "glTexImage3D", (_a: i32, _b: i32, _c: i32, _d: i32, _e: i32, _f: i32, _g: i32, _h: i32, _i: i32, _j: i32));
    gl_noop!(linker, "glTexImage2D", (_a: i32, _b: i32, _c: i32, _d: i32, _e: i32, _f: i32, _g: i32, _h: i32, _i: i32));
    gl_noop!(linker, "glTexParameteri", (_t: i32, _p: i32, _v: i32));
    gl_noop!(linker, "glReadPixels", (_x: i32, _y: i32, _w: i32, _h: i32, _f: i32, _t: i32, _p: i32));
    gl_noop!(linker, "glBindRenderbuffer", (_t: i32, _r: i32));
    gl_noop!(linker, "glRenderbufferStorageMultisample", (_t: i32, _s: i32, _f: i32, _w: i32, _h: i32));
    gl_noop!(linker, "glRenderbufferStorage", (_t: i32, _f: i32, _w: i32, _h: i32));
    gl_noop!(linker, "glCompressedTexSubImage3D", (_a: i32, _b: i32, _c: i32, _d: i32, _e: i32, _f: i32, _g: i32, _h: i32, _i: i32, _j: i32, _k: i32));
    gl_noop!(linker, "glCompressedTexSubImage2D", (_a: i32, _b: i32, _c: i32, _d: i32, _e: i32, _f: i32, _g: i32, _h: i32, _i: i32));
    gl_noop!(linker, "glTexSubImage3D", (_a: i32, _b: i32, _c: i32, _d: i32, _e: i32, _f: i32, _g: i32, _h: i32, _i: i32, _j: i32, _k: i32));
    gl_noop!(linker, "glTexSubImage2D", (_a: i32, _b: i32, _c: i32, _d: i32, _e: i32, _f: i32, _g: i32, _h: i32, _i: i32));
    gl_noop!(linker, "glTexParameterfv", (_t: i32, _p: i32, _v: i32));
    gl_noop!(linker, "glTexParameterf", (_t: i32, _p: i32, _v: f32));
    gl_noop!(linker, "glClipControlEXT", (_o: i32, _d: i32));

    Ok(())
}

fn register_webgpu(linker: &mut Linker<StoreState>) -> Result<()> {
    linker.func_wrap(
        "env",
        "emscripten_webgpu_get_device",
        |mut caller: Caller<'_, StoreState>| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let device_id = if let Some((dev, _)) = rt.webgpu_backend.get_or_init_device() {
                dev
            } else {
                let queue_id = call_emwgpu_create(&mut caller, "emwgpuCreateQueue")?;
                let create_dev = get_export_func(&mut caller, "emwgpuCreateDevice")?
                    .typed::<(i32, i32), i32>(&caller)?;
                let dev = create_dev.call(&mut caller, (0, queue_id))?;
                let add_ref = get_export_func(&mut caller, "wgpuDeviceAddRef")?
                    .typed::<i32, ()>(&caller)?;
                add_ref.call(&mut caller, dev)?;
                rt.webgpu_backend.set_device(dev, queue_id);
                dev
            };
            let add_ref = get_export_func(&mut caller, "wgpuDeviceAddRef")?
                .typed::<i32, ()>(&caller)?;
            add_ref.call(&mut caller, device_id)?;
            Ok(device_id)
        },
    )?;

    linker.func_wrap(
        "env",
        "emwgpuGetPreferredFormat",
        |caller: Caller<'_, StoreState>| -> i32 {
            caller.data().runtime.webgpu_backend.preferred_format()
        },
    )?;

    linker.func_wrap("env", "getWebGpuTimestampInterval", || -> i32 { 4 })?;

    linker.func_wrap(
        "env",
        "wgpuDeviceHasFeature",
        |caller: Caller<'_, StoreState>, _dev: i32, feature: i32| -> i32 {
            caller.data().runtime.webgpu_backend.has_feature(feature)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuDeviceGetLimits",
        |caller: Caller<'_, StoreState>, _dev: i32, r: i32| -> Result<i32> {
            let rt = &caller.data().runtime;
            rt.webgpu_backend.fill_limits(&rt.memory, r)?;
            Ok(1)
        },
    )?;

    linker.func_wrap(
        "env",
        "emwgpuDeviceDestroy",
        |caller: Caller<'_, StoreState>, _dev: i32| {
            caller.data().runtime.webgpu_backend.device_destroy();
        },
    )?;

    linker.func_wrap(
        "env",
        "emwgpuDeviceCreateBuffer",
        |mut caller: Caller<'_, StoreState>, _dev: i32, desc_ptr: i32, out_id: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let usage = rt.memory.read_u32(desc_ptr + 16)?;
            let size = rt.memory.read_u64(desc_ptr + 24)?;
            let mapped = rt.memory.read_u32(desc_ptr + 32)? != 0;
            let mapped_ptr = if mapped && size > 0 {
                guest_malloc(&mut caller, size as usize)?
            } else {
                0
            };
            let ok = rt.webgpu_backend.create_buffer(
                &rt.memory,
                out_id,
                desc_ptr,
                size,
                usage,
                mapped,
                mapped_ptr,
            )?;
            Ok(i32::from(ok))
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuBufferGetSize",
        |caller: Caller<'_, StoreState>, buf_id: i32| -> i64 {
            caller
                .data()
                .runtime
                .webgpu_backend
                .get_buffer(buf_id)
                .map(|b| b.size as i64)
                .unwrap_or(0)
        },
    )?;

    linker.func_wrap(
        "env",
        "emwgpuBufferGetConstMappedRange",
        |mut caller: Caller<'_, StoreState>, buf_id: i32, offset: i32, size: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let Some(meta) = rt.webgpu_backend.get_buffer(buf_id) else {
                return Ok(0);
            };
            let base = if meta.mapped_ptr != 0 {
                meta.mapped_ptr
            } else {
                let sz = if size < 0 {
                    meta.size as usize
                } else {
                    size as u32 as usize
                };
                let p = guest_malloc(&mut caller, sz.max(16))?;
                rt.webgpu_backend.set_buffer_mapped_ptr(buf_id, p);
                p
            };
            Ok(base.wrapping_add(offset))
        },
    )?;

    linker.func_wrap(
        "env",
        "emwgpuBufferMapAsync",
        |mut caller: Caller<'_, StoreState>,
         buf_id: i32,
         future_id: i64,
         mode: i64,
         offset: i32,
         size: i32|
         -> Result<()> {
            let rt = caller.data().runtime.clone();
            let mapped_ptr = if let Some(meta) = rt.webgpu_backend.get_buffer(buf_id) {
                if meta.mapped_ptr == 0 && meta.size > 0 {
                    let p = guest_malloc(&mut caller, meta.size as usize)?;
                    rt.webgpu_backend.set_buffer_mapped_ptr(buf_id, p);
                    p
                } else {
                    meta.mapped_ptr
                }
            } else {
                0
            };
            let status = rt.webgpu_backend.map_async(
                &rt.memory,
                buf_id,
                mode,
                offset,
                size,
                mapped_ptr,
            )?;
            let cb = get_export_func(&mut caller, "emwgpuOnMapAsyncCompleted")?
                .typed::<(f64, i32, i32), ()>(&caller)?;
            cb.call(&mut caller, (future_id as f64, status, 0))
        },
    )?;

    linker.func_wrap(
        "env",
        "emwgpuBufferUnmap",
        |mut caller: Caller<'_, StoreState>, buf_id: i32| -> Result<()> {
            let rt = caller.data().runtime.clone();
            let ptr = rt.webgpu_backend.unmap_buffer(&rt.memory, buf_id)?;
            if ptr != 0 {
                guest_free(&mut caller, ptr)?;
            }
            Ok(())
        },
    )?;

    linker.func_wrap(
        "env",
        "emwgpuDeviceCreateShaderModule",
        |caller: Caller<'_, StoreState>, _dev: i32, desc_ptr: i32, out_id: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            rt.webgpu_backend
                .create_shader_module(&rt.memory, out_id, desc_ptr)
        },
    )?;

    linker.func_wrap(
        "env",
        "emwgpuShaderModuleGetCompilationInfo",
        |mut caller: Caller<'_, StoreState>,
         module_id: i32,
         future_id: i64,
         struct_ptr: i32|
         -> Result<()> {
            let rt = caller.data().runtime.clone();
            let status = rt.webgpu_backend.get_shader_compilation_info(
                &rt.memory,
                module_id,
                struct_ptr,
            )?;
            let cb = get_export_func(&mut caller, "emwgpuOnCompilationInfoCompleted")?
                .typed::<(f64, i32, i32), ()>(&caller)?;
            cb.call(&mut caller, (future_id as f64, status, struct_ptr))
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuDeviceCreateTexture",
        |mut caller: Caller<'_, StoreState>, _dev: i32, desc_ptr: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let id = call_emwgpu_create(&mut caller, "emwgpuCreateTexture")?;
            rt.webgpu_backend.create_texture(&rt.memory, id, desc_ptr)?;
            Ok(id)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuTextureGetDepthOrArrayLayers",
        |caller: Caller<'_, StoreState>, tex_id: i32| -> i32 {
            caller
                .data()
                .runtime
                .webgpu_backend
                .get_texture(tex_id)
                .map(|t| t.depth_or_layers)
                .unwrap_or(1)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuTextureGetDimension",
        |caller: Caller<'_, StoreState>, tex_id: i32| -> i32 {
            caller
                .data()
                .runtime
                .webgpu_backend
                .get_texture(tex_id)
                .map(|t| t.dimension)
                .unwrap_or(2)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuTextureGetUsage",
        |caller: Caller<'_, StoreState>, tex_id: i32| -> i64 {
            caller
                .data()
                .runtime
                .webgpu_backend
                .get_texture(tex_id)
                .map(|t| t.usage as i64)
                .unwrap_or(0)
        },
    )?;

    linker.func_wrap(
        "env",
        "emwgpuDelete",
        |caller: Caller<'_, StoreState>, id: i32| {
            caller.data().runtime.webgpu_backend.remove_object(id);
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuInstanceCreateSurface",
        |mut caller: Caller<'_, StoreState>, _inst: i32, _desc_ptr: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let id = call_emwgpu_create(&mut caller, "emwgpuCreateSurface")?;
            rt.webgpu_backend.create_surface(id);
            Ok(id)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuSurfaceConfigure",
        |caller: Caller<'_, StoreState>, surf_id: i32, cfg_ptr: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            rt.webgpu_backend
                .surface_configure(&rt.memory, surf_id, cfg_ptr)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuDeviceCreateBindGroupLayout",
        |mut caller: Caller<'_, StoreState>, _dev: i32, desc_ptr: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let id = call_emwgpu_create(&mut caller, "emwgpuCreateBindGroupLayout")?;
            rt.webgpu_backend
                .create_bind_group_layout(&rt.memory, id, desc_ptr)?;
            Ok(id)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuDeviceCreatePipelineLayout",
        |mut caller: Caller<'_, StoreState>, _dev: i32, desc_ptr: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let id = call_emwgpu_create(&mut caller, "emwgpuCreatePipelineLayout")?;
            rt.webgpu_backend
                .create_pipeline_layout(&rt.memory, id, desc_ptr)?;
            Ok(id)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuDeviceCreateRenderPipeline",
        |mut caller: Caller<'_, StoreState>, _dev: i32, desc_ptr: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let id = call_emwgpu_create(&mut caller, "emwgpuCreateRenderPipeline")?;
            rt.webgpu_backend
                .create_render_pipeline(&rt.memory, id, desc_ptr)?;
            Ok(id)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuDeviceCreateComputePipeline",
        |mut caller: Caller<'_, StoreState>, _dev: i32, desc_ptr: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let id = call_emwgpu_create(&mut caller, "emwgpuCreateComputePipeline")?;
            rt.webgpu_backend
                .create_compute_pipeline(&rt.memory, id, desc_ptr)?;
            Ok(id)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuDeviceCreateBindGroup",
        |mut caller: Caller<'_, StoreState>, _dev: i32, desc_ptr: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let id = call_emwgpu_create(&mut caller, "emwgpuCreateBindGroup")?;
            rt.webgpu_backend
                .create_bind_group(&rt.memory, id, desc_ptr)?;
            Ok(id)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuDeviceCreateCommandEncoder",
        |mut caller: Caller<'_, StoreState>, _dev: i32, desc_ptr: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let id = call_emwgpu_create(&mut caller, "emwgpuCreateCommandEncoder")?;
            rt.webgpu_backend
                .create_command_encoder(&rt.memory, id, desc_ptr)?;
            Ok(id)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuDeviceCreateQuerySet",
        |mut caller: Caller<'_, StoreState>, _dev: i32, desc_ptr: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let id = call_emwgpu_create(&mut caller, "emwgpuCreateQuerySet")?;
            rt.webgpu_backend
                .create_query_set(&rt.memory, id, desc_ptr)?;
            Ok(id)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuDeviceCreateSampler",
        |mut caller: Caller<'_, StoreState>, _dev: i32, desc_ptr: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let id = call_emwgpu_create(&mut caller, "emwgpuCreateSampler")?;
            rt.webgpu_backend.create_sampler(&rt.memory, id, desc_ptr)?;
            Ok(id)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuTextureCreateView",
        |mut caller: Caller<'_, StoreState>, tex_id: i32, desc_ptr: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let id = call_emwgpu_create(&mut caller, "emwgpuCreateTextureView")?;
            rt.webgpu_backend
                .create_texture_view(&rt.memory, id, tex_id, desc_ptr)?;
            Ok(id)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuCommandEncoderBeginRenderPass",
        |mut caller: Caller<'_, StoreState>, enc_id: i32, desc_ptr: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let id = call_emwgpu_create(&mut caller, "emwgpuCreateRenderPassEncoder")?;
            rt.webgpu_backend
                .encoder_begin_render_pass(&rt.memory, enc_id, id, desc_ptr)?;
            Ok(id)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuCommandEncoderBeginComputePass",
        |mut caller: Caller<'_, StoreState>, enc_id: i32, desc_ptr: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let id = call_emwgpu_create(&mut caller, "emwgpuCreateComputePassEncoder")?;
            rt.webgpu_backend
                .encoder_begin_compute_pass(&rt.memory, enc_id, id, desc_ptr)?;
            Ok(id)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuCommandEncoderFinish",
        |mut caller: Caller<'_, StoreState>, enc_id: i32, _desc_ptr: i32| -> Result<i32> {
            let rt = caller.data().runtime.clone();
            let id = call_emwgpu_create(&mut caller, "emwgpuCreateCommandBuffer")?;
            rt.webgpu_backend.encoder_finish(enc_id, id);
            Ok(id)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuQueueSubmit",
        |caller: Caller<'_, StoreState>, queue_id: i32, n: i32, cmds_ptr: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            rt.webgpu_backend
                .queue_submit(&rt.memory, queue_id, n.max(0) as u32, cmds_ptr)
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuQueueWriteTexture",
        |caller: Caller<'_, StoreState>,
         queue_id: i32,
         dst_ptr: i32,
         data_ptr: i32,
         sz: i32,
         layout_ptr: i32,
         ext_ptr: i32|
         -> Result<()> {
            let rt = &caller.data().runtime;
            rt.webgpu_backend.queue_write_texture(
                &rt.memory,
                queue_id,
                dst_ptr,
                data_ptr,
                sz.max(0) as u32,
                layout_ptr,
                ext_ptr,
            )
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuQueueWriteBuffer",
        |caller: Caller<'_, StoreState>,
         queue_id: i32,
         buf_id: i32,
         off: i64,
         data_ptr: i32,
         sz: i32|
         -> Result<()> {
            let rt = &caller.data().runtime;
            rt.webgpu_backend.queue_write_buffer(
                &rt.memory,
                queue_id,
                buf_id,
                off as u64,
                data_ptr,
                sz.max(0) as u32,
            )
        },
    )?;

    linker.func_wrap(
        "env",
        "presentBrowserFrame",
        |caller: Caller<'_, StoreState>, dev: i32, tex: i32, cmd: i32| {
            caller
                .data()
                .runtime
                .webgpu_backend
                .present_browser_frame(dev, tex, cmd);
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuCommandEncoderPushDebugGroup",
        |caller: Caller<'_, StoreState>, enc: i32, lbl: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            rt.webgpu_backend
                .encoder_push_debug_group(&rt.memory, enc, lbl)
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuCommandEncoderPopDebugGroup",
        |caller: Caller<'_, StoreState>, enc: i32| {
            caller
                .data()
                .runtime
                .webgpu_backend
                .encoder_pop_debug_group(enc);
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuCommandEncoderCopyTextureToTexture",
        |caller: Caller<'_, StoreState>, enc: i32, s: i32, d: i32, ext: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            rt.webgpu_backend
                .encoder_copy_texture_to_texture(&rt.memory, enc, s, d, ext)
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuCommandEncoderCopyTextureToBuffer",
        |caller: Caller<'_, StoreState>, enc: i32, s: i32, d: i32, ext: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            rt.webgpu_backend
                .encoder_copy_texture_to_buffer(&rt.memory, enc, s, d, ext)
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuCommandEncoderCopyBufferToBuffer",
        |caller: Caller<'_, StoreState>,
         enc: i32,
         s: i32,
         so: i64,
         d: i32,
         dst_o: i64,
         sz: i64| {
            caller
                .data()
                .runtime
                .webgpu_backend
                .encoder_copy_buffer_to_buffer(
                    enc,
                    s,
                    so as u64,
                    d,
                    dst_o as u64,
                    sz as u64,
                );
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuCommandEncoderResolveQuerySet",
        |caller: Caller<'_, StoreState>,
         enc: i32,
         qs: i32,
         first: i32,
         count: i32,
         dst: i32,
         off: i64| {
            caller
                .data()
                .runtime
                .webgpu_backend
                .encoder_resolve_query_set(
                    enc,
                    qs,
                    first as u32,
                    count as u32,
                    dst,
                    off as u64,
                );
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuComputePassEncoderSetPipeline",
        |caller: Caller<'_, StoreState>, p: i32, pl: i32| {
            caller
                .data()
                .runtime
                .webgpu_backend
                .compute_pass_set_pipeline(p, pl);
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuComputePassEncoderDispatchWorkgroups",
        |caller: Caller<'_, StoreState>, p: i32, x: i32, y: i32, z: i32| {
            caller
                .data()
                .runtime
                .webgpu_backend
                .compute_pass_dispatch(p, x as u32, y as u32, z as u32);
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuComputePassEncoderSetBindGroup",
        |caller: Caller<'_, StoreState>, p: i32, i: i32, g: i32, dc: i32, do_ptr: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            rt.webgpu_backend.compute_pass_set_bind_group(
                &rt.memory,
                p,
                i as u32,
                g,
                dc.max(0) as u32,
                do_ptr,
            )
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuComputePassEncoderPushDebugGroup",
        |caller: Caller<'_, StoreState>, p: i32, lbl: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            rt.webgpu_backend
                .compute_pass_push_debug_group(&rt.memory, p, lbl)
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuComputePassEncoderPopDebugGroup",
        |caller: Caller<'_, StoreState>, p: i32| {
            caller
                .data()
                .runtime
                .webgpu_backend
                .compute_pass_pop_debug_group(p);
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuComputePassEncoderEnd",
        |caller: Caller<'_, StoreState>, p: i32| {
            caller.data().runtime.webgpu_backend.compute_pass_end(p);
        },
    )?;

    linker.func_wrap(
        "env",
        "wgpuRenderPassEncoderPushDebugGroup",
        |caller: Caller<'_, StoreState>, p: i32, lbl: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            rt.webgpu_backend
                .render_pass_push_debug_group(&rt.memory, p, lbl)
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuRenderPassEncoderPopDebugGroup",
        |caller: Caller<'_, StoreState>, p: i32| {
            caller
                .data()
                .runtime
                .webgpu_backend
                .render_pass_pop_debug_group(p);
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuRenderPassEncoderEnd",
        |caller: Caller<'_, StoreState>, p: i32| {
            caller.data().runtime.webgpu_backend.render_pass_end(p);
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuRenderPassEncoderSetPipeline",
        |caller: Caller<'_, StoreState>, p: i32, pl: i32| {
            caller
                .data()
                .runtime
                .webgpu_backend
                .render_pass_set_pipeline(p, pl);
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuRenderPassEncoderSetBindGroup",
        |caller: Caller<'_, StoreState>, p: i32, i: i32, g: i32, dc: i32, do_ptr: i32| -> Result<()> {
            let rt = &caller.data().runtime;
            rt.webgpu_backend.render_pass_set_bind_group(
                &rt.memory,
                p,
                i as u32,
                g,
                dc.max(0) as u32,
                do_ptr,
            )
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuRenderPassEncoderDraw",
        |caller: Caller<'_, StoreState>, p: i32, vc: i32, ic: i32, fv: i32, fi: i32| {
            caller.data().runtime.webgpu_backend.render_pass_draw(
                p,
                vc as u32,
                ic as u32,
                fv as u32,
                fi as u32,
            );
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuRenderPassEncoderSetVertexBuffer",
        |caller: Caller<'_, StoreState>, p: i32, s: i32, b: i32, o: i64, sz: i64| {
            caller
                .data()
                .runtime
                .webgpu_backend
                .render_pass_set_vertex_buffer(p, s as u32, b, o as u64, sz);
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuRenderPassEncoderSetIndexBuffer",
        |caller: Caller<'_, StoreState>, p: i32, b: i32, f: i32, o: i64, sz: i64| {
            caller
                .data()
                .runtime
                .webgpu_backend
                .render_pass_set_index_buffer(p, b, f, o as u64, sz);
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuRenderPassEncoderDrawIndexed",
        |caller: Caller<'_, StoreState>,
         p: i32,
         ic: i32,
         inc: i32,
         fi: i32,
         bv: i32,
         fin: i32| {
            caller
                .data()
                .runtime
                .webgpu_backend
                .render_pass_draw_indexed(p, ic as u32, inc as u32, fi as u32, bv, fin as u32);
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuRenderPassEncoderSetStencilReference",
        |caller: Caller<'_, StoreState>, p: i32, r: i32| {
            caller
                .data()
                .runtime
                .webgpu_backend
                .render_pass_set_stencil_reference(p, r as u32);
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuRenderPassEncoderSetViewport",
        |caller: Caller<'_, StoreState>,
         p: i32,
         x: f32,
         y: f32,
         w: f32,
         h: f32,
         mn: f32,
         mx: f32| {
            caller
                .data()
                .runtime
                .webgpu_backend
                .render_pass_set_viewport(p, x, y, w, h, mn, mx);
        },
    )?;
    linker.func_wrap(
        "env",
        "wgpuRenderPassEncoderSetScissorRect",
        |caller: Caller<'_, StoreState>, p: i32, x: i32, y: i32, w: i32, h: i32| {
            caller
                .data()
                .runtime
                .webgpu_backend
                .render_pass_set_scissor_rect(p, x as u32, y as u32, w as u32, h as u32);
        },
    )?;

    Ok(())
}
