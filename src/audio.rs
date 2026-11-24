use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Sample, SampleFormat, SizedSample, StreamConfig};

use crate::memory::SharedMem;
use crate::runtime::{MainThreadTask, RuntimeState};

/// Нативный интерфейс аудио-бэкенда для FMOD в Roblox WebClient (`WebClient.wasm`).
///
/// Поддерживает оба пути вывода FMOD:
/// 1. **AudioWorklet Fastpath (`audio-processor-fast.js` + `SharedArrayBuffer`)** — приоритетный
///    режим без блокировки главного потока: чтение двойного планарного буфера `f32` из общей памяти
///    WASM (`dataOffset`) и пробуждение микшер-потока FMOD через атомарную запись слота `1`/`2` и
///    `atomic_notify` по `syncOffset`.
/// 2. **ScriptProcessor / AudioWorklet Slowpath fallback** — генерация сэмплов вызовом экспортов
///    `FMOD_JS_MixFunction` / `FMOD_JS_MixerSlowpathFunction` на главном потоке с буферизацией в
///    потокобезопасную очередь.
#[allow(unused_variables, dead_code)]
pub trait AudioBackend: Send + Sync + 'static {
    fn sample_rate(&self) -> i32 {
        48_000
    }
    fn max_channels(&self) -> i32 {
        2
    }
    fn worklet_supported(&self) -> bool {
        false
    }

    // --- FMOD AudioWorklet API ---
    fn worklet_open_context(&self) -> i32 {
        self.max_channels()
    }
    fn worklet_get_sample_rate(&self, is_reinit: bool) -> i32 {
        if self.worklet_supported() {
            self.sample_rate()
        } else {
            -1
        }
    }
    fn worklet_prepare(&self, speaker_channels: i32, buffer_size: i32) -> i32 {
        -1
    }
    fn worklet_set_fastpath_payload(
        &self,
        data_offset: i32,
        sync_offset: i32,
        channel_count: i32,
        buffer_size: i32,
    ) {
    }
    fn worklet_create_node(&self) {}
    fn worklet_connect(&self) {}
    fn worklet_disconnect(&self) {}
    fn worklet_resume_and_check_node(&self) -> i32 {
        0
    }
    fn worklet_suspend(&self) {}
    fn worklet_close(&self) {}
    fn worklet_write_slowpath(&self, mem: &SharedMem, src_ptr: i32, channels: i32, frames: i32) {}
    fn worklet_write_slowpath_slice(
        &self,
        mem: &SharedMem,
        src_ptr: i32,
        channels: i32,
        frames: i32,
        slice_idx: i32,
    ) {
    }

    // --- FMOD ScriptProcessor fallback API ---
    fn script_open_context(&self) -> i32 {
        0
    }
    fn script_create_processor(&self, channels: i32, buffer_size: i32) {}
    fn script_connect(&self) {}
    fn script_disconnect(&self) {}
    fn script_suspend(&self) {}
    fn script_resume(&self) {}
    fn script_write_interleaved(
        &self,
        mem: &SharedMem,
        src_ptr: i32,
        dst_frame_offset: i32,
        frames: i32,
        channels: i32,
    ) {
    }
    fn script_commit_mix_block(&self) {}

    // --- Управление громкостью и mute со стороны хоста ---
    fn set_volume(&self, volume: f32) {}
    fn volume(&self) -> f32 {
        1.0
    }
    fn set_muted(&self, muted: bool) {}
    fn is_muted(&self) -> bool {
        false
    }
}

pub struct NullAudioBackend;
impl AudioBackend for NullAudioBackend {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioMixMode {
    ScriptProcessor { frames: i32 },
    WorkletSlowpath,
}

#[derive(Debug, Clone, Copy)]
struct FastpathState {
    ready: bool,
    data_offset: i32,
    sync_offset: i32,
    channel_count: usize,
    buffer_size: usize,
    half_floats: usize,
    buffer_flag: usize,
    frame_cursor: usize,
}

impl Default for FastpathState {
    fn default() -> Self {
        Self {
            ready: false,
            data_offset: 0,
            sync_offset: 0,
            channel_count: 2,
            buffer_size: 512,
            half_floats: 1024,
            buffer_flag: 0,
            frame_cursor: 0,
        }
    }
}

struct FallbackMixBuffer {
    channels: usize,
    buffer_size: usize,
    scratch_interleaved: Vec<f32>,
    fifo_interleaved: VecDeque<f32>,
}

impl Default for FallbackMixBuffer {
    fn default() -> Self {
        Self {
            channels: 2,
            buffer_size: 1024,
            scratch_interleaved: Vec::new(),
            fifo_interleaved: VecDeque::with_capacity(48_000 * 2),
        }
    }
}

enum StreamCmd {
    Play,
    Pause,
    Shutdown,
}

#[allow(dead_code)]
pub struct CpalAudioBackend {
    mem: SharedMem,
    runtime_ref: Mutex<Option<Weak<RuntimeState>>>,

