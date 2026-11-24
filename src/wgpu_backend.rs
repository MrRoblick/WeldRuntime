use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex};
use winit::window::Window;

use crate::backends::{WebGpuBackend, WgpuBufferMeta, WgpuTextureMeta, fill_default_wgpu_limits};
use crate::memory::SharedMem;

struct BufferEntry {
    buffer: wgpu::Buffer,
    meta: WgpuBufferMeta,
    map_mode: Option<wgpu::MapMode>,
    mapped_range: Option<(u64, u64)>,
}

struct TextureEntry {
    texture: wgpu::Texture,
    meta: WgpuTextureMeta,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
}

struct SurfaceState {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    preferred_format_id: i32,
    supports_copy_dst: bool,
}

pub struct RealWebGpu {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    adapter_vendor: String,
    adapter_renderer: String,
    surface_state: Mutex<SurfaceState>,
    device_queue_ids: Mutex<Option<(i32, i32)>>,

    buffers: Mutex<HashMap<i32, BufferEntry>>,
    shaders: Mutex<HashMap<i32, wgpu::ShaderModule>>,
    textures: Mutex<HashMap<i32, TextureEntry>>,
    texture_views: Mutex<HashMap<i32, wgpu::TextureView>>,
    samplers: Mutex<HashMap<i32, wgpu::Sampler>>,
    bind_group_layouts: Mutex<HashMap<i32, wgpu::BindGroupLayout>>,
    pipeline_layouts: Mutex<HashMap<i32, wgpu::PipelineLayout>>,
    bind_groups: Mutex<HashMap<i32, wgpu::BindGroup>>,
    render_pipelines: Mutex<HashMap<i32, wgpu::RenderPipeline>>,
    compute_pipelines: Mutex<HashMap<i32, wgpu::ComputePipeline>>,
    query_sets: Mutex<HashMap<i32, wgpu::QuerySet>>,
    command_encoders: Mutex<HashMap<i32, wgpu::CommandEncoder>>,
    render_passes: Mutex<HashMap<i32, wgpu::RenderPass<'static>>>,
    compute_passes: Mutex<HashMap<i32, wgpu::ComputePass<'static>>>,
    command_buffers: Mutex<HashMap<i32, wgpu::CommandBuffer>>,
}

impl RealWebGpu {
    pub fn new(window: Arc<Window>) -> anyhow::Result<Self> {
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        let surface = instance.create_surface(window)?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .map_err(|e| anyhow::anyhow!("wgpu request_adapter failed: {e:?}"))?;

        let info = adapter.get_info();
        println!(
            "[wgpu] Adapter: {} ({:?}, driver={})",
            info.name, info.backend, info.driver
        );

        let supported = adapter.features();
        let desired_features = [
            wgpu::Features::TEXTURE_COMPRESSION_BC,
            wgpu::Features::TEXTURE_COMPRESSION_BC_SLICED_3D,
            wgpu::Features::TEXTURE_COMPRESSION_ETC2,
            wgpu::Features::TEXTURE_COMPRESSION_ASTC,
            wgpu::Features::RG11B10UFLOAT_RENDERABLE,
            wgpu::Features::FLOAT32_FILTERABLE,
            wgpu::Features::TIMESTAMP_QUERY,
            wgpu::Features::DEPTH_CLIP_CONTROL,
            wgpu::Features::DEPTH32FLOAT_STENCIL8,
            wgpu::Features::INDIRECT_FIRST_INSTANCE,
            wgpu::Features::SHADER_F16,
            wgpu::Features::BGRA8UNORM_STORAGE,
            wgpu::Features::DUAL_SOURCE_BLENDING,
            wgpu::Features::CLIP_DISTANCES,
            wgpu::Features::TEXTURE_FORMAT_16BIT_NORM,
        ];
        let mut required_features = wgpu::Features::empty();
        for f in desired_features {
            if supported.contains(f) {
                required_features |= f;
            }
        }

        let required_limits = adapter.limits();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("WeldRuntime WebGPU Device"),
            required_features,
            required_limits,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .map_err(|e| anyhow::anyhow!("wgpu request_device failed: {e:?}"))?;

        device.on_uncaptured_error(Arc::new(|err| {
            eprintln!("[wgpu:ERROR] {err}");
        }));

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| *f == wgpu::TextureFormat::Bgra8Unorm)
            .or_else(|| {
                caps.formats
                    .iter()
                    .copied()
                    .find(|f| *f == wgpu::TextureFormat::Rgba8Unorm)
            })
            .unwrap_or(caps.formats[0]);

        let preferred_format_id = texture_format_to_id(format).unwrap_or(27);
        let supports_copy_dst = caps.usages.contains(wgpu::TextureUsages::COPY_DST);
        let mut usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        if supports_copy_dst {
            usage |= wgpu::TextureUsages::COPY_DST;
        }
        let alpha_mode = caps
            .alpha_modes
            .first()
            .copied()
            .unwrap_or(wgpu::CompositeAlphaMode::Opaque);

        let config = wgpu::SurfaceConfiguration {
            usage,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        println!(
            "[wgpu] Surface configured {}x{}, format={:?} (id={}), copy_dst={}",
            width, height, format, preferred_format_id, supports_copy_dst
        );

        Ok(Self {
            device: Arc::new(device),
            queue: Arc::new(queue),
            adapter_vendor: format!("0x{:04x}", info.vendor),
            adapter_renderer: info.name,
            surface_state: Mutex::new(SurfaceState {
                surface,
                config,
                preferred_format_id,
                supports_copy_dst,
            }),
            device_queue_ids: Mutex::new(None),
            buffers: Mutex::new(HashMap::new()),
            shaders: Mutex::new(HashMap::new()),
            textures: Mutex::new(HashMap::new()),
            texture_views: Mutex::new(HashMap::new()),
            samplers: Mutex::new(HashMap::new()),
            bind_group_layouts: Mutex::new(HashMap::new()),
            pipeline_layouts: Mutex::new(HashMap::new()),
            bind_groups: Mutex::new(HashMap::new()),
            render_pipelines: Mutex::new(HashMap::new()),
            compute_pipelines: Mutex::new(HashMap::new()),
            query_sets: Mutex::new(HashMap::new()),
            command_encoders: Mutex::new(HashMap::new()),
            render_passes: Mutex::new(HashMap::new()),
            compute_passes: Mutex::new(HashMap::new()),
            command_buffers: Mutex::new(HashMap::new()),
        })
    }
}

impl WebGpuBackend for RealWebGpu {
    fn adapter_names(&self) -> (String, String) {
        (self.adapter_vendor.clone(), self.adapter_renderer.clone())
    }

    fn get_or_init_device(&self) -> Option<(i32, i32)> {
        *self.device_queue_ids.lock().unwrap()
    }

    fn set_device(&self, device_id: i32, queue_id: i32) {
        *self.device_queue_ids.lock().unwrap() = Some((device_id, queue_id));
    }

    fn preferred_format(&self) -> i32 {
        self.surface_state.lock().unwrap().preferred_format_id
    }

