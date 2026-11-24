use crate::color_gpu::{NVENC_SURFACES, Nv12Converter};
use crate::content::{BlackBars, EDGE_SAMPLES, is_bright, near_middle};
use crate::game_texture::{CaptureMode, GameTextures};
use crate::video::{Quality, output_size};
use crate::window::{Target, capture_crop};
use anyhow::{Context, Result, anyhow, bail};
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use stream_wire::{ContentInsets, MAX_MEDIA};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::{IInspectable, Interface, factory};
use windows_capture::d3d11::{StagingTexture, create_direct3d_device};

unsafe extern "C" {
    fn lan_frame_clock_us() -> i64;
    fn lan_keep_graphics_capture_loaded();
    fn lan_nvenc_create(
        device: *mut c_void,
        textures: *const *mut c_void,
        texture_count: u32,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        spatial_aq: i32,
        asynchronous: i32,
        result: *mut *mut c_void,
    ) -> i32;
    fn lan_nvenc_queue(encoder: *mut c_void, next_slot: *mut u32) -> u32;
    fn lan_nvenc_submit(encoder: *mut c_void, slot: u32, frame: u32, keyframe: i32) -> i32;
    fn lan_nvenc_receive(
        encoder: *mut c_void,
        wait_us: u32,
        frame_event: *mut c_void,
        info: *mut OutputInfo,
        consume: unsafe extern "C" fn(*mut c_void, *const c_void, u32),
        context: *mut c_void,
    ) -> i32;
    fn lan_nvenc_destroy(encoder: *mut c_void);
}

#[repr(C)]
#[derive(Default)]
struct OutputInfo {
    frame: u32,
    slot: u32,
    idr: u32,
}

struct CompletedPicture {
    picture: GpuPicture,
    idr: bool,
    elapsed: Duration,
}

struct TextureEncoder<'a> {
    raw: *mut c_void,
    // The driver borrows the textures until the encoder has drained and unregistered them.
    _surfaces: &'a Nv12Converter,
    asynchronous: bool,
    started: [Instant; NVENC_SURFACES],
}

impl<'a> TextureEncoder<'a> {
    fn new(
        device: &ID3D11Device,
        surfaces: &'a Nv12Converter,
        size: (u32, u32),
        fps: u32,
        quality: Quality,
    ) -> Result<Self> {
        Self::open_mode(device, surfaces, size, fps, quality, true).or_else(|error| {
            eprintln!("NVENC 异步初始化失败，尝试同步 D3D11：{error}");
            Self::open_mode(device, surfaces, size, fps, quality, false)
        })
    }

    fn open_mode(
        device: &ID3D11Device,
        surfaces: &'a Nv12Converter,
        size: (u32, u32),
        fps: u32,
        quality: Quality,
        asynchronous: bool,
    ) -> Result<Self> {
        let (bitrate, spatial_aq) = quality.nvenc_quality();
        let textures: [_; NVENC_SURFACES] = std::array::from_fn(|i| surfaces.texture(i).as_raw());
        let mut raw = null_mut();
        let code = unsafe {
            lan_nvenc_create(
                device.as_raw(),
                textures.as_ptr(),
                textures.len() as u32,
                size.0,
                size.1,
                fps,
                bitrate,
                i32::from(spatial_aq),
                i32::from(asynchronous),
                &mut raw,
            )
        };
        if code != 0 || raw.is_null() {
            bail!("NVENC D3D11 初始化失败（{code}）");
        }
        Ok(Self {
            raw,
            _surfaces: surfaces,
            asynchronous,
            started: [Instant::now(); NVENC_SURFACES],
        })
    }

    fn queue(&self) -> (usize, usize) {
        let mut slot = 0;
        let count = unsafe { lan_nvenc_queue(self.raw, &mut slot) };
        (count as usize, slot as usize)
    }

    fn submit(
        &mut self,
        slot: usize,
        index: u32,
        keyframe: bool,
        started: Instant,
    ) -> Result<bool> {
        match unsafe { lan_nvenc_submit(self.raw, slot as u32, index, i32::from(keyframe)) } {
            0 => {
                self.started[slot] = started;
                Ok(true)
            }
            1 => Ok(false),
            code => bail!("NVENC D3D11 提交失败（{code}）"),
        }
    }

    fn receive(&mut self, wait: Duration) -> Result<Option<CompletedPicture>> {
        self.receive_or_frame(wait, null_mut())
    }