    device_sample_rate: i32,
    device_channels: usize,
    fmod_max_channels: i32,
    worklet_enabled: bool,

    fastpath: Arc<Mutex<FastpathState>>,
    fallback: Arc<Mutex<FallbackMixBuffer>>,

    worklet_node_created: AtomicBool,
    worklet_connected: Arc<AtomicBool>,
    script_connected: Arc<AtomicBool>,
    suspended: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    volume_bits: Arc<AtomicU32>,

    script_buffer_size: AtomicI32,
    script_channels: AtomicI32,

    cmd_tx: Mutex<Option<mpsc::Sender<StreamCmd>>>,
    stream_thread: Mutex<Option<JoinHandle<()>>>,
    pump_wakeup: Arc<(Mutex<bool>, Condvar)>,
    pump_stop: Arc<AtomicBool>,
    pump_thread: Mutex<Option<JoinHandle<()>>>,
}

impl CpalAudioBackend {
    pub fn try_new(
        mem: SharedMem,
        initial_volume: f32,
        muted: bool,
        prefer_worklet: bool,
    ) -> Result<Arc<Self>, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default audio output device found".to_owned())?;
        let device_desc = device
            .description()
            .map(|d| d.name().to_owned())
            .unwrap_or_else(|_| "Default Output".to_owned());

        let (stream_cfg, sample_format) = choose_output_config(&device)?;
        let device_sample_rate = stream_cfg.sample_rate as i32;
        let device_channels = stream_cfg.channels.max(1) as usize;
        let fmod_max_channels = normalize_fmod_channels(device_channels);

        let fastpath = Arc::new(Mutex::new(FastpathState::default()));
        let fallback = Arc::new(Mutex::new(FallbackMixBuffer::default()));
        let worklet_connected = Arc::new(AtomicBool::new(false));
        let script_connected = Arc::new(AtomicBool::new(false));
        let suspended = Arc::new(AtomicBool::new(false));
        let muted_flag = Arc::new(AtomicBool::new(muted));
        let vol_clamped = initial_volume.clamp(0.0, 2.0);
        let volume_bits = Arc::new(AtomicU32::new(vol_clamped.to_bits()));

        let (cmd_tx, cmd_rx) = mpsc::channel::<StreamCmd>();
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(1);

        let mem_cb = mem.clone();
        let fp_cb = fastpath.clone();
        let fb_cb = fallback.clone();
        let wc_cb = worklet_connected.clone();
        let sc_cb = script_connected.clone();
        let susp_cb = suspended.clone();
        let mute_cb = muted_flag.clone();
        let vol_cb = volume_bits.clone();

        let stream_thread = thread::Builder::new()
            .name("weld-audio-device".into())
            .spawn(move || {
                let ctx = CallbackCtx {
                    mem: mem_cb,
                    dev_channels: device_channels,
                    fastpath: fp_cb,
                    fallback: fb_cb,
                    worklet_connected: wc_cb,
                    script_connected: sc_cb,
                    suspended: susp_cb,
                    muted: mute_cb,
                    volume_bits: vol_cb,
                };
                let build_res = build_typed_stream(&device, &stream_cfg, sample_format, ctx);
                let stream = match build_res {
                    Ok(s) => {
                        let _ = s.play();
                        let _ = init_tx.send(Ok(()));
                        s
                    }
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                };

                while let Ok(cmd) = cmd_rx.recv() {
                    match cmd {
                        StreamCmd::Play => {
                            let _ = stream.play();
                        }
                        StreamCmd::Pause => {
                            let _ = stream.pause();
                        }
                        StreamCmd::Shutdown => {
                            let _ = stream.pause();
                            break;
                        }
                    }
                }
            })
            .map_err(|e| format!("failed to spawn audio device thread: {e}"))?;