    fn has_feature(&self, feature: i32) -> i32 {
        let f = self.device.features();
        let has = match feature {
            1 => true, // core-features-and-limits
            2 => f.contains(wgpu::Features::DEPTH_CLIP_CONTROL),
            3 => f.contains(wgpu::Features::DEPTH32FLOAT_STENCIL8),
            4 => f.contains(wgpu::Features::TEXTURE_COMPRESSION_BC),
            5 => f.contains(wgpu::Features::TEXTURE_COMPRESSION_BC_SLICED_3D),
            6 => f.contains(wgpu::Features::TEXTURE_COMPRESSION_ETC2),
            7 => f.contains(wgpu::Features::TEXTURE_COMPRESSION_ASTC),
            9 => f.contains(wgpu::Features::TIMESTAMP_QUERY),
            10 => f.contains(wgpu::Features::INDIRECT_FIRST_INSTANCE),
            11 => f.contains(wgpu::Features::SHADER_F16),
            12 => f.contains(wgpu::Features::RG11B10UFLOAT_RENDERABLE),
            13 => f.contains(wgpu::Features::BGRA8UNORM_STORAGE),
            14 => f.contains(wgpu::Features::FLOAT32_FILTERABLE),
            16 => f.contains(wgpu::Features::CLIP_DISTANCES),
            17 => f.contains(wgpu::Features::DUAL_SOURCE_BLENDING),
            19 | 327692 => f.contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM),
            _ => false,
        };
        i32::from(has)
    }

    fn fill_limits(&self, mem: &SharedMem, r: i32) -> wasmtime::Result<()> {
        let lim = self.device.limits();
        fill_default_wgpu_limits(mem, r)?;
        mem.write_u32(r + 4, lim.max_texture_dimension_1d)?;
        mem.write_u32(r + 8, lim.max_texture_dimension_2d)?;
        mem.write_u32(r + 12, lim.max_texture_dimension_3d)?;
        mem.write_u32(r + 16, lim.max_texture_array_layers)?;
        mem.write_u32(r + 20, lim.max_bind_groups)?;
        mem.write_u32(r + 24, lim.max_bind_groups + lim.max_vertex_buffers)?;
        mem.write_u32(r + 28, lim.max_bindings_per_bind_group)?;
        mem.write_u32(r + 32, lim.max_dynamic_uniform_buffers_per_pipeline_layout)?;
        mem.write_u32(r + 36, lim.max_dynamic_storage_buffers_per_pipeline_layout)?;
        mem.write_u32(r + 40, lim.max_sampled_textures_per_shader_stage)?;
        mem.write_u32(r + 44, lim.max_samplers_per_shader_stage)?;
        mem.write_u32(r + 48, lim.max_storage_buffers_per_shader_stage)?;
        mem.write_u32(r + 52, lim.max_storage_textures_per_shader_stage)?;
        mem.write_u32(r + 56, lim.max_uniform_buffers_per_shader_stage)?;
        mem.write_u64(r + 64, lim.max_uniform_buffer_binding_size as u64)?;
        mem.write_u64(r + 72, lim.max_storage_buffer_binding_size as u64)?;
        mem.write_u32(r + 80, lim.min_uniform_buffer_offset_alignment)?;
        mem.write_u32(r + 84, lim.min_storage_buffer_offset_alignment)?;
        mem.write_u32(r + 88, lim.max_vertex_buffers)?;
        mem.write_u64(r + 96, lim.max_buffer_size)?;
        mem.write_u32(r + 104, lim.max_vertex_attributes)?;
        mem.write_u32(r + 108, lim.max_vertex_buffer_array_stride)?;
        mem.write_u32(r + 112, lim.max_inter_stage_shader_variables)?;
        mem.write_u32(r + 116, lim.max_color_attachments)?;
        mem.write_u32(r + 120, lim.max_color_attachment_bytes_per_sample)?;
        mem.write_u32(r + 124, lim.max_compute_workgroup_storage_size)?;
        mem.write_u32(r + 128, lim.max_compute_invocations_per_workgroup)?;
        mem.write_u32(r + 132, lim.max_compute_workgroup_size_x)?;
        mem.write_u32(r + 136, lim.max_compute_workgroup_size_y)?;
        mem.write_u32(r + 140, lim.max_compute_workgroup_size_z)?;
        mem.write_u32(r + 144, lim.max_compute_workgroups_per_dimension)?;
        Ok(())
    }

    fn device_destroy(&self) {
        self.device.destroy();
    }

    fn create_buffer(
        &self,
        mem: &SharedMem,
        id: i32,
        desc_ptr: i32,
        size: u64,
        usage: u32,
        mapped: bool,
        mapped_ptr: i32,
    ) -> wasmtime::Result<bool> {
        let label = read_opt_string_view(mem, desc_ptr + 4)?;
        let wgpu_usage = wgpu::BufferUsages::from_bits_truncate(usage);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: label.as_deref(),
            size,
            usage: wgpu_usage,
            mapped_at_creation: mapped,
        });
        self.buffers.lock().unwrap().insert(
            id,
            BufferEntry {
                buffer,
                meta: WgpuBufferMeta {
                    size,
                    usage,
                    mapped_ptr,
                },
                map_mode: if mapped { Some(wgpu::MapMode::Write) } else { None },
                mapped_range: if mapped { Some((0, size)) } else { None },
            },
        );
        Ok(true)
    }

    fn get_buffer(&self, id: i32) -> Option<WgpuBufferMeta> {
        self.buffers.lock().unwrap().get(&id).map(|b| b.meta.clone())
    }

    fn set_buffer_mapped_ptr(&self, id: i32, ptr: i32) {
        if let Some(b) = self.buffers.lock().unwrap().get_mut(&id) {
            b.meta.mapped_ptr = ptr;
        }
    }

    fn map_async(
        &self,
        mem: &SharedMem,
        buf_id: i32,
        mode: i64,
        offset: i32,
        size: i32,
        mapped_ptr: i32,
    ) -> wasmtime::Result<i32> {
        let map_mode = if (mode & 1) != 0 {
            wgpu::MapMode::Read
        } else {
            wgpu::MapMode::Write
        };

        let (buffer, start, end) = {
            let mut map = self.buffers.lock().unwrap();
            let Some(entry) = map.get_mut(&buf_id) else {
                return Ok(3);
            };
            let buf_size = entry.meta.size;
            let start = offset as u32 as u64;
            let len = if size < 0 {
                buf_size.saturating_sub(start)
            } else {
                size as u32 as u64
            };
            let end = (start + len).min(buf_size);
            entry.map_mode = Some(map_mode);
            entry.mapped_range = Some((start, end));
            if mapped_ptr != 0 {
                entry.meta.mapped_ptr = mapped_ptr;
            }
            (entry.buffer.clone(), start, end)
        };

        let status = Arc::new(Mutex::new(None));
        let status_cb = status.clone();
        buffer.slice(start..end).map_async(map_mode, move |res| {
            *status_cb.lock().unwrap() = Some(res.is_ok());
        });
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());

        let ok = status.lock().unwrap().unwrap_or(false);
        if !ok {
            return Ok(3);
        }

        if map_mode == wgpu::MapMode::Read && mapped_ptr != 0 && end > start {
            if let Ok(view) = buffer.slice(start..end).get_mapped_range() {
                mem.write_bytes(mapped_ptr.wrapping_add(start as i32), &view)?;
            }
        }

        Ok(1)
    }

    fn unmap_buffer(&self, mem: &SharedMem, id: i32) -> wasmtime::Result<i32> {
        let mut map = self.buffers.lock().unwrap();
        let Some(entry) = map.get_mut(&id) else {
            return Ok(0);
        };
        let ptr = entry.meta.mapped_ptr;
        entry.meta.mapped_ptr = 0;
        let was_write = entry.map_mode == Some(wgpu::MapMode::Write);
        let range = entry.mapped_range.take().unwrap_or((0, entry.meta.size));
        entry.map_mode = None;

        if was_write && ptr != 0 && range.1 > range.0 {
            let len = (range.1 - range.0) as usize;
            let bytes = mem.read_bytes(ptr.wrapping_add(range.0 as i32), len)?;
            if let Ok(mut view) = entry.buffer.slice(range.0..range.1).get_mapped_range_mut() {
                let n = view.len().min(bytes.len());
                view.slice(..n).copy_from_slice(&bytes[..n]);
            }
        }
        entry.buffer.unmap();
        Ok(ptr)
    }

    fn create_shader_module(&self, mem: &SharedMem, id: i32, desc_ptr: i32) -> wasmtime::Result<()> {
        let next = mem.read_i32(desc_ptr)?;
        let label = read_opt_string_view(mem, desc_ptr + 4)?;
        let mut code = String::new();
        if next != 0 {
            let stype = mem.read_u32(next + 4)?;
            if stype == 2 {
                code = read_string_view(mem, next + 8)?;
            }
        }
        let module = self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: label.as_deref(),
            source: wgpu::ShaderSource::Wgsl(code.into()),
        });
        println!("[wgpu] create_shader_module id={id} label={label:?}");
        self.shaders.lock().unwrap().insert(id, module);
        Ok(())
    }

    fn create_texture(&self, mem: &SharedMem, id: i32, desc_ptr: i32) -> wasmtime::Result<()> {
        let label = read_opt_string_view(mem, desc_ptr + 4)?;
        let usage_bits = mem.read_u32(desc_ptr + 16)?;
        let dim_id = mem.read_i32(desc_ptr + 24)?;
        let width = mem.read_u32(desc_ptr + 28)?.max(1);
        let height = mem.read_u32(desc_ptr + 32)?.max(1);
        let depth_or_layers = mem.read_u32(desc_ptr + 36)?.max(1);
        let format_id = mem.read_u32(desc_ptr + 40)?;
        let mip_level_count = mem.read_u32(desc_ptr + 44)?.max(1);
        let sample_count = mem.read_u32(desc_ptr + 48)?.max(1);
        let view_fmt_count = mem.read_u32(desc_ptr + 52)?;
        let view_fmt_ptr = mem.read_i32(desc_ptr + 56)?;

        let mut view_formats = Vec::new();
        if view_fmt_count > 0 && view_fmt_ptr != 0 {
            for i in 0..view_fmt_count {
                let fid = mem.read_u32(view_fmt_ptr + (i as i32 * 4))?;
                if let Some(f) = id_to_texture_format(fid) {
                    view_formats.push(f);
                }
            }
        }

        let format = id_to_texture_format(format_id).unwrap_or(wgpu::TextureFormat::Rgba8Unorm);
        let dimension = match dim_id {
            1 => wgpu::TextureDimension::D1,
            3 => wgpu::TextureDimension::D3,
            _ => wgpu::TextureDimension::D2,
        };
        let usage = wgpu::TextureUsages::from_bits_truncate(usage_bits);

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: label.as_deref(),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: depth_or_layers,
            },
            mip_level_count,
            sample_count,
            dimension,
            format,
            usage,
            view_formats: &view_formats,
        });

        self.textures.lock().unwrap().insert(
            id,
            TextureEntry {
                texture,
                meta: WgpuTextureMeta {
                    depth_or_layers: depth_or_layers as i32,
                    dimension: dim_id,
                    usage: usage_bits as u64,
                },
                width,
                height,
                format,
            },
        );
        Ok(())
    }

    fn get_texture(&self, id: i32) -> Option<WgpuTextureMeta> {
        self.textures.lock().unwrap().get(&id).map(|t| t.meta.clone())
    }

    fn create_texture_view(
        &self,
        mem: &SharedMem,
        id: i32,
        tex_id: i32,
        desc_ptr: i32,
    ) -> wasmtime::Result<()> {
        let tex = {
            let map = self.textures.lock().unwrap();
            map.get(&tex_id).map(|t| t.texture.clone())
        };
        let Some(tex) = tex else {
            return Ok(());
        };

        let view = if desc_ptr != 0 {
            let label = read_opt_string_view(mem, desc_ptr + 4)?;
            let format = id_to_texture_format(mem.read_u32(desc_ptr + 12)?);
            let dimension = id_to_texture_view_dim(mem.read_u32(desc_ptr + 16)?);
            let base_mip_level = mem.read_u32(desc_ptr + 20)?;
            let mip_raw = mem.read_u32(desc_ptr + 24)?;
            let base_array_layer = mem.read_u32(desc_ptr + 28)?;
            let arr_raw = mem.read_u32(desc_ptr + 32)?;
            let aspect = id_to_aspect(mem.read_u32(desc_ptr + 36)?);
            let usage_bits = mem.read_u32(desc_ptr + 40)?;
            let usage = if usage_bits != 0 {
                Some(wgpu::TextureUsages::from_bits_truncate(usage_bits))
            } else {
                None
            };
            tex.create_view(&wgpu::TextureViewDescriptor {
                label: label.as_deref(),
                format,
                dimension,
                usage,
                aspect,
                base_mip_level,
                mip_level_count: (mip_raw != u32::MAX && mip_raw != 0).then_some(mip_raw),
                base_array_layer,
                array_layer_count: (arr_raw != u32::MAX && arr_raw != 0).then_some(arr_raw),
            })
        } else {
            tex.create_view(&wgpu::TextureViewDescriptor::default())
        };

        self.texture_views.lock().unwrap().insert(id, view);
        Ok(())
    }

    fn create_sampler(&self, mem: &SharedMem, id: i32, desc_ptr: i32) -> wasmtime::Result<()> {
        let desc = if desc_ptr != 0 {
            let label = read_opt_string_view(mem, desc_ptr + 4)?;
            let address_mode_u = id_to_address_mode(mem.read_u32(desc_ptr + 12)?);
            let address_mode_v = id_to_address_mode(mem.read_u32(desc_ptr + 16)?);
            let address_mode_w = id_to_address_mode(mem.read_u32(desc_ptr + 20)?);
            let mag_filter = id_to_filter_mode(mem.read_u32(desc_ptr + 24)?);
            let min_filter = id_to_filter_mode(mem.read_u32(desc_ptr + 28)?);
            let mipmap_filter = id_to_mipmap_filter(mem.read_u32(desc_ptr + 32)?);
            let lod_min_clamp = mem.read_f32(desc_ptr + 36)?;
            let lod_max_clamp = mem.read_f32(desc_ptr + 40)?;
            let compare = id_to_compare_opt(mem.read_u32(desc_ptr + 44)?);
            let anisotropy_clamp = mem.read_u16(desc_ptr + 48)?.max(1);
            let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
                label: label.as_deref(),
                address_mode_u,
                address_mode_v,
                address_mode_w,
                mag_filter,
                min_filter,
                mipmap_filter,
                lod_min_clamp,
                lod_max_clamp,
                compare,
                anisotropy_clamp,
                border_color: None,
            });
            sampler
        } else {
            self.device.create_sampler(&wgpu::SamplerDescriptor::default())
        };
        self.samplers.lock().unwrap().insert(id, desc);
        Ok(())
    }

    fn create_bind_group_layout(
        &self,
        mem: &SharedMem,
        id: i32,
        desc_ptr: i32,
    ) -> wasmtime::Result<()> {
        let label = read_opt_string_view(mem, desc_ptr + 4)?;
        let count = mem.read_u32(desc_ptr + 12)?;
        let ptr = mem.read_i32(desc_ptr + 16)?;
        let mut entries = Vec::with_capacity(count as usize);

        for i in 0..count {
            let p = ptr + (i as i32 * 88);
            let binding = mem.read_u32(p + 4)?;
            let vis_bits = mem.read_u32(p + 8)?;
            let visibility = wgpu::ShaderStages::from_bits_truncate(vis_bits);

            let buf_type = mem.read_u32(p + 24 + 4)?;
            let samp_type = mem.read_u32(p + 48 + 4)?;
            let tex_sample_type = mem.read_u32(p + 56 + 4)?;
            let stex_access = mem.read_u32(p + 72 + 4)?;

            let ty = if buf_type != 0 {
                let bty = match buf_type {
                    2 => wgpu::BufferBindingType::Uniform,
                    3 => wgpu::BufferBindingType::Storage { read_only: false },
                    _ => wgpu::BufferBindingType::Storage { read_only: true },
                };
                let has_dynamic_offset = mem.read_u32(p + 24 + 8)? != 0;
                let min_sz = mem.read_u64(p + 24 + 16)?;
                wgpu::BindingType::Buffer {
                    ty: bty,
                    has_dynamic_offset,
                    min_binding_size: NonZeroU64::new(min_sz),
                }
            } else if samp_type != 0 {
                let sty = match samp_type {
                    2 => wgpu::SamplerBindingType::Filtering,
                    3 => wgpu::SamplerBindingType::NonFiltering,
                    _ => wgpu::SamplerBindingType::Comparison,
                };
                wgpu::BindingType::Sampler(sty)
            } else if tex_sample_type != 0 {
                let sample_type = match tex_sample_type {
                    2 => wgpu::TextureSampleType::Float { filterable: true },
                    3 => wgpu::TextureSampleType::Float { filterable: false },
                    4 => wgpu::TextureSampleType::Depth,
                    5 => wgpu::TextureSampleType::Sint,
                    _ => wgpu::TextureSampleType::Uint,
                };
                let view_dimension = id_to_texture_view_dim(mem.read_u32(p + 56 + 8)?)
                    .unwrap_or(wgpu::TextureViewDimension::D2);
                let multisampled = mem.read_u32(p + 56 + 12)? != 0;
                wgpu::BindingType::Texture {
                    sample_type,
                    view_dimension,
                    multisampled,
                }
            } else if stex_access != 0 {
                let access = match stex_access {
                    2 => wgpu::StorageTextureAccess::WriteOnly,
                    3 => wgpu::StorageTextureAccess::ReadOnly,
                    _ => wgpu::StorageTextureAccess::ReadWrite,
                };
                let format = id_to_texture_format(mem.read_u32(p + 72 + 8)?)
                    .unwrap_or(wgpu::TextureFormat::Rgba8Unorm);
                let view_dimension = id_to_texture_view_dim(mem.read_u32(p + 72 + 12)?)
                    .unwrap_or(wgpu::TextureViewDimension::D2);
                wgpu::BindingType::StorageTexture {
                    access,
                    format,
                    view_dimension,
                }
            } else {
                continue;
            };

            entries.push(wgpu::BindGroupLayoutEntry {
                binding,
                visibility,
                ty,
                count: None,
            });
        }

        let bgl = self.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: label.as_deref(),
            entries: &entries,
        });
        self.bind_group_layouts.lock().unwrap().insert(id, bgl);
        Ok(())
    }

    fn create_pipeline_layout(
        &self,
        mem: &SharedMem,
        id: i32,
        desc_ptr: i32,
    ) -> wasmtime::Result<()> {
        let label = read_opt_string_view(mem, desc_ptr + 4)?;
        let count = mem.read_u32(desc_ptr + 12)?;
        let ptr = mem.read_i32(desc_ptr + 16)?;
        let immediate_size = mem.read_u32(desc_ptr + 20)?;

        let bgl_map = self.bind_group_layouts.lock().unwrap();
        let mut owned_bgls = Vec::with_capacity(count as usize);
        for i in 0..count {
            let bgl_id = mem.read_i32(ptr + (i as i32 * 4))?;
            owned_bgls.push(bgl_map.get(&bgl_id).cloned());
        }
        drop(bgl_map);

        let refs: Vec<Option<&wgpu::BindGroupLayout>> =
            owned_bgls.iter().map(|b| b.as_ref()).collect();
        let pl = self.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: label.as_deref(),
            bind_group_layouts: &refs,
            immediate_size,
        });
        self.pipeline_layouts.lock().unwrap().insert(id, pl);
        Ok(())
    }

    fn create_bind_group(&self, mem: &SharedMem, id: i32, desc_ptr: i32) -> wasmtime::Result<()> {
        let label = read_opt_string_view(mem, desc_ptr + 4)?;
        let layout_id = mem.read_i32(desc_ptr + 12)?;
        let count = mem.read_u32(desc_ptr + 16)?;
        let entries_ptr = mem.read_i32(desc_ptr + 20)?;

        let layout = self.bind_group_layouts.lock().unwrap().get(&layout_id).cloned();
        let Some(layout) = layout else {
            return Ok(());
        };

        enum OwnedRes {
            Buf {
                binding: u32,
                buffer: wgpu::Buffer,
                offset: u64,
                size: Option<NonZeroU64>,
            },
            Samp {
                binding: u32,
                sampler: wgpu::Sampler,
            },
            View {
                binding: u32,
                view: wgpu::TextureView,
            },
        }

        let buffers = self.buffers.lock().unwrap();
        let samplers = self.samplers.lock().unwrap();
        let views = self.texture_views.lock().unwrap();

        let mut owned = Vec::with_capacity(count as usize);
        for i in 0..count {
            let s = entries_ptr + (i as i32 * 40);
            let binding = mem.read_u32(s + 4)?;
            let buf_id = mem.read_i32(s + 8)?;
            let samp_id = mem.read_i32(s + 32)?;
            let view_id = mem.read_i32(s + 36)?;

            if buf_id != 0 {
                if let Some(b) = buffers.get(&buf_id) {
                    let offset = mem.read_u64(s + 16)?;
                    let sz_raw = mem.read_i64(s + 24)?;
                    let size = if sz_raw < 0 {
                        None
                    } else {
                        NonZeroU64::new(sz_raw as u64)
                    };
                    owned.push(OwnedRes::Buf {
                        binding,
                        buffer: b.buffer.clone(),
                        offset,
                        size,
                    });
                }
            } else if samp_id != 0 {
                if let Some(samp) = samplers.get(&samp_id) {
                    owned.push(OwnedRes::Samp {
                        binding,
                        sampler: samp.clone(),
                    });
                }
            } else if view_id != 0 {
                if let Some(v) = views.get(&view_id) {
                    owned.push(OwnedRes::View {
                        binding,
                        view: v.clone(),
                    });
                }
            }
        }
        drop(buffers);
        drop(samplers);
        drop(views);

        let entries: Vec<wgpu::BindGroupEntry<'_>> = owned
            .iter()
            .map(|r| match r {
                OwnedRes::Buf {
                    binding,
                    buffer,
                    offset,
                    size,
                } => wgpu::BindGroupEntry {
                    binding: *binding,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer,
                        offset: *offset,
                        size: *size,
                    }),
                },
                OwnedRes::Samp { binding, sampler } => wgpu::BindGroupEntry {
                    binding: *binding,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
                OwnedRes::View { binding, view } => wgpu::BindGroupEntry {
                    binding: *binding,
                    resource: wgpu::BindingResource::TextureView(view),
                },
            })
            .collect();

        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: label.as_deref(),
            layout: &layout,
            entries: &entries,
        });
        self.bind_groups.lock().unwrap().insert(id, bg);
        Ok(())
    }

    fn create_render_pipeline(
        &self,
        mem: &SharedMem,
        id: i32,
        desc_ptr: i32,
    ) -> wasmtime::Result<()> {
        let label = read_opt_string_view(mem, desc_ptr + 4)?;
        let layout_id = mem.read_i32(desc_ptr + 12)?;
        let layout = if layout_id != 0 {
            self.pipeline_layouts.lock().unwrap().get(&layout_id).cloned()
        } else {
            None
        };

        // Vertex state at desc_ptr + 16
        let vs_ptr = desc_ptr + 16;
        let vs_mod_id = mem.read_i32(vs_ptr + 4)?;
        let vs_entry = read_opt_string_view(mem, vs_ptr + 8)?;
        let vs_const_cnt = mem.read_u32(vs_ptr + 16)?;
        let vs_const_ptr = mem.read_i32(vs_ptr + 20)?;
        let vs_constants = read_pipeline_constants(mem, vs_const_cnt, vs_const_ptr)?;
        let vb_count = mem.read_u32(vs_ptr + 24)?;
        let vb_ptr = mem.read_i32(vs_ptr + 28)?;

        struct OwnedVb {
            array_stride: u64,
            step_mode: wgpu::VertexStepMode,
            attrs: Vec<wgpu::VertexAttribute>,
        }
        let mut owned_vbs: Vec<Option<OwnedVb>> = Vec::with_capacity(vb_count as usize);
        for i in 0..vb_count {
            let b = vb_ptr + (i as i32 * 24);
            let step_id = mem.read_u32(b + 4)?;
            let attr_cnt = mem.read_u32(b + 16)?;
            if step_id == 0 && attr_cnt == 0 {
                owned_vbs.push(None);
                continue;
            }
            let array_stride = mem.read_u64(b + 8)?;
            let step_mode = match step_id {
                2 => wgpu::VertexStepMode::Instance,
                _ => wgpu::VertexStepMode::Vertex,
            };
            let attr_ptr = mem.read_i32(b + 20)?;
            let mut attrs = Vec::with_capacity(attr_cnt as usize);
            for j in 0..attr_cnt {
                let ap = attr_ptr + (j as i32 * 24);
                let format = id_to_vertex_format(mem.read_u32(ap + 4)?)
                    .unwrap_or(wgpu::VertexFormat::Float32x4);
                let offset = mem.read_u64(ap + 8)?;
                let shader_location = mem.read_u32(ap + 16)?;
                attrs.push(wgpu::VertexAttribute {
                    format,
                    offset,
                    shader_location,
                });
            }
            owned_vbs.push(Some(OwnedVb {
                array_stride,
                step_mode,
                attrs,
            }));
        }

        let vb_layouts: Vec<Option<wgpu::VertexBufferLayout<'_>>> = owned_vbs
            .iter()
            .map(|ov| {
                ov.as_ref().map(|v| wgpu::VertexBufferLayout {
                    array_stride: v.array_stride,
                    step_mode: v.step_mode,
                    attributes: &v.attrs,
                })
            })
            .collect();

        // Primitive state at desc_ptr + 48
        let prim_ptr = desc_ptr + 48;
        let primitive = wgpu::PrimitiveState {
            topology: id_to_topology(mem.read_u32(prim_ptr + 4)?),
            strip_index_format: id_to_index_format_opt(mem.read_u32(prim_ptr + 8)?),
            front_face: match mem.read_u32(prim_ptr + 12)? {
                2 => wgpu::FrontFace::Cw,
                _ => wgpu::FrontFace::Ccw,
            },
            cull_mode: match mem.read_u32(prim_ptr + 16)? {
                2 => Some(wgpu::Face::Front),
                3 => Some(wgpu::Face::Back),
                _ => None,
            },
            unclipped_depth: mem.read_u32(prim_ptr + 20)? != 0,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        };

        // DepthStencil at desc_ptr + 72
        let ds_ptr = mem.read_i32(desc_ptr + 72)?;
        let depth_stencil = if ds_ptr != 0 {
            let format = id_to_texture_format(mem.read_u32(ds_ptr + 4)?)
                .unwrap_or(wgpu::TextureFormat::Depth24PlusStencil8);
            let depth_write_enabled = Some(mem.read_u32(ds_ptr + 8)? != 0);
            let depth_compare = id_to_compare_opt(mem.read_u32(ds_ptr + 12)?);
            let stencil = wgpu::StencilState {
                front: read_stencil_face(mem, ds_ptr + 16)?,
                back: read_stencil_face(mem, ds_ptr + 32)?,
                read_mask: mem.read_u32(ds_ptr + 48)?,
                write_mask: mem.read_u32(ds_ptr + 52)?,
            };
            let bias = wgpu::DepthBiasState {
                constant: mem.read_i32(ds_ptr + 56)?,
                slope_scale: mem.read_f32(ds_ptr + 60)?,
                clamp: mem.read_f32(ds_ptr + 64)?,
            };
            Some(wgpu::DepthStencilState {
                format,
                depth_write_enabled,
                depth_compare,
                stencil,
                bias,
            })
        } else {
            None
        };

        // Multisample at desc_ptr + 76
        let ms_ptr = desc_ptr + 76;
        let multisample = wgpu::MultisampleState {
            count: mem.read_u32(ms_ptr + 4)?.max(1),
            mask: mem.read_u32(ms_ptr + 8)? as u64,
            alpha_to_coverage_enabled: mem.read_u32(ms_ptr + 12)? != 0,
        };

        // Fragment at desc_ptr + 92
        let fs_ptr = mem.read_i32(desc_ptr + 92)?;
        let mut fs_mod = None;
        let mut fs_entry = None;
        let mut fs_constants: Vec<(String, f64)> = Vec::new();
        let mut fs_targets: Vec<Option<wgpu::ColorTargetState>> = Vec::new();
        if fs_ptr != 0 {
            let fs_mod_id = mem.read_i32(fs_ptr + 4)?;
            fs_mod = self.shaders.lock().unwrap().get(&fs_mod_id).cloned();
            fs_entry = read_opt_string_view(mem, fs_ptr + 8)?;
            let fs_const_cnt = mem.read_u32(fs_ptr + 16)?;
            let fs_const_ptr = mem.read_i32(fs_ptr + 20)?;
            fs_constants = read_pipeline_constants(mem, fs_const_cnt, fs_const_ptr)?;
            let target_cnt = mem.read_u32(fs_ptr + 24)?;
            let target_ptr = mem.read_i32(fs_ptr + 28)?;
            for i in 0..target_cnt {
                let tp = target_ptr + (i as i32 * 24);
                let fmt_opt = id_to_texture_format(mem.read_u32(tp + 4)?);
                if let Some(format) = fmt_opt {
                    let blend_ptr = mem.read_i32(tp + 8)?;
                    let blend = if blend_ptr != 0 {
                        Some(wgpu::BlendState {
                            color: read_blend_component(mem, blend_ptr)?,
                            alpha: read_blend_component(mem, blend_ptr + 12)?,
                        })
                    } else {
                        None
                    };
                    let write_mask = wgpu::ColorWrites::from_bits_truncate(mem.read_u32(tp + 16)?);
                    fs_targets.push(Some(wgpu::ColorTargetState {
                        format,
                        blend,
                        write_mask,
                    }));
                } else {
                    fs_targets.push(None);
                }
            }
        }

        let vs_mod = self.shaders.lock().unwrap().get(&vs_mod_id).cloned();
        let Some(vs_mod) = vs_mod else {
            return Ok(());
        };

        let vs_const_refs: Vec<(&str, f64)> =
            vs_constants.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        let fs_const_refs: Vec<(&str, f64)> =
            fs_constants.iter().map(|(k, v)| (k.as_str(), *v)).collect();

        let fragment = fs_mod.as_ref().map(|m| wgpu::FragmentState {
            module: m,
            entry_point: fs_entry.as_deref(),
            compilation_options: wgpu::PipelineCompilationOptions {
                constants: &fs_const_refs,
                ..Default::default()
            },
            targets: &fs_targets,
        });

        let pipeline = self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: label.as_deref(),
            layout: layout.as_ref(),
            vertex: wgpu::VertexState {
                module: &vs_mod,
                entry_point: vs_entry.as_deref(),
                compilation_options: wgpu::PipelineCompilationOptions {
                    constants: &vs_const_refs,
                    ..Default::default()
                },
                buffers: &vb_layouts,
            },
            primitive,
            depth_stencil,
            multisample,
            fragment,
            multiview_mask: None,
            cache: None,
        });
        self.render_pipelines.lock().unwrap().insert(id, pipeline);
        Ok(())
    }

    fn create_compute_pipeline(
        &self,
        mem: &SharedMem,
        id: i32,
        desc_ptr: i32,
    ) -> wasmtime::Result<()> {
        let label = read_opt_string_view(mem, desc_ptr + 4)?;
        let layout_id = mem.read_i32(desc_ptr + 12)?;
        let layout = if layout_id != 0 {
            self.pipeline_layouts.lock().unwrap().get(&layout_id).cloned()
        } else {
            None
        };
        let cs_ptr = desc_ptr + 16;
        let mod_id = mem.read_i32(cs_ptr + 4)?;
        let entry_point = read_opt_string_view(mem, cs_ptr + 8)?;
        let const_cnt = mem.read_u32(cs_ptr + 16)?;
        let const_ptr = mem.read_i32(cs_ptr + 20)?;
        let constants = read_pipeline_constants(mem, const_cnt, const_ptr)?;
        let const_refs: Vec<(&str, f64)> = constants.iter().map(|(k, v)| (k.as_str(), *v)).collect();

        let module = self.shaders.lock().unwrap().get(&mod_id).cloned();
        let Some(module) = module else {
            return Ok(());
        };

        let pipeline = self.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: label.as_deref(),
            layout: layout.as_ref(),
            module: &module,
            entry_point: entry_point.as_deref(),
            compilation_options: wgpu::PipelineCompilationOptions {
                constants: &const_refs,
                ..Default::default()
            },
            cache: None,
        });
        self.compute_pipelines.lock().unwrap().insert(id, pipeline);
        Ok(())
    }

    fn create_query_set(&self, mem: &SharedMem, id: i32, desc_ptr: i32) -> wasmtime::Result<()> {
        let label = read_opt_string_view(mem, desc_ptr + 4)?;
        let ty_id = mem.read_u32(desc_ptr + 12)?;
        let count = mem.read_u32(desc_ptr + 16)?.max(1);
        let ty = match ty_id {
            2 => wgpu::QueryType::Timestamp,
            _ => wgpu::QueryType::Occlusion,
        };
        let qs = self.device.create_query_set(&wgpu::QuerySetDescriptor {
            label: label.as_deref(),
            ty,
            count,
        });
        self.query_sets.lock().unwrap().insert(id, qs);
        Ok(())
    }

    fn surface_configure(&self, mem: &SharedMem, _surf_id: i32, cfg_ptr: i32) -> wasmtime::Result<()> {
        let width = mem.read_u32(cfg_ptr + 24)?;
        let height = mem.read_u32(cfg_ptr + 28)?;
        let mut st = self.surface_state.lock().unwrap();
        if width > 0 {
            st.config.width = width;
        }
        if height > 0 {
            st.config.height = height;
        }
        st.surface.configure(&self.device, &st.config);
        Ok(())
    }

    fn create_command_encoder(
        &self,
        mem: &SharedMem,
        id: i32,
        desc_ptr: i32,
    ) -> wasmtime::Result<()> {
        let label = if desc_ptr != 0 {
            read_opt_string_view(mem, desc_ptr + 4)?
        } else {
            None
        };
        let enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: label.as_deref(),
        });
        self.command_encoders.lock().unwrap().insert(id, enc);
        Ok(())
    }

    fn encoder_begin_render_pass(
        &self,
        mem: &SharedMem,
        enc_id: i32,
        pass_id: i32,
        desc_ptr: i32,
    ) -> wasmtime::Result<()> {
        let label = read_opt_string_view(mem, desc_ptr + 4)?;
        let color_cnt = mem.read_u32(desc_ptr + 12)?;
        let color_ptr = mem.read_i32(desc_ptr + 16)?;
        let ds_ptr = mem.read_i32(desc_ptr + 20)?;
        let occ_qs_id = mem.read_i32(desc_ptr + 24)?;
        let ts_ptr = mem.read_i32(desc_ptr + 28)?;

        struct OwnedColor {
            view: wgpu::TextureView,
            depth_slice: Option<u32>,
            resolve_target: Option<wgpu::TextureView>,
            ops: wgpu::Operations<wgpu::Color>,
        }

        let views = self.texture_views.lock().unwrap();
        let mut owned_colors: Vec<Option<OwnedColor>> = Vec::with_capacity(color_cnt as usize);
        for i in 0..color_cnt {
            let u = color_ptr + (i as i32 * 56);
            let view_id = mem.read_i32(u + 4)?;
            if view_id == 0 {
                owned_colors.push(None);
                continue;
            }
            let Some(view) = views.get(&view_id).cloned() else {
                owned_colors.push(None);
                continue;
            };
            let slice_raw = mem.read_u32(u + 8)?;
            let depth_slice = (slice_raw != u32::MAX).then_some(slice_raw);
            let resolve_id = mem.read_i32(u + 12)?;
            let resolve_target = if resolve_id != 0 {
                views.get(&resolve_id).cloned()
            } else {
                None
            };
            let load_op = mem.read_u32(u + 16)?;
            let store_op = mem.read_u32(u + 20)?;
            let clear_value = wgpu::Color {
                r: mem.read_f64(u + 24)?,
                g: mem.read_f64(u + 32)?,
                b: mem.read_f64(u + 40)?,
                a: mem.read_f64(u + 48)?,
            };
            let load = match load_op {
                2 => wgpu::LoadOp::Clear(clear_value),
                _ => wgpu::LoadOp::Load,
            };
            let store = match store_op {
                2 => wgpu::StoreOp::Discard,
                _ => wgpu::StoreOp::Store,
            };
            owned_colors.push(Some(OwnedColor {
                view,
                depth_slice,
                resolve_target,
                ops: wgpu::Operations { load, store },
            }));
        }

        struct OwnedDs {
            view: wgpu::TextureView,
            depth_ops: Option<wgpu::Operations<f32>>,
            stencil_ops: Option<wgpu::Operations<u32>>,
        }
        let owned_ds = if ds_ptr != 0 {
            let view_id = mem.read_i32(ds_ptr + 4)?;
            if let Some(view) = views.get(&view_id).cloned() {
                let d_load_id = mem.read_u32(ds_ptr + 8)?;
                let d_store_id = mem.read_u32(ds_ptr + 12)?;
                let d_clear = mem.read_f32(ds_ptr + 16)?;
                let d_ro = mem.read_u32(ds_ptr + 20)? != 0;
                let depth_ops = if d_ro || d_load_id == 0 {
                    None
                } else {
                    Some(wgpu::Operations {
                        load: if d_load_id == 2 {
                            wgpu::LoadOp::Clear(d_clear)
                        } else {
                            wgpu::LoadOp::Load
                        },
                        store: if d_store_id == 2 {
                            wgpu::StoreOp::Discard
                        } else {
                            wgpu::StoreOp::Store
                        },
                    })
                };

                let s_load_id = mem.read_u32(ds_ptr + 24)?;
                let s_store_id = mem.read_u32(ds_ptr + 28)?;
                let s_clear = mem.read_u32(ds_ptr + 32)?;
                let s_ro = mem.read_u32(ds_ptr + 36)? != 0;
                let stencil_ops = if s_ro || s_load_id == 0 {
                    None
                } else {
                    Some(wgpu::Operations {
                        load: if s_load_id == 2 {
                            wgpu::LoadOp::Clear(s_clear)
                        } else {
                            wgpu::LoadOp::Load
                        },
                        store: if s_store_id == 2 {
                            wgpu::StoreOp::Discard
                        } else {
                            wgpu::StoreOp::Store
                        },
                    })
                };

                Some(OwnedDs {
                    view,
                    depth_ops,
                    stencil_ops,
                })
            } else {
                None
            }
        } else {
            None
        };
        drop(views);

        let query_sets = self.query_sets.lock().unwrap();
        let occ_qs = if occ_qs_id != 0 {
            query_sets.get(&occ_qs_id).cloned()
        } else {
            None
        };
        let ts_owned = if ts_ptr != 0 {
            let qs_id = mem.read_i32(ts_ptr + 4)?;
            let beg = mem.read_u32(ts_ptr + 8)?;
            let end = mem.read_u32(ts_ptr + 12)?;
            query_sets.get(&qs_id).cloned().map(|qs| (qs, beg, end))
        } else {
            None
        };
        drop(query_sets);

        let color_attachments: Vec<Option<wgpu::RenderPassColorAttachment<'_>>> = owned_colors
            .iter()
            .map(|oc| {
                oc.as_ref().map(|c| wgpu::RenderPassColorAttachment {
                    view: &c.view,
                    depth_slice: c.depth_slice,
                    resolve_target: c.resolve_target.as_ref(),
                    ops: c.ops,
                })
            })
            .collect();

        let depth_stencil_attachment = owned_ds.as_ref().map(|ds| {
            wgpu::RenderPassDepthStencilAttachment {
                view: &ds.view,
                depth_ops: ds.depth_ops,
                stencil_ops: ds.stencil_ops,
            }
        });

        let timestamp_writes = ts_owned.as_ref().map(|(qs, beg, end)| {
            wgpu::RenderPassTimestampWrites {
                query_set: qs,
                beginning_of_pass_write_index: (*beg != u32::MAX).then_some(*beg),
                end_of_pass_write_index: (*end != u32::MAX).then_some(*end),
            }
        });

        let mut enc_map = self.command_encoders.lock().unwrap();
        let Some(enc) = enc_map.get_mut(&enc_id) else {
            return Ok(());
        };
        let pass = enc
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: label.as_deref(),
                color_attachments: &color_attachments,
                depth_stencil_attachment,
                timestamp_writes,
                occlusion_query_set: occ_qs.as_ref(),
                multiview_mask: None,
            })
            .forget_lifetime();
        drop(enc_map);

        self.render_passes.lock().unwrap().insert(pass_id, pass);
        Ok(())
    }

    fn encoder_begin_compute_pass(
        &self,
        mem: &SharedMem,
        enc_id: i32,
        pass_id: i32,
        desc_ptr: i32,
    ) -> wasmtime::Result<()> {
        let label = if desc_ptr != 0 {
            read_opt_string_view(mem, desc_ptr + 4)?
        } else {
            None
        };
        let ts_ptr = if desc_ptr != 0 {
            mem.read_i32(desc_ptr + 12)?
        } else {
            0
        };
        let ts_owned = if ts_ptr != 0 {
            let qs_id = mem.read_i32(ts_ptr + 4)?;
            let beg = mem.read_u32(ts_ptr + 8)?;
            let end = mem.read_u32(ts_ptr + 12)?;
            self.query_sets
                .lock()
                .unwrap()
                .get(&qs_id)
                .cloned()
                .map(|qs| (qs, beg, end))
        } else {
            None
        };
        let timestamp_writes = ts_owned.as_ref().map(|(qs, beg, end)| {
            wgpu::ComputePassTimestampWrites {
                query_set: qs,
                beginning_of_pass_write_index: (*beg != u32::MAX).then_some(*beg),
                end_of_pass_write_index: (*end != u32::MAX).then_some(*end),
            }
        });

        let mut enc_map = self.command_encoders.lock().unwrap();
        let Some(enc) = enc_map.get_mut(&enc_id) else {
            return Ok(());
        };
        let pass = enc
            .begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: label.as_deref(),
                timestamp_writes,
            })
            .forget_lifetime();
        drop(enc_map);
        self.compute_passes.lock().unwrap().insert(pass_id, pass);
        Ok(())
    }

    fn encoder_finish(&self, enc_id: i32, cmd_id: i32) {
        if let Some(enc) = self.command_encoders.lock().unwrap().remove(&enc_id) {
            let cmd = enc.finish();
            self.command_buffers.lock().unwrap().insert(cmd_id, cmd);
        }
    }

    fn encoder_copy_buffer_to_buffer(
        &self,
        enc_id: i32,
        src: i32,
        src_off: u64,
        dst: i32,
        dst_off: u64,
        size: u64,
    ) {
        let buffers = self.buffers.lock().unwrap();
        let (Some(sb), Some(db)) = (buffers.get(&src), buffers.get(&dst)) else {
            return;
        };
        let sbuf = sb.buffer.clone();
        let dbuf = db.buffer.clone();
        drop(buffers);
        if let Some(enc) = self.command_encoders.lock().unwrap().get_mut(&enc_id) {
            enc.copy_buffer_to_buffer(&sbuf, src_off, &dbuf, dst_off, size);
        }
    }

    fn encoder_copy_texture_to_texture(
        &self,
        mem: &SharedMem,
        enc_id: i32,
        src_ptr: i32,
        dst_ptr: i32,
        ext_ptr: i32,
    ) -> wasmtime::Result<()> {
        let src_tex_id = mem.read_i32(src_ptr)?;
        let src_mip = mem.read_u32(src_ptr + 4)?;
        let src_origin = read_origin3d(mem, src_ptr + 8)?;
        let src_aspect = id_to_aspect(mem.read_u32(src_ptr + 20)?);

        let dst_tex_id = mem.read_i32(dst_ptr)?;
        let dst_mip = mem.read_u32(dst_ptr + 4)?;
        let dst_origin = read_origin3d(mem, dst_ptr + 8)?;
        let dst_aspect = id_to_aspect(mem.read_u32(dst_ptr + 20)?);

        let extent = read_extent3d(mem, ext_ptr)?;

        let textures = self.textures.lock().unwrap();
        let (Some(st), Some(dt)) = (textures.get(&src_tex_id), textures.get(&dst_tex_id)) else {
            return Ok(());
        };
        let stex = st.texture.clone();
        let dtex = dt.texture.clone();
        drop(textures);

        if let Some(enc) = self.command_encoders.lock().unwrap().get_mut(&enc_id) {
            enc.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &stex,
                    mip_level: src_mip,
                    origin: src_origin,
                    aspect: src_aspect,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &dtex,
                    mip_level: dst_mip,
                    origin: dst_origin,
                    aspect: dst_aspect,
                },
                extent,
            );
        }
        Ok(())
    }

    fn encoder_copy_texture_to_buffer(
        &self,
        mem: &SharedMem,
        enc_id: i32,
        src_ptr: i32,
        dst_ptr: i32,
        ext_ptr: i32,
    ) -> wasmtime::Result<()> {
        let src_tex_id = mem.read_i32(src_ptr)?;
        let src_mip = mem.read_u32(src_ptr + 4)?;
        let src_origin = read_origin3d(mem, src_ptr + 8)?;
        let src_aspect = id_to_aspect(mem.read_u32(src_ptr + 20)?);

        let dst_layout = read_texel_copy_buffer_layout(mem, dst_ptr)?;
        let dst_buf_id = mem.read_i32(dst_ptr + 16)?;
        let extent = read_extent3d(mem, ext_ptr)?;

        let tex = self.textures.lock().unwrap().get(&src_tex_id).map(|t| t.texture.clone());
        let buf = self.buffers.lock().unwrap().get(&dst_buf_id).map(|b| b.buffer.clone());
        let (Some(tex), Some(buf)) = (tex, buf) else {
            return Ok(());
        };
        if let Some(enc) = self.command_encoders.lock().unwrap().get_mut(&enc_id) {
            enc.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &tex,
                    mip_level: src_mip,
                    origin: src_origin,
                    aspect: src_aspect,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &buf,
                    layout: dst_layout,
                },
                extent,
            );
        }
        Ok(())
    }

    fn encoder_resolve_query_set(
        &self,
        enc_id: i32,
        qs_id: i32,
        first: u32,
        count: u32,
        dst_buf: i32,
        dst_off: u64,
    ) {
        let qs = self.query_sets.lock().unwrap().get(&qs_id).cloned();
        let buf = self.buffers.lock().unwrap().get(&dst_buf).map(|b| b.buffer.clone());
        let (Some(qs), Some(buf)) = (qs, buf) else {
            return;
        };
        if let Some(enc) = self.command_encoders.lock().unwrap().get_mut(&enc_id) {
            enc.resolve_query_set(&qs, first..first + count, &buf, dst_off);
        }
    }

    fn encoder_push_debug_group(
        &self,
        mem: &SharedMem,
        enc_id: i32,
        lbl_ptr: i32,
    ) -> wasmtime::Result<()> {
        let lbl = read_string_view(mem, lbl_ptr)?;
        if let Some(enc) = self.command_encoders.lock().unwrap().get_mut(&enc_id) {
            enc.push_debug_group(&lbl);
        }
        Ok(())
    }

    fn encoder_pop_debug_group(&self, enc_id: i32) {
        if let Some(enc) = self.command_encoders.lock().unwrap().get_mut(&enc_id) {
            enc.pop_debug_group();
        }
    }

    fn compute_pass_set_pipeline(&self, pass_id: i32, pl_id: i32) {
        let pl = self.compute_pipelines.lock().unwrap().get(&pl_id).cloned();
        if let (Some(pass), Some(pl)) = (self.compute_passes.lock().unwrap().get_mut(&pass_id), pl) {
            pass.set_pipeline(&pl);
        }
    }

    fn compute_pass_set_bind_group(
        &self,
        mem: &SharedMem,
        pass_id: i32,
        idx: u32,
        bg_id: i32,
        dyn_cnt: u32,
        dyn_ptr: i32,
    ) -> wasmtime::Result<()> {
        let mut offsets = Vec::with_capacity(dyn_cnt as usize);
        for i in 0..dyn_cnt {
            offsets.push(mem.read_u32(dyn_ptr + (i as i32 * 4))?);
        }
        let bg = self.bind_groups.lock().unwrap().get(&bg_id).cloned();
        if let Some(pass) = self.compute_passes.lock().unwrap().get_mut(&pass_id) {
            pass.set_bind_group(idx, bg.as_ref(), &offsets);
        }
        Ok(())
    }

    fn compute_pass_dispatch(&self, pass_id: i32, x: u32, y: u32, z: u32) {
        if let Some(pass) = self.compute_passes.lock().unwrap().get_mut(&pass_id) {
            pass.dispatch_workgroups(x, y, z);
        }
    }

    fn compute_pass_push_debug_group(
        &self,
        mem: &SharedMem,
        pass_id: i32,
        lbl_ptr: i32,
    ) -> wasmtime::Result<()> {
        let lbl = read_string_view(mem, lbl_ptr)?;
        if let Some(pass) = self.compute_passes.lock().unwrap().get_mut(&pass_id) {
            pass.push_debug_group(&lbl);
        }
        Ok(())
    }

    fn compute_pass_pop_debug_group(&self, pass_id: i32) {
        if let Some(pass) = self.compute_passes.lock().unwrap().get_mut(&pass_id) {
            pass.pop_debug_group();
        }
    }

    fn compute_pass_end(&self, pass_id: i32) {
        self.compute_passes.lock().unwrap().remove(&pass_id);
    }

    fn render_pass_set_pipeline(&self, pass_id: i32, pl_id: i32) {
        let pl = self.render_pipelines.lock().unwrap().get(&pl_id).cloned();
        if let (Some(pass), Some(pl)) = (self.render_passes.lock().unwrap().get_mut(&pass_id), pl) {
            pass.set_pipeline(&pl);
        }
    }

    fn render_pass_set_bind_group(
        &self,
        mem: &SharedMem,
        pass_id: i32,
        idx: u32,
        bg_id: i32,
        dyn_cnt: u32,
        dyn_ptr: i32,
    ) -> wasmtime::Result<()> {
        let mut offsets = Vec::with_capacity(dyn_cnt as usize);
        for i in 0..dyn_cnt {
            offsets.push(mem.read_u32(dyn_ptr + (i as i32 * 4))?);
        }
        let bg = self.bind_groups.lock().unwrap().get(&bg_id).cloned();
        if let Some(pass) = self.render_passes.lock().unwrap().get_mut(&pass_id) {
            pass.set_bind_group(idx, bg.as_ref(), &offsets);
        }
        Ok(())
    }

    fn render_pass_set_vertex_buffer(
        &self,
        pass_id: i32,
        slot: u32,
        buf_id: i32,
        off: u64,
        size: i64,
    ) {
        let buf = self.buffers.lock().unwrap().get(&buf_id).map(|b| b.buffer.clone());
        if let Some(pass) = self.render_passes.lock().unwrap().get_mut(&pass_id) {
            if let Some(ref b) = buf {
                if size < 0 {
                    pass.set_vertex_buffer(slot, b.slice(off..));
                } else {
                    pass.set_vertex_buffer(slot, b.slice(off..off + size as u64));
                }
            }
        }
    }

    fn render_pass_set_index_buffer(
        &self,
        pass_id: i32,
        buf_id: i32,
        fmt: i32,
        off: u64,
        size: i64,
    ) {
        let buf = self.buffers.lock().unwrap().get(&buf_id).map(|b| b.buffer.clone());
        let format = match fmt {
            1 => wgpu::IndexFormat::Uint16,
            _ => wgpu::IndexFormat::Uint32,
        };
        if let (Some(pass), Some(ref b)) = (self.render_passes.lock().unwrap().get_mut(&pass_id), buf) {
            if size < 0 {
                pass.set_index_buffer(b.slice(off..), format);
            } else {
                pass.set_index_buffer(b.slice(off..off + size as u64), format);
            }
        }
    }

    fn render_pass_draw(&self, pass_id: i32, vc: u32, ic: u32, fv: u32, fi: u32) {
        if let Some(pass) = self.render_passes.lock().unwrap().get_mut(&pass_id) {
            pass.draw(fv..fv + vc, fi..fi + ic);
        }
    }

    fn render_pass_draw_indexed(
        &self,
        pass_id: i32,
        ic: u32,
        inc: u32,
        fi: u32,
        bv: i32,
        fin: u32,
    ) {
        if let Some(pass) = self.render_passes.lock().unwrap().get_mut(&pass_id) {
            pass.draw_indexed(fi..fi + ic, bv, fin..fin + inc);
        }
    }

    fn render_pass_set_viewport(
        &self,
        pass_id: i32,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        min_d: f32,
        max_d: f32,
    ) {
        if let Some(pass) = self.render_passes.lock().unwrap().get_mut(&pass_id) {
            pass.set_viewport(x, y, w, h, min_d, max_d);
        }
    }

    fn render_pass_set_scissor_rect(&self, pass_id: i32, x: u32, y: u32, w: u32, h: u32) {
        if let Some(pass) = self.render_passes.lock().unwrap().get_mut(&pass_id) {
            pass.set_scissor_rect(x, y, w, h);
        }
    }

    fn render_pass_set_stencil_reference(&self, pass_id: i32, ref_val: u32) {
        if let Some(pass) = self.render_passes.lock().unwrap().get_mut(&pass_id) {
            pass.set_stencil_reference(ref_val);
        }
    }

    fn render_pass_push_debug_group(
        &self,
        mem: &SharedMem,
        pass_id: i32,
        lbl_ptr: i32,
    ) -> wasmtime::Result<()> {
        let lbl = read_string_view(mem, lbl_ptr)?;
        if let Some(pass) = self.render_passes.lock().unwrap().get_mut(&pass_id) {
            pass.push_debug_group(&lbl);
        }
        Ok(())
    }

    fn render_pass_pop_debug_group(&self, pass_id: i32) {
        if let Some(pass) = self.render_passes.lock().unwrap().get_mut(&pass_id) {
            pass.pop_debug_group();
        }
    }

    fn render_pass_end(&self, pass_id: i32) {
        self.render_passes.lock().unwrap().remove(&pass_id);
    }

    fn queue_submit(
        &self,
        mem: &SharedMem,
        _queue_id: i32,
        count: u32,
        cmds_ptr: i32,
    ) -> wasmtime::Result<()> {
        let mut cmds = Vec::with_capacity(count as usize);
        {
            let mut map = self.command_buffers.lock().unwrap();
            for i in 0..count {
                let id = mem.read_i32(cmds_ptr + (i as i32 * 4))?;
                if let Some(cmd) = map.remove(&id) {
                    cmds.push(cmd);
                }
            }
        }
        if !cmds.is_empty() {
            self.queue.submit(cmds);
        }
        Ok(())
    }

    fn queue_write_buffer(
        &self,
        mem: &SharedMem,
        _queue_id: i32,
        buf_id: i32,
        offset: u64,
        data_ptr: i32,
        size: u32,
    ) -> wasmtime::Result<()> {
        if size == 0 {
            return Ok(());
        }
        let buf = self.buffers.lock().unwrap().get(&buf_id).map(|b| b.buffer.clone());
        let Some(buf) = buf else {
            return Ok(());
        };
        let bytes = mem.read_bytes(data_ptr, size as usize)?;
        self.queue.write_buffer(&buf, offset, &bytes);
        Ok(())
    }

    fn queue_write_texture(
        &self,
        mem: &SharedMem,
        _queue_id: i32,
        dst_ptr: i32,
        data_ptr: i32,
        size: u32,
        layout_ptr: i32,
        ext_ptr: i32,
    ) -> wasmtime::Result<()> {
        let dst_tex_id = mem.read_i32(dst_ptr)?;
        let dst_mip = mem.read_u32(dst_ptr + 4)?;
        let dst_origin = read_origin3d(mem, dst_ptr + 8)?;
        let dst_aspect = id_to_aspect(mem.read_u32(dst_ptr + 20)?);

        let layout = read_texel_copy_buffer_layout(mem, layout_ptr)?;
        let extent = read_extent3d(mem, ext_ptr)?;

        let tex = self.textures.lock().unwrap().get(&dst_tex_id).map(|t| t.texture.clone());
        let Some(tex) = tex else {
            return Ok(());
        };
        let data = mem.read_bytes(data_ptr, size as usize)?;
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: dst_mip,
                origin: dst_origin,
                aspect: dst_aspect,
            },
            &data,
            layout,
            extent,
        );
        Ok(())
    }

    fn present_browser_frame(&self, _dev_id: i32, tex_id: i32, cmd_buf_id: i32) {
        let src_tex = self.textures.lock().unwrap().get(&tex_id).map(|t| {
            (t.texture.clone(), t.width, t.height, t.format)
        });
        let frame_cmd = self.command_buffers.lock().unwrap().remove(&cmd_buf_id);

        let st = self.surface_state.lock().unwrap();
        let surf_w = st.config.width;
        let surf_h = st.config.height;

        let frame = match st.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                st.surface.configure(&self.device, &st.config);
                match st.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(f)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
                    other => {
                        eprintln!(
                            "[wgpu] get_current_texture failed after reconfigure: {other:?}"
                        );
                        if let Some(cmd) = frame_cmd {
                            self.queue.submit([cmd]);
                        }
                        return;
                    }
                }
            }
            other => {
                eprintln!("[wgpu] get_current_texture skipped: {other:?}");
                if let Some(cmd) = frame_cmd {
                    self.queue.submit([cmd]);
                }
                return;
            }
        };

        let mut copy_cmds = Vec::new();
        if let Some(cmd) = frame_cmd {
            copy_cmds.push(cmd);
        }

        if let Some((src, tw, th, t_fmt)) = src_tex {
            if st.supports_copy_dst && t_fmt == st.config.format {
                let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("presentBrowserFrame copy"),
                });
                enc.copy_texture_to_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &src,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyTextureInfo {
                        texture: &frame.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::Extent3d {
                        width: tw.min(surf_w),
                        height: th.min(surf_h),
                        depth_or_array_layers: 1,
                    },
                );
                copy_cmds.push(enc.finish());
            }
        }

        if !copy_cmds.is_empty() {
            self.queue.submit(copy_cmds);
        }
        self.queue.present(frame);
    }

    fn resize_surface(&self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        let mut st = self.surface_state.lock().unwrap();
        if st.config.width != width || st.config.height != height {
            st.config.width = width;
            st.config.height = height;
            st.surface.configure(&self.device, &st.config);
        }
    }

    fn remove_object(&self, id: i32) {
        self.buffers.lock().unwrap().remove(&id);
        self.shaders.lock().unwrap().remove(&id);
        self.textures.lock().unwrap().remove(&id);
        self.texture_views.lock().unwrap().remove(&id);
        self.samplers.lock().unwrap().remove(&id);
        self.bind_group_layouts.lock().unwrap().remove(&id);
        self.pipeline_layouts.lock().unwrap().remove(&id);
        self.bind_groups.lock().unwrap().remove(&id);
        self.render_pipelines.lock().unwrap().remove(&id);
        self.compute_pipelines.lock().unwrap().remove(&id);
        self.query_sets.lock().unwrap().remove(&id);
        self.render_passes.lock().unwrap().remove(&id);
        self.compute_passes.lock().unwrap().remove(&id);
        self.command_encoders.lock().unwrap().remove(&id);
        self.command_buffers.lock().unwrap().remove(&id);
    }
}