    fn receive_or_frame(
        &mut self,
        wait: Duration,
        frame_event: *mut c_void,
    ) -> Result<Option<CompletedPicture>> {
        let mut bytes = Vec::<u8>::new();
        let mut info = OutputInfo::default();
        let code = unsafe {
            lan_nvenc_receive(
                self.raw,
                wait.as_micros().min(u128::from(u32::MAX)) as u32,
                frame_event,
                &mut info,
                collect_output,
                (&mut bytes as *mut Vec<u8>).cast(),
            )
        };
        match code {
            0 => Ok(Some(CompletedPicture {
                picture: GpuPicture {
                    index: info.frame,
                    bytes,
                },
                idr: info.idr != 0,
                elapsed: self.started[info.slot as usize].elapsed(),
            })),
            1 => Ok(None),
            _ => bail!("NVENC D3D11 读取码流失败（{code}）"),
        }
    }
}

unsafe extern "C" fn collect_output(context: *mut c_void, data: *const c_void, len: u32) {
    if data.is_null() || len as usize > MAX_MEDIA {
        return;
    }
    unsafe {
        (*context.cast::<Vec<u8>>())
            .extend_from_slice(std::slice::from_raw_parts(data.cast(), len as usize));
    }
}

impl Drop for TextureEncoder<'_> {
    fn drop(&mut self) {
        crate::teardown("NVENC resources closing");
        unsafe { lan_nvenc_destroy(self.raw) };
        crate::teardown("NVENC resources closed");
    }
}

pub struct GpuPicture {
    pub index: u32,
    pub bytes: Vec<u8>,
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    content: Mutex<ContentInsets>,
}

pub struct GpuVideo {
    pub size: (u32, u32),
    pub adapter: String,
    pub encoder_name: &'static str,
    shared: Arc<Shared>,
    packets: Mutex<Receiver<Result<GpuPicture>>>,
    worker: Option<JoinHandle<()>>,
}

static GPU_CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);

struct GpuCaptureLease;

impl GpuCaptureLease {
    fn acquire() -> Result<Self> {
        GPU_CAPTURE_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow!("上一次 D3D11 会话仍在释放"))?;
        Ok(Self)
    }
}

impl Drop for GpuCaptureLease {
    fn drop(&mut self) {
        GPU_CAPTURE_ACTIVE.store(false, Ordering::Release);
    }
}

fn finish_worker(worker: JoinHandle<()>) {
    let deadline = Instant::now() + Duration::from_millis(250);
    while !worker.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    if worker.is_finished() {
        let _ = worker.join();
        crate::teardown("GPU worker joined");
    } else {
        // The driver may still be cleaning up. Keep accepting clients, and let the lease
        // route the next session to the compatible encoder until this worker actually exits.
        crate::teardown("GPU worker still closing; listener released");
    }
}

impl GpuVideo {
    pub const ENCODER: &str = "nvenc-d3d11";

    pub fn open(
        target: &Target,
        fps: u32,
        quality: Quality,
        detect_bars: bool,
        mode: CaptureMode,
        requested_size: Option<(u32, u32)>,
    ) -> Result<Self> {
        let lease = GpuCaptureLease::acquire()?;
        let shared = Arc::new(Shared::default());
        let (ready, startup) = sync_channel(1);
        let (encoded, packets) = sync_channel(4);
        let target = target.clone();
        let state = shared.clone();
        let worker = thread::spawn(move || {
            let _lease = lease;
            if let Err(error) = capture(
                &target,
                fps,
                quality,
                detect_bars,
                mode,
                requested_size,
                &state,
                &ready,
                &encoded,
            ) {
                let message = format!("{error:#}");
                let _ = ready.try_send(Err(anyhow!(message.clone())));
                let _ = encoded.try_send(Err(anyhow!(message)));
            }
        });
        match startup.recv_timeout(Duration::from_secs(4)) {
            Ok(Ok((size, adapter, encoder_name))) => Ok(Self {
                size,
                adapter,
                encoder_name,
                shared,
                packets: Mutex::new(packets),
                worker: Some(worker),
            }),
            failure => {
                shared.stop.store(true, Ordering::Relaxed);
                finish_worker(worker);
                match failure {
                    Ok(Err(error)) => Err(error),
                    _ => bail!("等待 NVIDIA D3D11 首帧超时"),
                }
            }
        }
    }