        match init_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = stream_thread.join();
                return Err(e);
            }
            Err(_) => {
                return Err("timed out initializing cpal output stream".into());
            }
        }

        println!(
            "[Audio] Initialized output {device_desc:?}: {} Hz, {} ch (FMOD {} ch, fmt={sample_format:?}, worklet={prefer_worklet})",
            device_sample_rate, device_channels, fmod_max_channels
        );

        let backend = Arc::new(Self {
            mem,
            runtime_ref: Mutex::new(None),
            device_sample_rate,
            device_channels,
            fmod_max_channels,
            worklet_enabled: prefer_worklet,
            fastpath,
            fallback,
            worklet_node_created: AtomicBool::new(false),
            worklet_connected,
            script_connected,
            suspended,
            muted: muted_flag,
            volume_bits,
            script_buffer_size: AtomicI32::new(1024),
            script_channels: AtomicI32::new(fmod_max_channels),
            cmd_tx: Mutex::new(Some(cmd_tx)),
            stream_thread: Mutex::new(Some(stream_thread)),
            pump_wakeup: Arc::new((Mutex::new(false), Condvar::new())),
            pump_stop: Arc::new(AtomicBool::new(false)),
            pump_thread: Mutex::new(None),
        });

        Self::spawn_fallback_pump(&backend);
        Ok(backend)
    }

    pub fn attach_runtime(&self, rt: Weak<RuntimeState>) {
        *self.runtime_ref.lock().unwrap() = Some(rt);
    }

    fn send_stream_cmd(&self, cmd: StreamCmd) {
        if let Some(ref tx) = *self.cmd_tx.lock().unwrap() {
            let _ = tx.send(cmd);
        }
    }

    fn notify_pump(&self) {
        let (lock, cvar) = &*self.pump_wakeup;
        if let Ok(mut woken) = lock.lock() {
            *woken = true;
            cvar.notify_all();
        }
    }

    /// Фоновый таймер для ScriptProcessor / AudioWorklet Slowpath (в режиме Fastpath спит на Condvar).
    fn spawn_fallback_pump(this: &Arc<Self>) {
        let weak_self = Arc::downgrade(this);
        let wakeup = this.pump_wakeup.clone();
        let stop = this.pump_stop.clone();

        let handle = thread::Builder::new()
            .name("weld-audio-fallback-pump".into())
            .spawn(move || {
                let mut next_deadline = Instant::now();
                while !stop.load(Ordering::SeqCst) {
                    let Some(audio) = weak_self.upgrade() else {
                        break;
                    };

                    let is_susp = audio.suspended.load(Ordering::Relaxed);
                    let sc = audio.script_connected.load(Ordering::Acquire);
                    let wc = audio.worklet_connected.load(Ordering::Acquire);
                    let fp_ready = audio.fastpath.lock().unwrap().ready;

                    let active_fallback = if is_susp {
                        None
                    } else if sc {
                        let frames = audio.script_buffer_size.load(Ordering::Relaxed).max(128);
                        Some((AudioMixMode::ScriptProcessor { frames }, frames as usize))
                    } else if wc && !fp_ready {
                        let frames = audio.fastpath.lock().unwrap().buffer_size.max(128);
                        Some((AudioMixMode::WorkletSlowpath, frames))
                    } else {
                        None
                    };

                    if let Some((mode, block_frames)) = active_fallback {
                        let target_queued_frames = block_frames * 4;
                        let need_more = {
                            let fb = audio.fallback.lock().unwrap();
                            let ch = fb.channels.max(1);
                            (fb.fifo_interleaved.len() / ch) < target_queued_frames
                        };
                        if need_more {
                            if let Some(weak_rt) = audio.runtime_ref.lock().unwrap().as_ref() {
                                if let Some(rt) = weak_rt.upgrade() {
                                    rt.push_main_task(MainThreadTask::AudioMix(mode));
                                }
                            }
                        }

                        let sr = audio.device_sample_rate.max(8_000) as f64;
                        let step_secs = ((block_frames as f64) / sr * 0.5).clamp(0.002, 0.020);
                        next_deadline += Duration::from_secs_f64(step_secs);
                        let now = Instant::now();
                        if next_deadline > now {
                            let (lock, cvar) = &*wakeup;
                            if let Ok(g) = lock.lock() {
                                let _ = cvar.wait_timeout(g, next_deadline - now);
                            }
                        } else {
                            next_deadline = now;
                        }
                    } else {
                        let (lock, cvar) = &*wakeup;
                        if let Ok(mut g) = lock.lock() {
                            if !*g && !stop.load(Ordering::SeqCst) {
                                if let Ok((mut g2, _)) =
                                    cvar.wait_timeout(g, Duration::from_millis(100))
                                {
                                    *g2 = false;
                                }
                            } else {
                                *g = false;
                            }
                        }
                        next_deadline = Instant::now();
                    }
                }
            })
            .ok();

        *this.pump_thread.lock().unwrap() = handle;
    }
}