fn read_string_view(mem: &SharedMem, ptr: i32) -> wasmtime::Result<String> {
    let data = mem.read_i32(ptr)?;
    let len = mem.read_u32(ptr + 4)?;
    if data == 0 {
        return Ok(String::new());
    }
    if len == u32::MAX {
        mem.read_cstr(data)
    } else {
        mem.read_utf8(data, Some(len as usize))
    }
}

fn read_opt_string_view(mem: &SharedMem, ptr: i32) -> wasmtime::Result<Option<String>> {
    let data = mem.read_i32(ptr)?;
    let len = mem.read_u32(ptr + 4)?;
    if data == 0 {
        return Ok((len == 0).then(String::new));
    }
    if len == u32::MAX {
        Ok(Some(mem.read_cstr(data)?))
    } else {
        Ok(Some(mem.read_utf8(data, Some(len as usize))?))
    }
}

fn read_origin3d(mem: &SharedMem, ptr: i32) -> wasmtime::Result<wgpu::Origin3d> {
    Ok(wgpu::Origin3d {
        x: mem.read_u32(ptr)?,
        y: mem.read_u32(ptr + 4)?,
        z: mem.read_u32(ptr + 8)?,
    })
}

fn read_extent3d(mem: &SharedMem, ptr: i32) -> wasmtime::Result<wgpu::Extent3d> {
    Ok(wgpu::Extent3d {
        width: mem.read_u32(ptr)?.max(1),
        height: mem.read_u32(ptr + 4)?.max(1),
        depth_or_array_layers: mem.read_u32(ptr + 8)?.max(1),
    })
}

