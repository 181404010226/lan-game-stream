use crate::content::BlackBars;
use crate::window::{Target, capture_crop};
use anyhow::{Context as _, Result, anyhow, bail};
use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use stream_wire::ContentInsets;
use windows::Win32::Graphics::Direct3D11::{D3D11_BOX, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::d3d11::StagingTexture;
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::settings::*;
use windows_capture::window::Window;

pub const FPS: u32 = 60;
const CREATE_NO_WINDOW: u32 = 0x08000000;
type CaptureError = Box<dyn std::error::Error + Send + Sync>;

pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    pool: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Drop for Pixels {
    fn drop(&mut self) {
        let mut pool = self.pool.lock().unwrap();
        if pool.len() < 3 && self.bgra.capacity() < 48 * 1024 * 1024 {
            pool.push(std::mem::take(&mut self.bgra));
        }
    }
}

#[derive(Default)]
struct FrameState {
    latest: Option<Arc<Pixels>>,
    closed: bool,
    content: ContentInsets,
}

#[derive(Default)]
pub struct FrameSlot {
    state: Mutex<FrameState>,
    ready: Condvar,
}

impl FrameSlot {
    pub fn first(&self) -> Result<Arc<Pixels>> {
        let (state, _) = self
            .ready
            .wait_timeout_while(
                self.state.lock().unwrap(),
                Duration::from_secs(5),
                |state| state.latest.is_none() && !state.closed,
            )
            .unwrap();
        state
            .latest
            .clone()
            .context("等待游戏画面超时，请取消最小化或切换为无边框/窗口模式")
    }

    fn latest(&self) -> Option<Arc<Pixels>> {
        let state = self.state.lock().unwrap();
        if state.closed {
            return None;
        }
        state.latest.clone()
    }

    pub fn content(&self) -> ContentInsets {
        self.state.lock().unwrap().content
    }
}

struct WindowGrab {
    target: Target,
    slot: Arc<FrameSlot>,
    next_capture: Instant,
    capture_interval: Duration,
    bars: Option<BlackBars>,
    last_sample: Instant,
    staging: Option<StagingTexture>,
    pool: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl GraphicsCaptureApiHandler for WindowGrab {
    type Flags = (Target, Arc<FrameSlot>, bool, u32);
    type Error = CaptureError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let (target, slot, inspect, fps) = ctx.flags;
        Ok(Self {
            target,
            slot,
            next_capture: Instant::now(),
            capture_interval: Duration::from_nanos(1_000_000_000 / u64::from(fps)),
            bars: inspect.then(BlackBars::default),
            last_sample: Instant::now() - Duration::from_secs(1),
            staging: None,
            pool: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let now = Instant::now();
        if now + self.capture_interval / 5 < self.next_capture {
            return Ok(());
        }
        if now.saturating_duration_since(self.next_capture) > self.capture_interval {
            self.next_capture = now;
        }
        self.next_capture += self.capture_interval;
        let (left, top, right, bottom) =
            capture_crop(self.target.hwnd(), frame.width(), frame.height());
        if right < left + 64 || bottom < top + 64 {
            return Ok(());
        }
        let width = right - left;
        let height = bottom - top;
        let format = frame.desc().Format;
        if self.staging.as_ref().is_none_or(|texture| {
            let desc = texture.desc();
            desc.Width != width || desc.Height != height || desc.Format != format
        }) {
            self.staging = Some(StagingTexture::new(frame.device(), width, height, format)?);
        }
        let region = D3D11_BOX {
            left,
            top,
            right,
            bottom,
            front: 0,
            back: 1,
        };
        let mut bgra = self.pool.lock().unwrap().pop().unwrap_or_default();
        read_pixels(frame, self.staging.as_ref().unwrap(), region, &mut bgra)?;
        if let Some(bars) = &mut self.bars
            && self.last_sample.elapsed() >= Duration::from_millis(125)
        {
            self.last_sample = Instant::now();
            bars.observe(&bgra, width, height);
        }
        let mut state = self.slot.state.lock().unwrap();
        state.content = self
            .bars
            .as_ref()
            .map_or(ContentInsets::default(), BlackBars::current);
        state.latest = Some(Arc::new(Pixels {
            width,
            height,
            bgra,
            pool: self.pool.clone(),
        }));
        self.slot.ready.notify_one();
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.slot.state.lock().unwrap().closed = true;
        self.slot.ready.notify_one();
        Ok(())
    }
}

fn read_pixels(
    frame: &Frame,
    staging: &StagingTexture,
    region: D3D11_BOX,
    output: &mut Vec<u8>,
) -> windows::core::Result<()> {
    let context = frame.device_context();
    let texture = staging.texture();
    let mut map = D3D11_MAPPED_SUBRESOURCE::default();
    let height = (region.bottom - region.top) as usize;
    let row_bytes = (region.right - region.left) as usize * 4;
    output.clear();
    output.reserve(height * row_bytes);
    // Both resources were created by the capture device. The mapped memory lasts until Unmap.
    unsafe {
        context.CopySubresourceRegion(
            texture,
            0,
            0,
            0,
            0,
            frame.as_raw_texture(),
            0,
            Some(&region),
        );
        context.Map(texture, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
        let stride = map.RowPitch as usize;
        let rows = std::slice::from_raw_parts(map.pData.cast::<u8>(), stride * height);
        for row in rows.chunks_exact(stride) {
            output.extend_from_slice(&row[..row_bytes]);
        }
        context.Unmap(texture, 0);
    }
    Ok(())
}

pub struct Capture {
    pub slot: Arc<FrameSlot>,
    control: Option<CaptureControl<WindowGrab, CaptureError>>,
}

impl Capture {
    pub fn start(target: &Target, content_updates: bool, fps: u32) -> Result<Self> {
        let slot = Arc::new(FrameSlot::default());
        let window = Window::from_raw_hwnd(target.hwnd());
        let settings = Settings::new(
            window,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            (target.clone(), slot.clone(), content_updates, fps),
        );
        let control = WindowGrab::start_free_threaded(settings)
            .map_err(|err| anyhow!("窗口采集失败：{err}"))?;
        Ok(Self {
            slot,
            control: Some(control),
        })
    }

    pub fn ended(&self) -> bool {
        self.control
            .as_ref()
            .is_none_or(CaptureControl::is_finished)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(control) = self.control.take()
            && let Err(error) = control.stop()
        {
            eprintln!("采集结束：{error}");
        }
    }
}

#[derive(Clone, Copy)]
enum Codec {
    Nvenc,
    Amf,
    X264,
}

impl Codec {
    fn name(self) -> &'static str {
        match self {
            Self::Nvenc => "h264_nvenc",
            Self::Amf => "h264_amf",
            Self::X264 => "libx264",
        }
    }

    fn options(self) -> &'static [&'static str] {
        match self {
            Self::Nvenc => &[
                "-preset",
                "p3",
                "-tune",
                "ull",
                "-rc",
                "cbr",
                "-zerolatency",
                "1",
                "-delay",
                "0",
                "-rc-lookahead",
                "0",
                "-spatial-aq",
                "1",
            ],
            Self::Amf => &[
                "-usage",
                "ultralowlatency",
                "-quality",
                "speed",
                "-rc",
                "cbr",
            ],
            Self::X264 => &[
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-threads",
                "4",
            ],
        }
    }
}

pub struct Encoder {
    ffmpeg: PathBuf,
    codec: Codec,
}

#[derive(Clone, Copy)]
pub enum Quality {
    Auto,
    Smooth,
    Balanced,
    Sharp,
    Latency,
}

impl FromStr for Quality {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "smooth" => Ok(Self::Smooth),
            "balanced" => Ok(Self::Balanced),
            "sharp" => Ok(Self::Sharp),
            "latency" => Ok(Self::Latency),
            _ => bail!("--quality 应为 auto、smooth、balanced、sharp 或 latency"),
        }
    }
}