impl Drop for CpalAudioBackend {
    fn drop(&mut self) {
        self.pump_stop.store(true, Ordering::SeqCst);
        self.notify_pump();
        self.send_stream_cmd(StreamCmd::Shutdown);
        let _ = self.cmd_tx.lock().unwrap().take();
        if let Some(h) = self.pump_thread.lock().unwrap().take() {
            let _ = h.join();
        }
        if let Some(h) = self.stream_thread.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

impl AudioBackend for CpalAudioBackend {
    fn sample_rate(&self) -> i32 {
        self.device_sample_rate
    }

    fn max_channels(&self) -> i32 {
        self.fmod_max_channels
    }

    fn worklet_supported(&self) -> bool {
        self.worklet_enabled
    }

    fn worklet_open_context(&self) -> i32 {
        self.worklet_node_created.store(false, Ordering::SeqCst);
        self.worklet_connected.store(false, Ordering::SeqCst);
        self.suspended.store(false, Ordering::SeqCst);
        {
            let mut fp = self.fastpath.lock().unwrap();
            fp.ready = false;
            fp.frame_cursor = 0;
            fp.buffer_flag = 0;
            fp.channel_count = self.fmod_max_channels.max(1) as usize;
        }
        self.send_stream_cmd(StreamCmd::Play);
        self.fmod_max_channels
    }

    fn worklet_get_sample_rate(&self, _is_reinit: bool) -> i32 {
        if self.worklet_enabled {
            self.device_sample_rate
        } else {
            -1
        }
    }

    fn worklet_prepare(&self, speaker_channels: i32, buffer_size: i32) -> i32 {
        if !self.worklet_enabled {
            return -1;
        }
        let ch = speaker_channels.max(1) as usize;
        let bs = buffer_size.max(128) as usize;
        {
            let mut fp = self.fastpath.lock().unwrap();
            fp.ready = false;
            fp.channel_count = ch;
            fp.buffer_size = bs;
            fp.half_floats = ch * bs;
            fp.buffer_flag = 0;
            fp.frame_cursor = 0;
        }
        {
            let mut fb = self.fallback.lock().unwrap();
            fb.channels = ch;
            fb.buffer_size = bs;
            fb.fifo_interleaved.clear();
        }
        println!("[Audio] FMOD AudioWorklet prepared (channels={ch}, dsp_buffer={bs} frames)");
        // 1 = SharedArrayBuffer fastpath supported (`mSharedArrayBuffers = true`)
        1
    }

    fn worklet_set_fastpath_payload(
        &self,
        data_offset: i32,
        sync_offset: i32,
        channel_count: i32,
        buffer_size: i32,
    ) {
        let ch = channel_count.max(1) as usize;
        let bs = buffer_size.max(1) as usize;
        let mut fp = self.fastpath.lock().unwrap();
        fp.data_offset = data_offset;
        fp.sync_offset = sync_offset;
        fp.channel_count = ch;
        fp.buffer_size = bs;
        fp.half_floats = ch * bs;
        fp.buffer_flag = 0;
        fp.frame_cursor = 0;
        fp.ready = data_offset != 0 && sync_offset != 0;
        println!(
            "[Audio] Fastpath attached: data={data_offset:#x}, sync={sync_offset:#x}, ch={ch}, frames={bs}"
        );
    }

    fn worklet_create_node(&self) {
        self.suspended.store(false, Ordering::SeqCst);
        self.worklet_node_created.store(true, Ordering::SeqCst);
        // В браузере `waitForAudioWorklet` ждёт появления `mWorkletNode` и сразу подключает его к destination.
        self.worklet_connected.store(true, Ordering::SeqCst);
        self.send_stream_cmd(StreamCmd::Play);

        // Предварительно будим микшер FMOD для заполнения половины 0, чтобы избежать пустого первого блока.
        let sync_ptr = {
            let fp = self.fastpath.lock().unwrap();
            if fp.ready { fp.sync_offset } else { 0 }
        };
        if sync_ptr != 0 {
            let _ = self.mem.atomic_store_i32(sync_ptr, 1);
            let _ = self.mem.atomic_notify(sync_ptr, 1);
        }
        self.notify_pump();
    }

    fn worklet_connect(&self) {
        self.worklet_connected.store(true, Ordering::SeqCst);
        self.suspended.store(false, Ordering::SeqCst);
        self.send_stream_cmd(StreamCmd::Play);
        self.notify_pump();
    }

    fn worklet_disconnect(&self) {
        self.worklet_connected.store(false, Ordering::SeqCst);
    }

    fn worklet_resume_and_check_node(&self) -> i32 {
        self.suspended.store(false, Ordering::SeqCst);
        self.send_stream_cmd(StreamCmd::Play);
        if self.worklet_node_created.load(Ordering::SeqCst) {
            self.worklet_connected.store(true, Ordering::SeqCst);
            self.notify_pump();
            1
        } else {
            0
        }
    }

    fn worklet_suspend(&self) {
        self.suspended.store(true, Ordering::SeqCst);
        self.send_stream_cmd(StreamCmd::Pause);
    }

    fn worklet_close(&self) {
        self.worklet_connected.store(false, Ordering::SeqCst);
        self.worklet_node_created.store(false, Ordering::SeqCst);
        let mut fp = self.fastpath.lock().unwrap();
        fp.ready = false;
        fp.data_offset = 0;
        fp.sync_offset = 0;
    }

    fn worklet_write_slowpath(&self, mem: &SharedMem, src_ptr: i32, channels: i32, frames: i32) {
        let ch = channels.max(1) as usize;
        let fr = frames.max(0) as usize;
        if fr == 0 || src_ptr == 0 {
            return;
        }
        // В EM_ASM 9115094 входной буфер `o` в памяти WASM имеет планарную раскладку `[ch][frame]`,
        // а `mOutputData` — интерлив `[frame * ch + c]`.
        let Ok(raw) = mem.read_bytes(src_ptr, ch * fr * 4) else {
            return;
        };
        let mut fb = self.fallback.lock().unwrap();
        fb.channels = ch;
        let max_samples = ch * self.device_sample_rate.max(8_000) as usize;
        for f in 0..fr {
            for c in 0..ch {
                let idx = (c * fr + f) * 4;
                let sample = f32::from_le_bytes([raw[idx], raw[idx + 1], raw[idx + 2], raw[idx + 3]]);
                if fb.fifo_interleaved.len() < max_samples {
                    fb.fifo_interleaved.push_back(sample);
                }
            }
        }
    }

    fn worklet_write_slowpath_slice(
        &self,
        mem: &SharedMem,
        src_ptr: i32,
        channels: i32,
        frames: i32,
        _slice_idx: i32,
    ) {
        self.worklet_write_slowpath(mem, src_ptr, channels, frames);
    }

    fn script_open_context(&self) -> i32 {
        self.suspended.store(false, Ordering::SeqCst);
        self.send_stream_cmd(StreamCmd::Play);
        self.device_sample_rate
    }

    fn script_create_processor(&self, channels: i32, buffer_size: i32) {
        let ch = channels.max(1);
        let bs = if buffer_size > 0 { buffer_size } else { 1024 };
        self.script_channels.store(ch, Ordering::SeqCst);
        self.script_buffer_size.store(bs, Ordering::SeqCst);
        let mut fb = self.fallback.lock().unwrap();
        fb.channels = ch as usize;
        fb.buffer_size = bs as usize;
        fb.scratch_interleaved
            .resize((ch as usize) * (bs as usize), 0.0);
        fb.fifo_interleaved.clear();
        println!("[Audio] FMOD ScriptProcessor created (channels={ch}, buffer={bs} frames)");
    }

    fn script_connect(&self) {
        self.script_connected.store(true, Ordering::SeqCst);
        self.suspended.store(false, Ordering::SeqCst);
        self.send_stream_cmd(StreamCmd::Play);
        self.notify_pump();
    }

    fn script_disconnect(&self) {
        self.script_connected.store(false, Ordering::SeqCst);
    }

    fn script_suspend(&self) {
        self.suspended.store(true, Ordering::SeqCst);
        self.send_stream_cmd(StreamCmd::Pause);
    }

    fn script_resume(&self) {
        self.suspended.store(false, Ordering::SeqCst);
        self.send_stream_cmd(StreamCmd::Play);
        self.notify_pump();
    }

    fn script_write_interleaved(
        &self,
        mem: &SharedMem,
        src_ptr: i32,
        dst_frame_offset: i32,
        frames: i32,
        channels: i32,
    ) {
        let ch = channels.max(1) as usize;
        let fr = frames.max(0) as usize;
        let dst_off = dst_frame_offset.max(0) as usize;
        if fr == 0 || src_ptr == 0 {
            return;
        }
        // В EM_ASM 9117920 входной массив `a` уже интерлив (`a[s * channels + i]`).
        let Ok(raw) = mem.read_bytes(src_ptr, fr * ch * 4) else {
            return;
        };
        let mut fb = self.fallback.lock().unwrap();
        fb.channels = ch;
        let needed = (dst_off + fr) * ch;
        if fb.scratch_interleaved.len() < needed {
            fb.scratch_interleaved.resize(needed, 0.0);
        }
        let base = dst_off * ch;
        for idx in 0..(fr * ch) {
            let b = idx * 4;
            fb.scratch_interleaved[base + idx] =
                f32::from_le_bytes([raw[b], raw[b + 1], raw[b + 2], raw[b + 3]]);
        }
    }

    fn script_commit_mix_block(&self) {
        let frames = self.script_buffer_size.load(Ordering::Relaxed).max(0) as usize;
        let mut fb = self.fallback.lock().unwrap();
        let count = frames * fb.channels;
        if count == 0 || fb.scratch_interleaved.len() < count {
            return;
        }
        let max_samples = fb.channels * self.device_sample_rate.max(8_000) as usize;
        for idx in 0..count {
            if fb.fifo_interleaved.len() >= max_samples {
                break;
            }
            let sample = fb.scratch_interleaved[idx];
            fb.fifo_interleaved.push_back(sample);
        }
    }

    fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 2.0);
        self.volume_bits.store(clamped.to_bits(), Ordering::Relaxed);
    }