fn read_texel_copy_buffer_layout(
    mem: &SharedMem,
    ptr: i32,
) -> wasmtime::Result<wgpu::TexelCopyBufferLayout> {
    let offset = mem.read_u64(ptr)?;
    let bpr = mem.read_u32(ptr + 8)?;
    let rpi = mem.read_u32(ptr + 12)?;
    Ok(wgpu::TexelCopyBufferLayout {
        offset,
        bytes_per_row: (bpr != u32::MAX).then_some(bpr),
        rows_per_image: (rpi != u32::MAX).then_some(rpi),
    })
}

fn read_pipeline_constants(
    mem: &SharedMem,
    count: u32,
    ptr: i32,
) -> wasmtime::Result<Vec<(String, f64)>> {
    let mut out = Vec::with_capacity(count as usize);
    if count == 0 || ptr == 0 {
        return Ok(out);
    }
    for i in 0..count {
        let p = ptr + (i as i32 * 24);
        let key = read_string_view(mem, p + 4)?;
        let val = mem.read_f64(p + 16)?;
        out.push((key, val));
    }
    Ok(out)
}

fn read_blend_component(mem: &SharedMem, ptr: i32) -> wasmtime::Result<wgpu::BlendComponent> {
    let op = match mem.read_u32(ptr)? {
        2 => wgpu::BlendOperation::Subtract,
        3 => wgpu::BlendOperation::ReverseSubtract,
        4 => wgpu::BlendOperation::Min,
        5 => wgpu::BlendOperation::Max,
        _ => wgpu::BlendOperation::Add,
    };
    let src_factor = id_to_blend_factor(mem.read_u32(ptr + 4)?);
    let dst_factor = id_to_blend_factor(mem.read_u32(ptr + 8)?);
    Ok(wgpu::BlendComponent {
        src_factor,
        dst_factor,
        operation: op,
    })
}