    pub fn content(&self) -> ContentInsets {
        *self.shared.content.lock().unwrap()
    }

    pub fn pump(
        &self,
        stop: &AtomicBool,
        mut emit: impl FnMut(GpuPicture) -> Result<()>,
    ) -> Result<()> {
        let packets = self.packets.lock().unwrap();
        while !stop.load(Ordering::Relaxed) {
            match packets.recv_timeout(Duration::from_millis(20)) {
                Ok(picture) => emit(picture?)?,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => bail!("D3D11 取画已结束"),
            }
        }
        Ok(())
    }

    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for GpuVideo {
    fn drop(&mut self) {
        self.stop();
        crate::teardown("waiting for GPU worker");
        if let Some(worker) = self.worker.take() {
            finish_worker(worker);
        }
    }
}

struct Runtime;
impl Runtime {
    fn initialize() -> windows::core::Result<Self> {
        unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
        Ok(Self)
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

struct WindowFrames {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    received: Receiver<()>,
    token: i64,
}

impl WindowFrames {
    fn open(target: &Target, device: &ID3D11Device) -> Result<Self> {
        let winrt = create_direct3d_device(device)?;
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(HWND(target.hwnd()))? };
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            item.Size()?,
        )?;
        let (notice, received) = sync_channel(1);
        let token = pool.FrameArrived(&TypedEventHandler::<
            Direct3D11CaptureFramePool,
            IInspectable,
        >::new(move |_, _| {
            let _ = notice.try_send(());
            Ok(())
        }))?;
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsCursorCaptureEnabled(true);
        let video = Self {
            pool,
            session,
            received,
            token,
        };
        video.session.StartCapture()?;
        unsafe { lan_keep_graphics_capture_loaded() };
        Ok(video)
    }

    fn next(&self, wait: Duration) -> Option<Direct3D11CaptureFrame> {
        if self.received.recv_timeout(wait).is_err() {
            return None;
        }
        let mut latest = self.pool.TryGetNextFrame().ok()?;
        // The pool has two buffers. Discard an older frame when both are available.
        if let Ok(newer) = self.pool.TryGetNextFrame() {
            latest = newer;
        }
        Some(latest)
    }
}

impl Drop for WindowFrames {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.token);
        crate::teardown("closing WGC session");
        let _ = self.session.Close();
        crate::teardown("closing WGC frame pool");
        let _ = self.pool.Close();
        crate::teardown("WGC closed");
    }
}

type NvidiaDevice = (ID3D11Device, ID3D11DeviceContext, String);
// Keep one adapter alive across video sessions. The capture lease serializes use
// of its immediate context, and the driver can finish retiring resources without
// unloading the device while a client is reconnecting.
static NVIDIA_DEVICE: Mutex<Option<NvidiaDevice>> = Mutex::new(None);

fn nvidia_device() -> Result<NvidiaDevice> {
    let mut cached = NVIDIA_DEVICE.lock().unwrap();
    if let Some(device) = cached.as_ref()
        && unsafe { device.0.GetDeviceRemovedReason() }.is_ok()
    {
        return Ok(device.clone());
    }
    let device = create_nvidia_device()?;
    *cached = Some(device.clone());
    Ok(device)
}

fn create_nvidia_device() -> Result<NvidiaDevice> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    for number in 0..32 {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(number) }) else {
            break;
        };
        let desc = unsafe { adapter.GetDesc1()? };
        if desc.VendorId != 0x10de {
            continue;
        }
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let name_end = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..name_end]);
        return Ok((
            device.context("没有 NVIDIA D3D11 设备")?,
            context.context("没有 NVIDIA D3D11 上下文")?,
            name,
        ));
    }
    bail!("没有 NVIDIA D3D11 适配器")
}

fn texture_and_crop(
    target: &Target,
    frame: &Direct3D11CaptureFrame,
) -> Result<(ID3D11Texture2D, D3D11_BOX)> {
    let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
    let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    let s = frame.ContentSize()?;
    let (left, top, right, bottom) = capture_crop(
        target.hwnd(),
        desc.Width.min(s.Width.max(0) as u32),
        desc.Height.min(s.Height.max(0) as u32),
    );
    if right < left + 64 || bottom < top + 64 {
        bail!("游戏窗口尺寸不可用");
    }
    Ok((
        texture,
        D3D11_BOX {
            left,
            top,
            right,
            bottom,
            front: 0,
            back: 1,
        },
    ))
}