    fn volume(&self) -> f32 {
        f32::from_bits(self.volume_bits.load(Ordering::Relaxed))
    }

    fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
}

struct CallbackCtx {
    mem: SharedMem,
    dev_channels: usize,
    fastpath: Arc<Mutex<FastpathState>>,
    fallback: Arc<Mutex<FallbackMixBuffer>>,
    worklet_connected: Arc<AtomicBool>,
    script_connected: Arc<AtomicBool>,
    suspended: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    volume_bits: Arc<AtomicU32>,
}

impl CallbackCtx {
    fn render_interleaved_f32(&self, out: &mut [f32]) {
        out.fill(0.0);
        let dev_ch = self.dev_channels.max(1);
        let total_frames = out.len() / dev_ch;
        if total_frames == 0 || self.suspended.load(Ordering::Relaxed) {
            return;
        }

        let gain = if self.muted.load(Ordering::Relaxed) {
            0.0
        } else {
            f32::from_bits(self.volume_bits.load(Ordering::Relaxed))
        };

        if self.worklet_connected.load(Ordering::Acquire) && self.try_render_fastpath(out, dev_ch, total_frames, gain) {
            return;
        }

        if self.script_connected.load(Ordering::Acquire) || self.worklet_connected.load(Ordering::Acquire) {
            self.render_from_fallback_fifo(out, dev_ch, total_frames, gain);
        }
    }