fn read_stencil_face(mem: &SharedMem, ptr: i32) -> wasmtime::Result<wgpu::StencilFaceState> {
    Ok(wgpu::StencilFaceState {
        compare: id_to_compare_opt(mem.read_u32(ptr)?).unwrap_or(wgpu::CompareFunction::Always),
        fail_op: id_to_stencil_op(mem.read_u32(ptr + 4)?),
        depth_fail_op: id_to_stencil_op(mem.read_u32(ptr + 8)?),
        pass_op: id_to_stencil_op(mem.read_u32(ptr + 12)?),
    })
}

fn id_to_blend_factor(id: u32) -> wgpu::BlendFactor {
    match id {
        1 => wgpu::BlendFactor::Zero,
        2 => wgpu::BlendFactor::One,
        3 => wgpu::BlendFactor::Src,
        4 => wgpu::BlendFactor::OneMinusSrc,
        5 => wgpu::BlendFactor::SrcAlpha,
        6 => wgpu::BlendFactor::OneMinusSrcAlpha,
        7 => wgpu::BlendFactor::Dst,
        8 => wgpu::BlendFactor::OneMinusDst,
        9 => wgpu::BlendFactor::DstAlpha,
        10 => wgpu::BlendFactor::OneMinusDstAlpha,
        11 => wgpu::BlendFactor::SrcAlphaSaturated,
        12 => wgpu::BlendFactor::Constant,
        13 => wgpu::BlendFactor::OneMinusConstant,
        14 => wgpu::BlendFactor::Src1,
        15 => wgpu::BlendFactor::OneMinusSrc1,
        16 => wgpu::BlendFactor::Src1Alpha,
        17 => wgpu::BlendFactor::OneMinusSrc1Alpha,
        _ => wgpu::BlendFactor::One,
    }
}