impl Quality {
    pub fn resolve(self, udp: bool) -> Self {
        match self {
            Self::Auto if udp => Self::Balanced,
            Self::Auto => Self::Smooth,
            Self::Latency if !udp => Self::Smooth,
            selected => selected,
        }
    }

    fn limits(self) -> (f64, f64) {
        match self {
            Self::Smooth | Self::Latency => (1600.0, 900.0),
            _ => (1920.0, 1080.0),
        }
    }

    fn rate(self) -> (&'static str, &'static str) {
        match self {
            Self::Smooth => ("4500k", "180k"),
            Self::Sharp => ("10M", "400k"),
            Self::Latency => ("7M", "160k"),
            _ => ("6500k", "260k"),
        }
    }

    pub fn low_latency(self) -> bool {
        matches!(self, Self::Latency)
    }
}

impl Encoder {
    pub fn detect() -> Result<Self> {
        let ffmpeg = std::env::var_os("LAN_STREAM_FFMPEG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("ffmpeg.exe"));
        for codec in [Codec::Nvenc, Codec::Amf, Codec::X264] {
            let output = Command::new(&ffmpeg)
                .creation_flags(CREATE_NO_WINDOW)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=1280x720:rate=60",
                    "-frames:v",
                    "3",
                    "-c:v",
                    codec.name(),
                ])
                .args(codec.options())
                .args([
                    "-pix_fmt",
                    "yuv420p",
                    "-profile:v",
                    "baseline",
                    "-bf",
                    "0",
                    "-f",
                    "null",
                    "-",
                ])
                .output()
                .context("找不到 FFmpeg。请将 ffmpeg.exe 加入 PATH，或设置 LAN_STREAM_FFMPEG")?;
            if output.status.success() {
                return Ok(Self { ffmpeg, codec });
            }
        }
        bail!("FFmpeg 没有可用的 H.264 编码器")
    }

    pub fn name(&self) -> &'static str {
        self.codec.name()
    }

    pub fn start(
        &self,
        source: &Pixels,
        width: u32,
        height: u32,
        quality: Quality,
        fps: u32,
        rtp: Option<&str>,
    ) -> Result<Child> {
        let size = format!("{}x{}", source.width, source.height);
        let filters = format!("scale={width}:{height}:flags=fast_bilinear,format=yuv420p");
        let (bitrate, vbv) = quality.rate();
        let mut command = Command::new(&self.ffmpeg);
        command
            .creation_flags(CREATE_NO_WINDOW)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "rawvideo",
                "-pixel_format",
                "bgra",
                "-video_size",
                &size,
                "-framerate",
                &fps.to_string(),
                "-probesize",
                "32",
                "-analyzeduration",
                "0",
                "-i",
                "pipe:0",
                "-an",
                "-filter_threads",
                "2",
                "-vf",
                &filters,
                "-c:v",
                self.codec.name(),
            ])
            .args(self.codec.options())
            .args([
                "-profile:v",
                "baseline",
                "-b:v",
                bitrate,
                "-maxrate",
                bitrate,
                "-bufsize",
                vbv,
                "-bf",
                "0",
                "-g",
                &(fps / 4).to_string(),
                "-flush_packets",
                "1",
            ]);
        if let Some(endpoint) = rtp {
            command
                .args([
                    "-f",
                    "rtp",
                    "-payload_type",
                    "96",
                    "-rtpflags",
                    "skip_rtcp",
                    endpoint,
                ])
                .stdout(Stdio::null());
        } else {
            command
                .args(["-bsf:v", "h264_metadata=aud=insert", "-f", "h264", "pipe:1"])
                .stdout(Stdio::piped());
        }
        command
            .stdin(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("启动 H.264 编码失败")
    }
}