    /// Точная реализация `AudioProcessor.process()` из `audio-processor-fast.js`:
    /// - В начале каждого блока из `buffer_size` сэмплов атомарно записывает `(bufferFlag ^ 1) + 1`
    ///   в `syncOffset` и будит поток FMOD через `atomic_notify(syncOffset, 1)`, чтобы FMOD
    ///   параллельно рендерил следующую половину, пока воспроизводится текущая.
    /// - Планарные каналы каждой половины имеют вид:
    ///   `half_base = data_offset + buffer_flag * half_floats * 4`,
    ///   `channel_base = half_base + ch * buffer_size * 4`.
    fn try_render_fastpath(
        &self,
        out: &mut [f32],
        dev_ch: usize,
        total_frames: usize,
        gain: f32,
    ) -> bool {
        let Ok(mut fp) = self.fastpath.try_lock() else {
            return true;
        };
        if !fp.ready || fp.buffer_size == 0 || fp.channel_count == 0 {
            return false;
        }

        let src_ch = fp.channel_count;
        let buf_sz = fp.buffer_size;
        let half_floats = fp.half_floats;
        let data_offset = fp.data_offset;
        let sync_offset = fp.sync_offset;

        // Проверяем границы WASM-памяти на весь двойной буфер.
        let required_bytes = half_floats.saturating_mul(2).saturating_mul(4);
        let start_addr = data_offset as u32 as usize;
        if start_addr.saturating_add(required_bytes) > self.mem.byte_len() {
            return true;
        }

        let mut written_frames = 0usize;
        while written_frames < total_frames {
            if fp.frame_cursor == 0 {
                // Slot encoding из audio-processor-fast.js:
                // 1 = заполнить половину 0, 2 = заполнить половину 1, 0 = ожидание (futex_wait).
                let next_slot = ((fp.buffer_flag ^ 1) + 1) as i32;
                let _ = self.mem.atomic_store_i32(sync_offset, next_slot);
                let _ = self.mem.atomic_notify(sync_offset, 1);
            }

            let avail = buf_sz - fp.frame_cursor;
            let chunk = (total_frames - written_frames).min(avail);
            let half_float_base = fp.buffer_flag * half_floats;

            if gain != 0.0 {
                for ch_out in 0..dev_ch {
                    let ch_src = map_output_channel(ch_out, dev_ch, src_ch);
                    let Some(ch_idx) = ch_src else {
                        continue;
                    };
                    let float_offset = half_float_base + ch_idx * buf_sz + fp.frame_cursor;
                    let byte_ptr = data_offset.wrapping_add((float_offset * 4) as i32);
                    if let Ok(bytes) = self.mem.read_bytes(byte_ptr, chunk * 4) {
                        for f in 0..chunk {
                            let b = f * 4;
                            let s = f32::from_le_bytes([
                                bytes[b],
                                bytes[b + 1],
                                bytes[b + 2],
                                bytes[b + 3],
                            ]);
                            let sample = if s.is_finite() {
                                (s * gain).clamp(-1.0, 1.0)
                            } else {
                                0.0
                            };
                            out[(written_frames + f) * dev_ch + ch_out] = sample;
                        }
                    }
                }
            }

            fp.frame_cursor += chunk;
            written_frames += chunk;
            if fp.frame_cursor >= buf_sz {
                fp.frame_cursor = 0;
                fp.buffer_flag ^= 1;
            }
        }

        true
    }