fn id_to_stencil_op(id: u32) -> wgpu::StencilOperation {
    match id {
        2 => wgpu::StencilOperation::Zero,
        3 => wgpu::StencilOperation::Replace,
        4 => wgpu::StencilOperation::Invert,
        5 => wgpu::StencilOperation::IncrementClamp,
        6 => wgpu::StencilOperation::DecrementClamp,
        7 => wgpu::StencilOperation::IncrementWrap,
        8 => wgpu::StencilOperation::DecrementWrap,
        _ => wgpu::StencilOperation::Keep,
    }
}

fn id_to_compare_opt(id: u32) -> Option<wgpu::CompareFunction> {
    match id {
        1 => Some(wgpu::CompareFunction::Never),
        2 => Some(wgpu::CompareFunction::Less),
        3 => Some(wgpu::CompareFunction::Equal),
        4 => Some(wgpu::CompareFunction::LessEqual),
        5 => Some(wgpu::CompareFunction::Greater),
        6 => Some(wgpu::CompareFunction::NotEqual),
        7 => Some(wgpu::CompareFunction::GreaterEqual),
        8 => Some(wgpu::CompareFunction::Always),
        _ => None,
    }
}

fn id_to_address_mode(id: u32) -> wgpu::AddressMode {
    match id {
        2 => wgpu::AddressMode::Repeat,
        3 => wgpu::AddressMode::MirrorRepeat,
        _ => wgpu::AddressMode::ClampToEdge,
    }
}