pub fn output_size(width: u32, height: u32, quality: Quality) -> (u32, u32) {
    let (cap_w, cap_h) = quality.limits();
    let scale = (cap_w / f64::from(width))
        .min(cap_h / f64::from(height))
        .min(1.0);
    (
        ((f64::from(width) * scale).round() as u32) & !1,
        ((f64::from(height) * scale).round() as u32) & !1,
    )
}

pub fn feed(
    mut output: impl Write,
    slot: &FrameSlot,
    first: &Pixels,
    stop: &AtomicBool,
    fps: u32,
) -> Result<()> {
    let interval = Duration::from_nanos(1_000_000_000 / u64::from(fps));
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let frame = slot.latest().context("游戏窗口已关闭")?;
        if (frame.width, frame.height) != (first.width, first.height) {
            bail!("游戏窗口尺寸已变化，请重新连接");
        }
        output.write_all(&frame.bgra)?;
        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else if now - next > interval {
            next = now;
        }
    }
    Ok(())
}

pub fn access_units(
    mut input: impl Read,
    mut emit: impl FnMut(Vec<u8>) -> Result<()>,
) -> Result<()> {
    let mut pending = Vec::with_capacity(64 * 1024);
    let mut read = [0; 16 * 1024];
    let mut found_first = false;
    let mut scan_from = 0;
    loop {
        let length = input.read(&mut read)?;
        if length == 0 {
            break;
        }
        pending.extend_from_slice(&read[..length]);
        while let Some(offset) = next_aud(&pending, scan_from) {
            if found_first {
                let tail = pending.split_off(offset);
                emit(std::mem::replace(&mut pending, tail))?;
                scan_from = 4;
            } else {
                found_first = true;
                scan_from = offset + 4;
            }
        }
        scan_from = scan_from.max(pending.len().saturating_sub(4));
        if pending.len() > stream_wire::MAX_MEDIA {
            bail!("H.264 帧长度异常");
        }
    }
    if found_first && !pending.is_empty() {
        emit(pending)?;
    }
    Ok(())
}

fn next_aud(bytes: &[u8], from: usize) -> Option<usize> {
    for index in from..bytes.len().saturating_sub(3) {
        if bytes[index..index + 3] != [0, 0, 1] || bytes[index + 3] & 31 != 9 {
            continue;
        }
        return Some(if index > 0 && bytes[index - 1] == 0 {
            index - 1
        } else {
            index
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_delimiters_split_pictures() {
        let bytes = [
            0, 0, 0, 1, 9, 16, 0, 0, 1, 7, 32, 0, 0, 1, 5, 55, 0, 0, 0, 1, 9, 32, 0, 0, 1, 1, 67,
        ];
        let mut pictures = Vec::new();
        access_units(bytes.as_slice(), |picture| {
            pictures.push(picture);
            Ok(())
        })
        .unwrap();
        assert_eq!(pictures, vec![bytes[..16].to_vec(), bytes[16..].to_vec()]);
        assert_eq!(output_size(2560, 1600, Quality::Balanced), (1728, 1080));
        assert_eq!(output_size(2800, 1260, Quality::Balanced), (1920, 864));
        assert_eq!(output_size(1920, 864, Quality::Smooth), (1600, 720));
    }
}