    fn render_from_fallback_fifo(
        &self,
        out: &mut [f32],
        dev_ch: usize,
        total_frames: usize,
        gain: f32,
    ) {
        let Ok(mut fb) = self.fallback.try_lock() else {
            return;
        };
        let src_ch = fb.channels.max(1);
        let mut frame_buf = [0.0f32; 8];

        for f in 0..total_frames {
            if fb.fifo_interleaved.len() < src_ch {
                break;
            }
            for item in frame_buf.iter_mut().take(src_ch.min(8)) {
                *item = fb.fifo_interleaved.pop_front().unwrap_or(0.0);
            }
            for _ in 8..src_ch {
                let _ = fb.fifo_interleaved.pop_front();
            }
            if gain == 0.0 {
                continue;
            }
            for ch_out in 0..dev_ch {
                if let Some(ch_src) = map_output_channel(ch_out, dev_ch, src_ch.min(8)) {
                    let s = frame_buf[ch_src];
                    out[f * dev_ch + ch_out] = if s.is_finite() {
                        (s * gain).clamp(-1.0, 1.0)
                    } else {
                        0.0
                    };
                }
            }
        }
    }
}

/// Сопоставляет выходной канал устройства с каналом FMOD.
#[inline]
fn map_output_channel(out_ch: usize, dev_channels: usize, src_channels: usize) -> Option<usize> {
    if src_channels == 0 {
        return None;
    }
    if out_ch < src_channels {
        Some(out_ch)
    } else if src_channels == 1 && (dev_channels == 1 || out_ch < 2) {
        // Моно-сигнал дублируем в L и R
        Some(0)
    } else {
        None
    }
}

/// FMOD в `func[116533]` поддерживает маппинг динамиков для 1, 2, 4, 5, 6, 8, 12 каналов
/// (иначе откатывается на MONO). Для стандартного десктопа ограничиваемся 1/2/6/8.
pub fn normalize_fmod_channels(device_channels: usize) -> i32 {
    match device_channels {
        0 => 2,
        1 => 1,
        2 | 3 => 2,
        4 | 5 => 4,
        6 | 7 => 6,
        _ => 8,
    }
}

fn choose_output_config(device: &cpal::Device) -> Result<(StreamConfig, SampleFormat), String> {
    let default_cfg = device
        .default_output_config()
        .map_err(|e| format!("failed to query default output config: {e}"))?;

    let sample_format = default_cfg.sample_format();
    let mut cfg: StreamConfig = default_cfg.into();
    // Просим низколатентный буфер, если устройство поддерживает диапазон, иначе оставляем Default
    if let Ok(configs) = device.supported_output_configs() {
        for range in configs {
            if range.channels() == cfg.channels
                && range.sample_format() == sample_format
                && range.min_sample_rate() <= cfg.sample_rate
                && cfg.sample_rate <= range.max_sample_rate()
            {
                if let cpal::SupportedBufferSize::Range { min, max } = *range.buffer_size() {
                    let target = 256u32.clamp(min.max(64), max.max(min));
                    cfg.buffer_size = BufferSize::Fixed(target);
                }
                break;
            }
        }
    }

    Ok((cfg, sample_format))
}

fn build_typed_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    ctx: CallbackCtx,
) -> Result<cpal::Stream, String> {
    // Попробуем сначала с запрошенным `config` (с фиксированным низколатентным буфером),
    // а при отказе драйвера — с `BufferSize::Default`.
    let ctx = Arc::new(ctx);
    let try_build = |cfg: &StreamConfig| -> Result<cpal::Stream, cpal::Error> {
        match sample_format {
            SampleFormat::F32 => build_stream_for::<f32>(device, cfg, ctx.clone()),
            SampleFormat::I16 => build_stream_for::<i16>(device, cfg, ctx.clone()),
            SampleFormat::U16 => build_stream_for::<u16>(device, cfg, ctx.clone()),
            SampleFormat::I32 => build_stream_for::<i32>(device, cfg, ctx.clone()),
            SampleFormat::U32 => build_stream_for::<u32>(device, cfg, ctx.clone()),
            SampleFormat::I8 => build_stream_for::<i8>(device, cfg, ctx.clone()),
            SampleFormat::U8 => build_stream_for::<u8>(device, cfg, ctx.clone()),
            SampleFormat::F64 => build_stream_for::<f64>(device, cfg, ctx.clone()),
            other => {
                // На редких форматах запрашиваем у драйвера стандартный F32 стрим.
                let _ = other;
                build_stream_for::<f32>(device, cfg, ctx.clone())
            }
        }
    };

    match try_build(config) {
        Ok(s) => Ok(s),
        Err(_) if config.buffer_size != BufferSize::Default => {
            let mut fallback_cfg = *config;
            fallback_cfg.buffer_size = BufferSize::Default;
            try_build(&fallback_cfg)
                .map_err(|e| format!("failed to build output audio stream: {e}"))
        }
        Err(e) => Err(format!("failed to build output audio stream: {e}")),
    }
}