fn id_to_filter_mode(id: u32) -> wgpu::FilterMode {
    match id {
        2 => wgpu::FilterMode::Linear,
        _ => wgpu::FilterMode::Nearest,
    }
}

fn id_to_mipmap_filter(id: u32) -> wgpu::MipmapFilterMode {
    match id {
        2 => wgpu::MipmapFilterMode::Linear,
        _ => wgpu::MipmapFilterMode::Nearest,
    }
}

fn id_to_aspect(id: u32) -> wgpu::TextureAspect {
    match id {
        2 => wgpu::TextureAspect::StencilOnly,
        3 => wgpu::TextureAspect::DepthOnly,
        _ => wgpu::TextureAspect::All,
    }
}

fn id_to_topology(id: u32) -> wgpu::PrimitiveTopology {
    match id {
        1 => wgpu::PrimitiveTopology::PointList,
        2 => wgpu::PrimitiveTopology::LineList,
        3 => wgpu::PrimitiveTopology::LineStrip,
        5 => wgpu::PrimitiveTopology::TriangleStrip,
        _ => wgpu::PrimitiveTopology::TriangleList,
    }
}

fn id_to_index_format_opt(id: u32) -> Option<wgpu::IndexFormat> {
    match id {
        1 => Some(wgpu::IndexFormat::Uint16),
        2 => Some(wgpu::IndexFormat::Uint32),
        _ => None,
    }
}

