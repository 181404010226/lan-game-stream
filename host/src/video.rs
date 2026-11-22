use crate::content::BlackBars;
use crate::window::{Target, capture_crop};
use anyhow::{Context as _, Result, anyhow, bail};
use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use stream_wire::ContentInsets;
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
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
    since_last_frame: Instant,
    bars: Option<BlackBars>,
    last_sample: Instant,
}

impl GraphicsCaptureApiHandler for WindowGrab {
    type Flags = (Target, Arc<FrameSlot>, bool);
    type Error = CaptureError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let (target, slot, inspect) = ctx.flags;
        Ok(Self {
            target,
            slot,
            since_last_frame: Instant::now() - Duration::from_secs(1),
            bars: inspect.then(BlackBars::default),
            last_sample: Instant::now() - Duration::from_secs(1),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.since_last_frame.elapsed() < Duration::from_millis(14) {
            return Ok(());
        }
        self.since_last_frame = Instant::now();
        let (left, top, right, bottom) =
            capture_crop(self.target.hwnd(), frame.width(), frame.height());
        if right < left + 64 || bottom < top + 64 {
            return Ok(());
        }
        let width = right - left;
        let height = bottom - top;
        let mut mapped = frame.buffer_crop(left, top, right, bottom)?;
        let stride = mapped.row_pitch() as usize;
        let row_bytes = width as usize * 4;
        let mut bgra = Vec::with_capacity(row_bytes * height as usize);
        for row in mapped
            .as_raw_buffer()
            .chunks_exact(stride)
            .take(height as usize)
        {
            bgra.extend_from_slice(&row[..row_bytes]);
        }
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

pub struct Capture {
    pub slot: Arc<FrameSlot>,
    control: Option<CaptureControl<WindowGrab, CaptureError>>,
}

impl Capture {
    pub fn start(target: &Target, content_updates: bool) -> Result<Self> {
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
            (target.clone(), slot.clone(), content_updates),
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

    pub fn start(&self, source: &Pixels, width: u32, height: u32) -> Result<Child> {
        let size = format!("{}x{}", source.width, source.height);
        let filters = format!("scale={width}:{height}:flags=fast_bilinear,format=yuv420p");
        let bitrate = if width * height > 1_400_000 {
            "8M"
        } else {
            "5M"
        };
        Command::new(&self.ffmpeg)
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
                "60",
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
                "320k",
                "-bf",
                "0",
                "-g",
                "30",
                "-bsf:v",
                "h264_metadata=aud=insert",
                "-flush_packets",
                "1",
                "-f",
                "h264",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("启动 H.264 编码失败")
    }
}

pub fn output_size(width: u32, height: u32) -> (u32, u32) {
    let scale = (1920.0 / f64::from(width))
        .min(1080.0 / f64::from(height))
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
) -> Result<()> {
    let interval = Duration::from_nanos(1_000_000_000 / u64::from(FPS));
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
        assert_eq!(output_size(2560, 1600), (1728, 1080));
        assert_eq!(output_size(2800, 1260), (1920, 864));
    }
}