fn input_texture(device: &ID3D11Device, size: (u32, u32)) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
        ..Default::default()
    };
    let mut texture = None;
    unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut texture))?;
    }
    texture.context("无法分配 NVENC 输入纹理")
}

enum FrameInput {
    Window {
        frames: WindowFrames,
        rgb: ID3D11Texture2D,
        size: (u32, u32),
        generation: u64,
        converted: [Option<u64>; NVENC_SURFACES],
    },
    Game(GameTextures),
}

impl FrameInput {
    fn open(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        target: &Target,
        mode: CaptureMode,
        requested: Option<(u32, u32)>,
    ) -> Result<(Self, (u32, u32))> {
        if mode != CaptureMode::Window
            && let Some(size) = requested
        {
            match GameTextures::open(device, target.pid, size, mode) {
                Ok(Some(reader)) => return Ok((Self::Game(reader), size)),
                Ok(None) if mode.required() => {
                    bail!("游戏内适配器未加载，请启用 LanStream-Offscreen 或选择窗口取帧")
                }
                Err(err) if mode.required() => return Err(err),
                Err(err) => eprintln!("游戏内纹理不可用，使用窗口取帧：{err:#}"),
                Ok(None) => {}
            }
        }
        let frames = WindowFrames::open(target, device)?;
        let first = frames
            .next(Duration::from_secs(2))
            .context("NVIDIA D3D11 取画没有首帧")?;
        unsafe { lan_keep_graphics_capture_loaded() };
        let (source, area) = texture_and_crop(target, &first)?;
        let size = (area.right - area.left, area.bottom - area.top);
        let rgb = input_texture(device, size)?;
        unsafe {
            context.CopySubresourceRegion(&rgb, 0, 0, 0, 0, &source, 0, Some(&area));
        }
        Ok((
            Self::Window {
                frames,
                rgb,
                size,
                generation: 0,
                converted: [None; NVENC_SURFACES],
            },
            size,
        ))
    }
    fn textures(&self) -> &[ID3D11Texture2D] {
        match self {
            Self::Window { rgb, .. } => std::slice::from_ref(rgb),
            Self::Game(game) => &game.textures,
        }
    }
    fn name(&self) -> &'static str {
        match self {
            Self::Window { .. } => GpuVideo::ENCODER,
            Self::Game(game) => game.encoder_name,
        }
    }
    fn convert(
        &mut self,
        target: &Target,
        context: &ID3D11DeviceContext,
        converter: &Nv12Converter,
        slot: usize,
        wait: Duration,
    ) -> Result<bool> {
        match self {
            Self::Game(game) => game.convert(converter, context, slot, wait),
            Self::Window {
                frames,
                rgb,
                size,
                generation,
                converted,
            } => {
                if let Some(frame) = frames.next(Duration::ZERO) {
                    let (source, area) = texture_and_crop(target, &frame)?;
                    if (area.right - area.left, area.bottom - area.top) != *size {
                        bail!("游戏窗口尺寸已变化，请重新连接");
                    }
                    unsafe {
                        context.CopySubresourceRegion(&*rgb, 0, 0, 0, 0, &source, 0, Some(&area));
                    }
                    *generation = generation.wrapping_add(1);
                }
                if converted[slot] != Some(*generation) {
                    converter.convert(slot)?;
                    converted[slot] = Some(*generation);
                }
                Ok(true)
            }
        }
    }
}