fn id_to_texture_view_dim(id: u32) -> Option<wgpu::TextureViewDimension> {
    match id {
        1 => Some(wgpu::TextureViewDimension::D1),
        2 => Some(wgpu::TextureViewDimension::D2),
        3 => Some(wgpu::TextureViewDimension::D2Array),
        4 => Some(wgpu::TextureViewDimension::Cube),
        5 => Some(wgpu::TextureViewDimension::CubeArray),
        6 => Some(wgpu::TextureViewDimension::D3),
        _ => None,
    }
}

fn id_to_vertex_format(id: u32) -> Option<wgpu::VertexFormat> {
    use wgpu::VertexFormat::*;
    Some(match id {
        1 => Uint8,
        2 => Uint8x2,
        3 => Uint8x4,
        4 => Sint8,
        5 => Sint8x2,
        6 => Sint8x4,
        7 => Unorm8,
        8 => Unorm8x2,
        9 => Unorm8x4,
        10 => Snorm8,
        11 => Snorm8x2,
        12 => Snorm8x4,
        13 => Uint16,
        14 => Uint16x2,
        15 => Uint16x4,
        16 => Sint16,
        17 => Sint16x2,
        18 => Sint16x4,
        19 => Unorm16,
        20 => Unorm16x2,
        21 => Unorm16x4,
        22 => Snorm16,
        23 => Snorm16x2,
        24 => Snorm16x4,
        25 => Float16,
        26 => Float16x2,
        27 => Float16x4,
        28 => Float32,
        29 => Float32x2,
        30 => Float32x3,
        31 => Float32x4,
        32 => Uint32,
        33 => Uint32x2,
        34 => Uint32x3,
        35 => Uint32x4,
        36 => Sint32,
        37 => Sint32x2,
        38 => Sint32x3,
        39 => Sint32x4,
        40 => Unorm10_10_10_2,
        41 => Unorm8x4Bgra,
        _ => return None,
    })
}

fn texture_format_to_id(fmt: wgpu::TextureFormat) -> Option<i32> {
    use wgpu::TextureFormat::*;
    Some(match fmt {
        Bgra8Unorm => 27,
        Bgra8UnormSrgb => 28,
        Rgba8Unorm => 22,
        Rgba8UnormSrgb => 23,
        _ => return None,
    })
}

fn id_to_texture_format(id: u32) -> Option<wgpu::TextureFormat> {
    use wgpu::AstcBlock::*;
    use wgpu::AstcChannel::*;
    use wgpu::TextureFormat::*;
    Some(match id {
        1 => R8Unorm,
        2 => R8Snorm,
        3 => R8Uint,
        4 => R8Sint,
        5 => R16Unorm,
        6 => R16Snorm,
        7 => R16Uint,
        8 => R16Sint,
        9 => R16Float,
        10 => Rg8Unorm,
        11 => Rg8Snorm,
        12 => Rg8Uint,
        13 => Rg8Sint,
        14 => R32Float,
        15 => R32Uint,
        16 => R32Sint,
        17 => Rg16Unorm,
        18 => Rg16Snorm,
        19 => Rg16Uint,
        20 => Rg16Sint,
        21 => Rg16Float,
        22 => Rgba8Unorm,
        23 => Rgba8UnormSrgb,
        24 => Rgba8Snorm,
        25 => Rgba8Uint,
        26 => Rgba8Sint,
        27 => Bgra8Unorm,
        28 => Bgra8UnormSrgb,
        29 => Rgb10a2Uint,
        30 => Rgb10a2Unorm,
        31 => Rg11b10Ufloat,
        32 => Rgb9e5Ufloat,
        33 => Rg32Float,
        34 => Rg32Uint,
        35 => Rg32Sint,
        36 => Rgba16Unorm,
        37 => Rgba16Snorm,
        38 => Rgba16Uint,
        39 => Rgba16Sint,
        40 => Rgba16Float,
        41 => Rgba32Float,
        42 => Rgba32Uint,
        43 => Rgba32Sint,
        44 => Stencil8,
        45 => Depth16Unorm,
        46 => Depth24Plus,
        47 => Depth24PlusStencil8,
        48 => Depth32Float,
        49 => Depth32FloatStencil8,
        50 => Bc1RgbaUnorm,
        51 => Bc1RgbaUnormSrgb,
        52 => Bc2RgbaUnorm,
        53 => Bc2RgbaUnormSrgb,
        54 => Bc3RgbaUnorm,
        55 => Bc3RgbaUnormSrgb,
        56 => Bc4RUnorm,
        57 => Bc4RSnorm,
        58 => Bc5RgUnorm,
        59 => Bc5RgSnorm,
        60 => Bc6hRgbUfloat,
        61 => Bc6hRgbFloat,
        62 => Bc7RgbaUnorm,
        63 => Bc7RgbaUnormSrgb,
        64 => Etc2Rgb8Unorm,
        65 => Etc2Rgb8UnormSrgb,
        66 => Etc2Rgb8A1Unorm,
        67 => Etc2Rgb8A1UnormSrgb,
        68 => Etc2Rgba8Unorm,
        69 => Etc2Rgba8UnormSrgb,
        70 => EacR11Unorm,
        71 => EacR11Snorm,
        72 => EacRg11Unorm,
        73 => EacRg11Snorm,
        74 => Astc { block: B4x4, channel: Unorm },
        75 => Astc { block: B4x4, channel: UnormSrgb },
        76 => Astc { block: B5x4, channel: Unorm },
        77 => Astc { block: B5x4, channel: UnormSrgb },
        78 => Astc { block: B5x5, channel: Unorm },
        79 => Astc { block: B5x5, channel: UnormSrgb },
        80 => Astc { block: B6x5, channel: Unorm },
        81 => Astc { block: B6x5, channel: UnormSrgb },
        82 => Astc { block: B6x6, channel: Unorm },
        83 => Astc { block: B6x6, channel: UnormSrgb },
        84 => Astc { block: B8x5, channel: Unorm },
        85 => Astc { block: B8x5, channel: UnormSrgb },
        86 => Astc { block: B8x6, channel: Unorm },
        87 => Astc { block: B8x6, channel: UnormSrgb },
        88 => Astc { block: B8x8, channel: Unorm },
        89 => Astc { block: B8x8, channel: UnormSrgb },
        90 => Astc { block: B10x5, channel: Unorm },
        91 => Astc { block: B10x5, channel: UnormSrgb },
        92 => Astc { block: B10x6, channel: Unorm },
        93 => Astc { block: B10x6, channel: UnormSrgb },
        94 => Astc { block: B10x8, channel: Unorm },
        95 => Astc { block: B10x8, channel: UnormSrgb },
        96 => Astc { block: B10x10, channel: Unorm },
        97 => Astc { block: B10x10, channel: UnormSrgb },
        98 => Astc { block: B12x10, channel: Unorm },
        99 => Astc { block: B12x10, channel: UnormSrgb },
        100 => Astc { block: B12x12, channel: Unorm },
        101 => Astc { block: B12x12, channel: UnormSrgb },
        _ => return None,
    })
}
