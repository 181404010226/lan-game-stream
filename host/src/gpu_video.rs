use crate::content::{BlackBars, EDGE_SAMPLES, is_bright, near_middle};
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
    fn lan_nvenc_create(
        device: *mut c_void,
        texture: *mut c_void,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        result: *mut *mut c_void,
    ) -> i32;
    fn lan_nvenc_picture(
        encoder: *mut c_void,
        frame: u64,
        keyframe: i32,
        consume: unsafe extern "C" fn(*mut c_void, *const c_void, u32),
        context: *mut c_void,
    ) -> i32;
    fn lan_nvenc_destroy(encoder: *mut c_void);
}

struct TextureEncoder(*mut c_void);

impl TextureEncoder {
    fn new(
        device: &ID3D11Device,
        texture: &ID3D11Texture2D,
        size: (u32, u32),
        fps: u32,
    ) -> Result<Self> {
        let mut handle = null_mut();
        let code = unsafe {
            lan_nvenc_create(
                device.as_raw(),
                texture.as_raw(),
                size.0,
                size.1,
                fps,
                7_000_000,
                &mut handle,
            )
        };
        if code != 0 || handle.is_null() {
            bail!("NVENC D3D11 初始化失败（{code}）");
        }
        Ok(Self(handle))
    }

    fn picture(&self, index: u32, keyframe: bool) -> Result<Vec<u8>> {
        let mut bytes = Vec::<u8>::new();
        let code = unsafe {
            lan_nvenc_picture(
                self.0,
                u64::from(index),
                i32::from(keyframe),
                collect_output,
                (&mut bytes as *mut Vec<u8>).cast(),
            )
        };
        if code != 0 {
            bail!("NVENC D3D11 编码失败（{code}）");
        }
        Ok(bytes)
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

impl Drop for TextureEncoder {
    fn drop(&mut self) {
        unsafe { lan_nvenc_destroy(self.0) };
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
    shared: Arc<Shared>,
    packets: Mutex<Receiver<Result<GpuPicture>>>,
    worker: Option<JoinHandle<()>>,
}

impl GpuVideo {
    pub const ENCODER: &str = "nvenc-d3d11";

    pub fn open(target: &Target, fps: u32, quality: Quality, detect_bars: bool) -> Result<Self> {
        let shared = Arc::new(Shared::default());
        let (ready, startup) = sync_channel(1);
        let (encoded, packets) = sync_channel(4);
        let target = target.clone();
        let state = shared.clone();
        let worker = thread::spawn(move || {
            if let Err(error) =
                capture(&target, fps, quality, detect_bars, &state, &ready, &encoded)
            {
                let message = format!("{error:#}");
                let _ = ready.try_send(Err(anyhow!(message.clone())));
                let _ = encoded.try_send(Err(anyhow!(message)));
            }
        });
        match startup.recv_timeout(Duration::from_secs(4)) {
            Ok(Ok((size, adapter))) => Ok(Self {
                size,
                adapter,
                shared,
                packets: Mutex::new(packets),
                worker: Some(worker),
            }),
            failure => {
                shared.stop.store(true, Ordering::Relaxed);
                let _ = worker.join();
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
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
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
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

fn nvidia_device() -> Result<(ID3D11Device, ID3D11DeviceContext, String)> {
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

fn capture(
    target: &Target,
    fps: u32,
    quality: Quality,
    detect_bars: bool,
    shared: &Shared,
    startup: &SyncSender<Result<((u32, u32), String)>>,
    encoded: &SyncSender<Result<GpuPicture>>,
) -> Result<()> {
    let _runtime = Runtime::initialize()?;
    let (device, context, adapter) = nvidia_device()?;
    let frames = WindowFrames::open(target, &device)?;
    let first = frames
        .next(Duration::from_secs(2))
        .context("NVIDIA D3D11 取画没有首帧")?;
    let (source, area) = texture_and_crop(target, &first)?;
    let source_size = (area.right - area.left, area.bottom - area.top);
    let size = output_size(source_size.0, source_size.1, quality);
    let texture = input_texture(&device, source_size)?;
    let converter = Nv12Converter::new(&device, &context, &texture, source_size, size, fps)?;
    unsafe {
        context.CopySubresourceRegion(&texture, 0, 0, 0, 0, &source, 0, Some(&area));
    }
    drop(first);
    converter.convert()?;
    let encoder = TextureEncoder::new(&device, converter.texture(), size, fps)?;
    let seed = encoder.picture(0, true)?;
    if seed.is_empty() {
        bail!("NVENC D3D11 没有立即输出首帧");
    }
    encoded.try_send(Ok(GpuPicture {
        index: 0,
        bytes: seed,
    }))?;
    startup
        .send(Ok((size, adapter)))
        .context("接收端已取消启动")?;
    let mut probe = detect_bars
        .then(|| EdgeProbe::new(&device, source_size))
        .transpose()?;
    let mut bars = BlackBars::default();
    let mut sampled = Instant::now() - Duration::from_secs(1);
    let interval = Duration::from_nanos(1_000_000_000 / u64::from(fps));
    let mut deadline = Instant::now() + interval;
    let mut index = 1_u32;
    let mut force_keyframe = false;
    let mut stats_at = Instant::now();
    let mut costs = Vec::new();
    while !shared.stop.load(Ordering::Relaxed) && target.exists() {
        if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            thread::sleep(remaining);
        }
        let now = Instant::now();
        if now.saturating_duration_since(deadline) > interval {
            deadline = now;
        }
        deadline += interval;
        let capture_start = Instant::now();
        if let Some(frame) = frames.next(Duration::ZERO) {
            let (source, area) = texture_and_crop(target, &frame)?;
            if (area.right - area.left, area.bottom - area.top) != source_size {
                bail!("游戏窗口尺寸已变化，请重新连接");
            }
            unsafe {
                context.CopySubresourceRegion(&texture, 0, 0, 0, 0, &source, 0, Some(&area));
            }
            converter.convert()?;
        }
        let bytes = encoder.picture(index, force_keyframe)?;
        if bytes.is_empty() {
            force_keyframe = true;
        } else {
            match encoded.try_send(Ok(GpuPicture { index, bytes })) {
                Ok(()) => force_keyframe = false,
                Err(TrySendError::Full(_)) => force_keyframe = true,
                Err(TrySendError::Disconnected(_)) => break,
            }
            costs.push(capture_start.elapsed().as_micros() as u64);
        }
        index = index.wrapping_add(1);
        if let Some(probe) = &mut probe
            && sampled.elapsed() >= Duration::from_millis(210)
        {
            sampled = Instant::now();
            probe.observe(&context, &texture, &mut bars)?;
            *shared.content.lock().unwrap() = bars.current();
        }
        if stats_at.elapsed() >= Duration::from_secs(5) && !costs.is_empty() {
            costs.sort_unstable();
            println!(
                "D3D11 → NVENC: {} fps, GPU 复制和编码 p50={}us p95={}us",
                (costs.len() as f64 / stats_at.elapsed().as_secs_f64()).round(),
                costs[costs.len() / 2],
                costs[costs.len() * 95 / 100]
            );
            costs.clear();
            stats_at = Instant::now();
        }
    }
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
        let encoder = TextureEncoder::new(&device, converter.texture(), size, 120).unwrap();
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
            converter.convert().unwrap();
            if frame == 0 {
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                unsafe {
                    context.CopyResource(diagnostic.texture(), converter.texture());
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
            let bytes = encoder.picture(frame, frame % 6 == 0).unwrap();
            assert!(!bytes.is_empty());
            output.extend_from_slice(&bytes);
        }
        if let Some(path) = std::env::var_os("LAN_STREAM_COLOR_TEST_OUT") {
            std::fs::write(path, output).unwrap();
        }
    }
}
use crate::color_gpu::Nv12Converter;