fn build_stream_for<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    ctx: Arc<CallbackCtx>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + Sample + cpal::FromSample<f32>,
{
    let mut scratch: Vec<f32> = Vec::with_capacity(4096);
    device.build_output_stream(
        *config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            if scratch.len() != data.len() {
                scratch.resize(data.len(), 0.0);
            }
            ctx.render_interleaved_f32(&mut scratch);
            for (dst, &src) in data.iter_mut().zip(scratch.iter()) {
                *dst = T::from_sample(src);
            }
        },
        move |err| {
            eprintln!("[Audio] Stream error: {err}");
        },
        Some(Duration::from_secs(3)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::{Config, Engine, MemoryType, SharedMemory};

    fn make_shared_mem() -> SharedMem {
        let mut cfg = Config::new();
        cfg.wasm_threads(true);
        cfg.shared_memory(true);
        let engine = Engine::new(&cfg).unwrap();
        let sm = SharedMemory::new(&engine, MemoryType::shared(2, 4).unwrap()).unwrap();
        SharedMem::new(sm)
    }

    #[test]
    fn fastpath_planar_double_buffer_matches_audio_processor_fast_js() {
        let mem = make_shared_mem();
        let data_offset = 256;
        let sync_offset = 64;
        let channels = 2usize;
        let buffer_size = 4usize;
        let half_floats = channels * buffer_size; // 8 floats per half

        // Заполним половину 0: L=[0.1, 0.2, 0.3, 0.4], R=[-0.1, -0.2, -0.3, -0.4]
        let half0_l = [0.1f32, 0.2, 0.3, 0.4];
        let half0_r = [-0.1f32, -0.2, -0.3, -0.4];
        // Заполним половину 1: L=[0.5, 0.6, 0.7, 0.8], R=[-0.5, -0.6, -0.7, -0.8]
        let half1_l = [0.5f32, 0.6, 0.7, 0.8];
        let half1_r = [-0.5f32, -0.6, -0.7, -0.8];

        for i in 0..4 {
            mem.write_f32(data_offset + (i * 4) as i32, half0_l[i]).unwrap();
            mem.write_f32(data_offset + ((4 + i) * 4) as i32, half0_r[i])
                .unwrap();
            mem.write_f32(data_offset + ((8 + i) * 4) as i32, half1_l[i])
                .unwrap();
            mem.write_f32(data_offset + ((12 + i) * 4) as i32, half1_r[i])
                .unwrap();
        }
        mem.atomic_store_i32(sync_offset, 0).unwrap();

        let fastpath = Arc::new(Mutex::new(FastpathState {
            ready: true,
            data_offset,
            sync_offset,
            channel_count: channels,
            buffer_size,
            half_floats,
            buffer_flag: 0,
            frame_cursor: 0,
        }));

        let ctx = CallbackCtx {
            mem: mem.clone(),
            dev_channels: 2,
            fastpath: fastpath.clone(),
            fallback: Arc::new(Mutex::new(FallbackMixBuffer::default())),
            worklet_connected: Arc::new(AtomicBool::new(true)),
            script_connected: Arc::new(AtomicBool::new(false)),
            suspended: Arc::new(AtomicBool::new(false)),
            muted: Arc::new(AtomicBool::new(false)),
            volume_bits: Arc::new(AtomicU32::new(1.0f32.to_bits())),
        };

        // Читаем первые 2 кадра из половины 0: syncOffset должен стать 2 (запрос заполнить половину 1)
        let mut out1 = [0.0f32; 4];
        ctx.render_interleaved_f32(&mut out1);
        assert_eq!(mem.atomic_load_i32(sync_offset).unwrap(), 2);
        assert_eq!(out1, [0.1, -0.1, 0.2, -0.2]);

        // Сбросим syncOffset как делает FMOD-поток, дочитаем ещё 4 кадра (остаток половины 0 + 2 кадра половины 1)
        mem.atomic_store_i32(sync_offset, 0).unwrap();
        let mut out2 = [0.0f32; 8];
        ctx.render_interleaved_f32(&mut out2);
        // При переходе на половину 1 syncOffset обязан стать 1 (запрос заполнить половину 0)
        assert_eq!(mem.atomic_load_i32(sync_offset).unwrap(), 1);
        assert_eq!(out2, [0.3, -0.3, 0.4, -0.4, 0.5, -0.5, 0.6, -0.6]);
    }
}