fn capture(
    target: &Target,
    fps: u32,
    quality: Quality,
    detect_bars: bool,
    capture_mode: CaptureMode,
    requested: Option<(u32, u32)>,
    shared: &Shared,
    startup: &SyncSender<Result<((u32, u32), String, &'static str)>>,
    encoded: &SyncSender<Result<GpuPicture>>,
) -> Result<()> {
    let _runtime = Runtime::initialize()?;
    let (device, context, adapter) = nvidia_device()?;
    let (mut input, source_size) =
        FrameInput::open(&device, &context, target, capture_mode, requested)?;
    let size = output_size(source_size.0, source_size.1, quality);
    let converter =
        Nv12Converter::from_inputs(&device, &context, input.textures(), source_size, size, fps)?;
    let mut encoder = TextureEncoder::new(&device, &converter, size, fps, quality)?;
    let started = Instant::now();
    if !input.convert(target, &context, &converter, 0, Duration::from_millis(750))? {
        bail!("等待游戏内首张可读纹理超时");
    }
    if !encoder.submit(0, 0, true, started)? {
        bail!("NVENC D3D11 未接受首帧");
    }
    let seed = encoder
        .receive(Duration::from_millis(1500))?
        .context("NVENC D3D11 等待首帧超时")?;
    if !seed.idr || seed.picture.bytes.is_empty() {
        bail!("NVENC D3D11 首帧缺少 IDR 码流");
    }
    let mut trace = std::env::var_os("LAN_STREAM_ENCODE_TRACE")
        .map(std::fs::File::create)
        .transpose()?
        .map(std::io::BufWriter::new);
    if let Some(file) = &mut trace {
        use std::io::Write;
        writeln!(file, "seq,encoded_us")?;
        writeln!(file, "{},{}", seed.picture.index, unsafe {
            lan_frame_clock_us()
        })?;
    }
    encoded.try_send(Ok(seed.picture))?;
    let mode = if encoder.asynchronous {
        "异步"
    } else {
        "同步"
    };
    let limit = if encoder.asynchronous { 2 } else { 1 };
    println!("D3D11 NVENC {mode}模式，最多 {limit} 帧在途");
    startup
        .send(Ok((size, adapter, input.name())))
        .context("接收端已取消启动")?;
    let mut probe = (detect_bars && matches!(input, FrameInput::Window { .. }))
        .then(|| EdgeProbe::new(&device, source_size))
        .transpose()?;
    let mut bars = BlackBars::default();
    let mut sampled = Instant::now() - Duration::from_secs(1);
    let interval = Duration::from_nanos(1_000_000_000 / u64::from(fps));
    let mut deadline = Instant::now() + interval;
    let mut index = 1_u32;
    let mut need_idr = false;
    let mut stats_at = Instant::now();
    let mut costs = Vec::new();
    let mut max_pending = 0;
    let frame_event = match &input {
        FrameInput::Game(game) => game.event(),
        _ => null_mut(),
    };
    let mut waiting_for_source = false;
    let mut last_sent = Instant::now();
    let mut last_input_slot = 0;
    while !shared.stop.load(Ordering::Relaxed) && target.exists() {
        let (pending, slot) = encoder.queue();
        let remaining = deadline.saturating_duration_since(Instant::now());
        if pending > 0 {
            let wait = if pending == limit || waiting_for_source {
                interval
            } else {
                remaining
            };
            let wake = if pending < limit && waiting_for_source {
                frame_event
            } else {
                null_mut()
            };
            if let Some(done) = encoder.receive_or_frame(wait, wake)? {
                if let Some(file) = &mut trace {
                    use std::io::Write;
                    writeln!(file, "{},{}", done.picture.index, unsafe {
                        lan_frame_clock_us()
                    })?;
                }
                costs.push(done.elapsed.as_micros() as u64);
                if done.picture.bytes.is_empty() {
                    need_idr = true;
                } else if !need_idr || done.idr {
                    match encoded.try_send(Ok(done.picture)) {
                        Ok(()) => need_idr = false,
                        Err(TrySendError::Full(_)) => need_idr = true,
                        Err(TrySendError::Disconnected(_)) => break,
                    }
                }
                if stats_at.elapsed() >= Duration::from_secs(5) {
                    costs.sort_unstable();
                    println!(
                        "D3D11 → NVENC ({mode}): {} fps, 采集至取出 p50={}us p95={}us, 峰值在途={max_pending}",
                        (costs.len() as f64 / stats_at.elapsed().as_secs_f64()).round(),
                        costs[costs.len() / 2],
                        costs[costs.len() * 95 / 100],
                    );
                    costs.clear();
                    max_pending = 0;
                    stats_at = Instant::now();
                }
                continue;
            }
            let oldest = (slot + NVENC_SURFACES - pending) % NVENC_SURFACES;
            if encoder.started[oldest].elapsed() > Duration::from_secs(2) {
                bail!("NVENC D3D11 等待在途帧超时");
            }
            if pending == limit {
                continue;
            }
        } else if !remaining.is_zero() {
            thread::sleep(remaining);
        } else if waiting_for_source && !frame_event.is_null() {
            unsafe {
                windows_sys::Win32::System::Threading::WaitForSingleObject(frame_event, 16);
            }
        }
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let now = Instant::now();
        if now < deadline {
            continue;
        }
        let capture_start = Instant::now();
        if !input.convert(target, &context, &converter, slot, Duration::ZERO)? {
            // A cooperative game may leave an unchanged menu idle. Keep the existing
            // video session alive using the last retired NV12 image, never a mapped slot.
            if pending != 0 || last_sent.elapsed() < Duration::from_millis(250) {
                waiting_for_source = true;
                continue;
            }
            if slot != last_input_slot {
                unsafe {
                    context
                        .CopyResource(converter.texture(slot), converter.texture(last_input_slot));
                }
            }
        }
        waiting_for_source = false;
        if now.saturating_duration_since(deadline) > interval {
            deadline = now;
        }
        deadline += interval;
        if encoder.submit(slot, index, need_idr, capture_start)? {
            last_sent = Instant::now();
            last_input_slot = slot;
            index = index.wrapping_add(1);
            max_pending = max_pending.max(pending + 1);
        }
        if let Some(probe) = &mut probe
            && sampled.elapsed() >= Duration::from_millis(210)
        {
            sampled = Instant::now();
            probe.observe(&context, &input.textures()[0], &mut bars)?;
            *shared.content.lock().unwrap() = bars.current();
        }
    }
    crate::teardown("capture loop stopped");
    Ok(())
}

struct EdgeProbe {
    size: (u32, u32),
    rows: StagingTexture,
    columns: StagingTexture,
    row_bytes: Vec<u8>,
    column_bytes: Vec<u8>,
}

impl EdgeProbe {
    fn new(device: &ID3D11Device, size: (u32, u32)) -> Result<Self> {
        Ok(Self {
            size,
            rows: StagingTexture::new(
                device,
                size.0,
                EDGE_SAMPLES as u32,
                DXGI_FORMAT_B8G8R8A8_UNORM,
            )?,
            columns: StagingTexture::new(
                device,
                EDGE_SAMPLES as u32,
                size.1,
                DXGI_FORMAT_B8G8R8A8_UNORM,
            )?,
            row_bytes: Vec::new(),
            column_bytes: Vec::new(),
        })
    }

    fn observe(
        &mut self,
        context: &ID3D11DeviceContext,
        source: &ID3D11Texture2D,
        bars: &mut BlackBars,
    ) -> Result<()> {
        let (width, height) = self.size;
        for i in 0..EDGE_SAMPLES {
            let y = near_middle(height as usize, i) as u32;
            let row = D3D11_BOX {
                left: 0,
                top: y,
                front: 0,
                right: width,
                bottom: y + 1,
                back: 1,
            };
            let x = near_middle(width as usize, i) as u32;
            let column = D3D11_BOX {
                left: x,
                top: 0,
                front: 0,
                right: x + 1,
                bottom: height,
                back: 1,
            };
            unsafe {
                context.CopySubresourceRegion(
                    self.rows.texture(),
                    0,
                    0,
                    i as u32,
                    0,
                    source,
                    0,
                    Some(&row),
                );
                context.CopySubresourceRegion(
                    self.columns.texture(),
                    0,
                    i as u32,
                    0,
                    0,
                    source,
                    0,
                    Some(&column),
                );
            }
        }
        read_samples(
            context,
            &self.rows,
            width as usize,
            EDGE_SAMPLES,
            &mut self.row_bytes,
        )?;
        read_samples(
            context,
            &self.columns,
            EDGE_SAMPLES,
            height as usize,
            &mut self.column_bytes,
        )?;
        bars.observe_edges(
            width as usize,
            height as usize,
            |x, n| is_bright(&self.row_bytes, x, n, width as usize),
            |y, n| is_bright(&self.column_bytes, n, y, EDGE_SAMPLES),
        );
        Ok(())
    }
}

fn read_samples(
    context: &ID3D11DeviceContext,
    staging: &StagingTexture,
    w: usize,
    h: usize,
    data: &mut Vec<u8>,
) -> Result<()> {
    data.clear();
    data.reserve(w * h * 4);
    let texture = staging.texture();
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        context.Map(texture, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let rows =
            std::slice::from_raw_parts(mapped.pData.cast::<u8>(), mapped.RowPitch as usize * h);
        for row in rows.chunks_exact(mapped.RowPitch as usize) {
            data.extend_from_slice(&row[..w * 4]);
        }
        context.Unmap(texture, 0);
    }
    Ok(())
}

#[cfg(test)]
mod gpu_color_test {
    use super::*;

    #[test]
    #[ignore = "requires a local NVIDIA D3D11 and NVENC driver"]
    fn nv12_preserves_neutral_and_saturated_color_patches() {
        let _runtime = Runtime::initialize().unwrap();
        let (device, context, _) = nvidia_device().unwrap();
        let size = (320_u32, 192_u32);
        let texture = input_texture(&device, size).unwrap();
        let converter = Nv12Converter::new(&device, &context, &texture, size, size, 120).unwrap();
        let mut encoder =
            TextureEncoder::new(&device, &converter, size, 120, Quality::Latency).unwrap();
        let limit = if encoder.asynchronous { 2 } else { 1 };
        let samples: [[u8; 3]; 8] = [
            [0, 0, 0],
            [32, 32, 32],
            [128, 128, 128],
            [255, 255, 255],
            [220, 35, 35],
            [35, 220, 35],
            [35, 35, 220],
            [35, 200, 220],
        ];
        let mut output = Vec::new();
        let mut expected = 0_u32;
        let mut take = |encoder: &mut TextureEncoder<'_>| {
            let done = encoder.receive(Duration::from_secs(1)).unwrap().unwrap();
            assert_eq!(done.picture.index, expected);
            assert!(!done.picture.bytes.is_empty());
            if expected % 6 == 0 {
                assert!(done.idr);
            }
            output.extend_from_slice(&done.picture.bytes);
            expected += 1;
        };
        let diagnostic = StagingTexture::new(
            &device,
            size.0,
            size.1,
            windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12,
        )
        .unwrap();
        for frame in 0..18_u32 {
            let mut pixels = Vec::with_capacity((size.0 * size.1 * 4) as usize);
            for y in 0..size.1 {
                for x in 0..size.0 {
                    let [r, g, b] = if y > 136 && (frame * 10..frame * 10 + 36).contains(&x) {
                        [200, 40, 160]
                    } else {
                        samples[(x / 40) as usize]
                    };
                    pixels.extend_from_slice(&[b, g, r, 255]);
                }
            }
            unsafe {
                context.UpdateSubresource(&texture, 0, None, pixels.as_ptr().cast(), size.0 * 4, 0);
            }
            let (pending, slot) = encoder.queue();
            let started = Instant::now();
            converter.convert(slot).unwrap();
            if frame == 0 {
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                unsafe {
                    context.CopyResource(diagnostic.texture(), converter.texture(slot));
                    context
                        .Map(
                            diagnostic.texture(),
                            0,
                            D3D11_MAP_READ,
                            0,
                            Some(&mut mapped),
                        )
                        .unwrap();
                    let pitch = mapped.RowPitch as usize;
                    let data = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), pitch * 288);
                    let values: Vec<_> = (0..8)
                        .map(|slot| {
                            let x = slot * 40 + 20;
                            let uv = pitch * (192 + 30) + x;
                            (data[60 * pitch + x], data[uv], data[uv + 1])
                        })
                        .collect();
                    println!("Actual NV12 YUV patches {values:?}");
                    context.Unmap(diagnostic.texture(), 0);
                }
            }
            assert!(
                encoder
                    .submit(slot, frame, frame % 6 == 0, started)
                    .unwrap()
            );
            if pending + 1 == limit {
                take(&mut encoder);
            }
        }
        while encoder.queue().0 > 0 {
            take(&mut encoder);
        }
        assert_eq!(expected, 18);
        println!(
            "NVENC hardware test: asynchronous={}, in-flight limit={limit}",
            encoder.asynchronous
        );
        if let Some(path) = std::env::var_os("LAN_STREAM_COLOR_TEST_OUT") {
            std::fs::write(path, output).unwrap();
        }
        // Close with work still in flight, then reuse the same textures in a sync session.
        for frame in 18..18 + limit as u32 {
            let (_, slot) = encoder.queue();
            converter.convert(slot).unwrap();
            assert!(encoder.submit(slot, frame, false, Instant::now()).unwrap());
        }
        drop(encoder);
        let mut sync =
            TextureEncoder::open_mode(&device, &converter, size, 120, Quality::Latency, false)
                .unwrap();
        converter.convert(0).unwrap();
        assert!(sync.submit(0, 0, true, Instant::now()).unwrap());
        assert!(sync.receive(Duration::from_secs(1)).unwrap().unwrap().idr);
    }
}
