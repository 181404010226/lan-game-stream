mod audio;
mod content;
mod fast_video;
mod gamepad;
mod gpu_video;
mod input;
mod network;
mod rtp;
mod video;
mod window;

use anyhow::{Context, Result, anyhow, bail};
use gamepad::Gamepad;
use input::GameInput;
use network::Interface;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufReader, BufWriter, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, Shutdown, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use stream_wire::PAD_BUTTONS;
use stream_wire::{
    AUTH, CHALLENGE, CONTENT, ContentInsets, ERROR, GAMEPAD, Hello, INFO, KEEPALIVE, KEYS,
    MAX_CONTROL, PING, POINTER, PONG, PORT, PadState, Secret, StreamInfo, VIDEO, discovery_game,
    nonce, parse_secret, receive, send, sign, valid_id, verifies,
};
use video::{Capture, Encoder, Quality};
use window::{Target, WindowResize};

struct Options {
    config: PathBuf,
    bind: Option<Ipv4Addr>,
    port: u16,
    list: bool,
    quality: Quality,
}

fn options() -> Result<Option<Options>> {
    let mut result = Options {
        config: std::env::current_exe()?.with_file_name("lan-stream-games.json"),
        bind: None,
        port: PORT,
        list: false,
        quality: Quality::Auto,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list" => result.list = true,
            "--config" => result.config = args.next().context("--config 缺少配置路径")?.into(),
            "--bind" => result.bind = Some(args.next().context("--bind 缺少 IPv4 地址")?.parse()?),
            "--port" => result.port = args.next().context("--port 缺少端口")?.parse()?,
            "--quality" => result.quality = args.next().context("--quality 缺少设置")?.parse()?,
            "--help" | "-h" => {
                println!(
                    "局域网游戏串流主机\n\nlan-stream-host.exe [--config 游戏配置路径] [--bind 局域网 IPv4] [--port 24865] [--quality auto|smooth|balanced|sharp|latency]\nlan-stream-host.exe --list"
                );
                return Ok(None);
            }
            _ => bail!("未知选项 {arg}，请使用 --help 查看用法"),
        }
    }
    Ok(Some(result))
}

#[derive(Deserialize)]
struct Entry {
    id: String,
    title: String,
    exe: PathBuf,
    key: String,
    #[serde(default)]
    xbox: bool,
}

struct Game {
    title: String,
    exe: PathBuf,
    key: Secret,
    xbox: bool,
}

fn games(path: &PathBuf) -> Result<HashMap<String, Game>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取游戏配置失败：{}", path.display()))?;
    let entries: Vec<Entry> =
        serde_json::from_str(text.trim_start_matches('\u{feff}')).context("游戏配置格式错误")?;
    let mut result = HashMap::new();
    for entry in entries {
        if !valid_id(&entry.id) || entry.title.trim().is_empty() || !entry.exe.is_absolute() {
            bail!("游戏配置含有无效标识、标题或 EXE 路径");
        }
        let key = parse_secret(&entry.key).context("游戏配置中有无效的配对密钥")?;
        if result
            .insert(
                entry.id,
                Game {
                    title: entry.title,
                    exe: entry.exe,
                    key,
                    xbox: entry.xbox,
                },
            )
            .is_some()
        {
            bail!("游戏配置中含有重复标识");
        }
    }
    if result.is_empty() {
        bail!("还没有游戏绑定，请先构建专用 APK");
    }
    Ok(result)
}

fn run() -> Result<()> {
    window::enable_physical_pixels();
    let Some(opts) = options()? else {
        return Ok(());
    };
    if opts.list {
        for target in window::list()? {
            println!(
                "{} | {} | {}",
                target.game_id,
                target.title,
                target.exe.display()
            );
        }
        return Ok(());
    }
    let games = Arc::new(games(&opts.config)?);
    let encoder = Encoder::detect()?;
    let interface = Interface::pick(opts.bind)?;
    let listener = TcpListener::bind((interface.address, opts.port))
        .context("监听失败，检查端口是否被另一个主机占用")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let discovery =
        UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).context("无法开启局域网自动发现")?;
    discovery.set_read_timeout(Some(Duration::from_millis(500)))?;
    let stop = Arc::new(AtomicBool::new(false));
    let cancel = stop.clone();
    ctrlc::set_handler(move || cancel.store(true, Ordering::SeqCst))?;
    let discovery_games = games.clone();
    let discovery_stop = stop.clone();
    thread::spawn(move || discover(discovery, interface, &discovery_games, &discovery_stop));
    println!("\n局域网游戏串流  ·  Windows 主机");
    println!("地址     {}", listener.local_addr()?);
    for game in games.values() {
        println!("已绑定   {}", game.title);
    }
    println!("编码     {}", encoder.name());
    println!("连接后请让游戏窗口保持在前台。Ctrl+C 结束主机。\n");
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                if !matches!(peer.ip(), IpAddr::V4(ip) if interface.contains(ip)) {
                    continue;
                }
                if let Err(error) = session(stream, &games, &encoder, opts.quality, &stop) {
                    eprintln!("会话结束：{error:#}");
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(35))
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn discover(
    socket: UdpSocket,
    interface: Interface,
    games: &HashMap<String, Game>,
    stop: &AtomicBool,
) {
    let mut buffer = [0; 64];
    while !stop.load(Ordering::Relaxed) {
        let (size, peer) = match socket.recv_from(&mut buffer) {
            Ok(packet) => packet,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::ConnectionReset
                ) =>
            {
                continue;
            }
            Err(error) => {
                eprintln!("自动发现结束：{error}");
                break;
            }
        };
        if !matches!(peer.ip(), IpAddr::V4(address) if interface.contains(address)) {
            continue;
        }
        let packet = &buffer[..size];
        if let Some(game) = discovery_game(packet).and_then(|id| games.get(id)) {
            let reply = sign(&game.key, &[b"discover", packet]);
            let _ = socket.send_to(&reply, peer);
        }
    }
}

fn session(
    mut socket: TcpStream,
    games: &HashMap<String, Game>,
    encoder: &Encoder,
    quality: Quality,
    host_stop: &AtomicBool,
) -> Result<()> {
    socket.set_nonblocking(false)?;
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(Duration::from_secs(2)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let hello = Hello::read(&mut socket)?;
    let Some(game) = games.get(&hello.game) else {
        send(&mut socket, ERROR, "电脑主机尚未绑定这个游戏".as_bytes())?;
        return Ok(());
    };
    let client_hello = hello.encode();
    let challenge = nonce()?;
    let proof = sign(&game.key, &[b"host", &client_hello, &challenge]);
    send(
        &mut socket,
        CHALLENGE,
        &[challenge.as_slice(), &proof].concat(),
    )?;
    let mut auth = Vec::new();
    let response = receive(&mut socket, &mut auth, 32)?;
    if response != AUTH
        || !verifies(
            &game.key,
            &[b"phone", &client_hello, &challenge],
            &auth[1..],
        )
    {
        send(&mut socket, ERROR, "游戏配对未通过".as_bytes())?;
        return Ok(());
    }
    let lane = if hello.fast_video {
        fast_video::FastVideo::negotiate(&mut socket)?
    } else {
        None
    };
    let Some(target) = window::for_exe(&game.exe)? else {
        send(
            &mut socket,
            ERROR,
            format!("已连接电脑，等待打开《{}》。", game.title).as_bytes(),
        )?;
        return Ok(());
    };
    let pad = if game.xbox && hello.content_updates {
        match Gamepad::connect() {
            Ok(pad) => Some(pad),
            Err(error) => {
                send(&mut socket, ERROR, error.to_string().as_bytes())?;
                return Err(error);
            }
        }
    } else {
        None
    };
    let quality = quality.resolve(lane.is_some());
    let low_latency = quality.low_latency();
    let refresh = window::refresh_rate(&target);
    let fps = if low_latency && refresh >= 119 {
        120
    } else {
        video::FPS
    };
    let _resize = WindowResize::apply(
        &target,
        hello.viewport,
        if low_latency { 1600 } else { 1920 },
    );
    if _resize.is_some() {
        thread::sleep(Duration::from_millis(300));
    }
    let started = match start_video(&target, encoder, quality, fps, hello.content_updates) {
        Ok(started) => started,
        Err(error) => {
            send(&mut socket, ERROR, error.to_string().as_bytes())?;
            return Err(error);
        }
    };
    let (width, height) = started.size;
    let desired = f64::from(hello.viewport.0) / f64::from(hello.viewport.1);
    let axes = lane
        .as_ref()
        .filter(|_| pad.is_some())
        .and_then(|udp| udp.input().ok());
    let active = f64::from(width) / f64::from(height);
    let info = StreamInfo {
        game_id: target.game_id.clone(),
        title: game.title.clone(),
        width,
        height,
        fps,
        encoder: started.encoder.into(),
        phone_aspect: desired.is_finite() && (active / desired - 1.0).abs() < 0.02,
        udp_video: lane.is_some(),
        low_latency,
        udp_axes: axes.is_some(),
    };
    stream(
        &socket,
        &target,
        started.capture,
        &info,
        StreamFeatures {
            content_updates: hello.content_updates,
            compact_audio: hello.fast_video,
            lane,
            axes,
            pad,
        },
        host_stop,
    )
}

struct StartedVideo {
    size: (u32, u32),
    encoder: &'static str,
    capture: VideoCapture,
}

enum VideoCapture {
    Gpu(gpu_video::GpuVideo),
    Ffmpeg {
        capture: Capture,
        first: Arc<video::Pixels>,
        child: Option<std::process::Child>,
        rtp: Option<rtp::RtpVideo>,
    },
}

impl VideoCapture {
    fn content(&self) -> ContentInsets {
        match self {
            Self::Gpu(gpu) => gpu.content(),
            Self::Ffmpeg { capture, .. } => capture.slot.content(),
        }
    }
}

fn start_video(
    target: &Target,
    encoder: &Encoder,
    quality: Quality,
    fps: u32,
    bars: bool,
) -> Result<StartedVideo> {
    if quality.low_latency() && encoder.name() == "h264_nvenc" {
        match gpu_video::GpuVideo::open(target, fps, quality, bars) {
            Ok(gpu) => {
                println!("D3D11 纹理直送 NVENC · {}", gpu.adapter);
                return Ok(StartedVideo {
                    size: gpu.size,
                    encoder: gpu_video::GpuVideo::ENCODER,
                    capture: VideoCapture::Gpu(gpu),
                });
            }
            Err(error) => eprintln!("D3D11 直送暂不可用，使用 FFmpeg：{error:#}"),
        }
    }
    let capture = Capture::start(target, bars, fps)?;
    let first = capture.slot.first()?;
    let size = video::output_size(first.width, first.height, quality);
    let rtp = if quality.low_latency() {
        rtp::RtpVideo::bind().ok()
    } else {
        None
    };
    let endpoint = rtp.as_ref().map(rtp::RtpVideo::endpoint).transpose()?;
    let child = encoder.start(&first, size.0, size.1, quality, fps, endpoint.as_deref())?;
    Ok(StartedVideo {
        size,
        encoder: encoder.name(),
        capture: VideoCapture::Ffmpeg {
            capture,
            first,
            child: Some(child),
            rtp,
        },
    })
}

struct StreamFeatures {
    content_updates: bool,
    compact_audio: bool,
    lane: Option<fast_video::FastVideo>,
    axes: Option<fast_video::FastInput>,
    pad: Option<Gamepad>,
}

fn stream(
    socket: &TcpStream,
    target: &Target,
    mut capture: VideoCapture,
    info: &StreamInfo,
    features: StreamFeatures,
    host_stop: &AtomicBool,
) -> Result<()> {
    let mut output = BufWriter::with_capacity(128 * 1024, socket);
    send(&mut output, INFO, &serde_json::to_vec(info)?)?;
    println!(
        "已连接 {} · {}×{} {} fps · {} · {}",
        socket.peer_addr()?,
        info.width,
        info.height,
        info.fps,
        if info.phone_aspect {
            "窗口匹配手机画幅"
        } else {
            "游戏画幅"
        },
        if info.udp_video { "UDP/FEC" } else { "TCP" }
    );
    let (mut child, local_rtp) = match &mut capture {
        VideoCapture::Ffmpeg { child, rtp, .. } => (child.take(), rtp.take()),
        VideoCapture::Gpu(_) => (None, None),
    };
    let stdin = child.as_mut().and_then(|encoder| encoder.stdin.take());
    let stdout = child.as_mut().and_then(|encoder| encoder.stdout.take());
    let stop = AtomicBool::new(false);
    let encoded_frames = AtomicU32::new(0);
    let StreamFeatures {
        content_updates,
        compact_audio,
        lane,
        axes,
        pad,
    } = features;
    let (tx, rx) = sync_channel::<(u8, Vec<u8>)>(12);
    thread::scope(|group| {
        let stop_ref = &stop;
        let acknowledgement = tx.clone();
        let pad = pad.map(|pad| Arc::new(Mutex::new(pad)));
        if let (Some(axes), Some(input)) = (axes, pad.clone()) {
            group.spawn(move || axes.receive(target, &input, stop_ref));
        }
        group.spawn(move || read_controls(socket, target, pad, stop_ref, acknowledgement));
        let status = tx.clone();
        if let (VideoCapture::Ffmpeg { capture, first, .. }, Some(stdin)) = (&capture, stdin) {
            group.spawn(move || {
                if let Err(error) = video::feed(stdin, &capture.slot, first, stop_ref, info.fps)
                    && !stop_ref.load(Ordering::Relaxed)
                {
                    let _ = status.try_send((ERROR, error.to_string().into_bytes()));
                }
            });
        }
        let audio_sender = tx.clone();
        group.spawn(|| {
            if let Err(error) = audio::capture(target.pid, &stop, audio_sender, compact_audio) {
                eprintln!("游戏进程音频不可用：{error:#}");
            }
        });
        let count = &encoded_frames;
        let capture_source = &capture;
        group.spawn(move || {
            let emit = |index: Option<u32>, encoded: Vec<u8>| {
                if stop_ref.load(Ordering::Relaxed) {
                    bail!("会话已结束");
                }
                let sequence = count.fetch_add(1, Ordering::Relaxed);
                if let Some(lane) = &lane {
                    lane.picture(index.unwrap_or(sequence), &encoded)?;
                } else {
                    tx.send((VIDEO, encoded))
                        .map_err(|_| anyhow!("会话已结束"))?;
                }
                Ok(())
            };
            let result = match (capture_source, local_rtp, stdout) {
                (VideoCapture::Gpu(gpu), _, _) => {
                    gpu.pump(stop_ref, |frame| emit(Some(frame.index), frame.bytes))
                }
                (_, Some(rtp), _) => rtp.access_units(stop_ref, |frame| emit(None, frame)),
                (_, None, Some(stdout)) => video::access_units(stdout, |frame| emit(None, frame)),
                _ => Err(anyhow!("编码器没有输出流")),
            };
            if !stop_ref.load(Ordering::Relaxed) {
                let text = result
                    .err()
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "H.264 编码已结束".into());
                let _ = tx.send((ERROR, text.into_bytes()));
            }
        });
        let mut last_video = Instant::now();
        let mut previous_count = 0;
        let mut last_update = Instant::now();
        let mut keepalive = Instant::now();
        let mut content = ContentInsets::default();
        let result = loop {
            if stop.load(Ordering::Relaxed) || host_stop.load(Ordering::Relaxed) {
                break Ok(());
            }
            if !target.exists()
                || matches!(&capture, VideoCapture::Ffmpeg { capture, .. } if capture.ended())
            {
                break send(&mut output, ERROR, "游戏窗口已关闭".as_bytes());
            }
            let count = encoded_frames.load(Ordering::Relaxed);
            if count != previous_count {
                previous_count = count;
                last_video = Instant::now();
            }
            if last_video.elapsed() > Duration::from_secs(7) {
                break send(&mut output, ERROR, "接收游戏画面超时".as_bytes());
            }
            if compact_audio && keepalive.elapsed() >= Duration::from_secs(1) {
                keepalive = Instant::now();
                if let Err(error) = send(&mut output, KEEPALIVE, &[]) {
                    break Err(error);
                }
            }
            if content_updates && last_update.elapsed() >= Duration::from_millis(100) {
                last_update = Instant::now();
                let measured = capture.content();
                if measured != content {
                    content = measured;
                    if let Err(error) = send(&mut output, CONTENT, &content.encode()) {
                        break Err(error);
                    }
                }
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok((kind, data)) => {
                    if let Err(error) = send(&mut output, kind, &data) {
                        break Err(error);
                    }
                    if kind == ERROR {
                        break Ok(());
                    }
                    if kind == VIDEO {
                        last_video = Instant::now();
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break Ok(()),
            }
        };
        stop.store(true, Ordering::Relaxed);
        let _ = socket.shutdown(Shutdown::Both);
        drop(rx);
        if let VideoCapture::Gpu(gpu) = &capture {
            gpu.stop();
        }
        if let Some(process) = &mut child {
            let _ = process.kill();
            let _ = process.wait();
        }
        println!("手机已断开");
        result.context("连接已结束")
    })
}

fn read_controls(
    socket: &TcpStream,
    target: &Target,
    pad: Option<Arc<Mutex<Gamepad>>>,
    stop: &AtomicBool,
    acknowledgement: SyncSender<(u8, Vec<u8>)>,
) {
    let mut controls = GameInput::new(target);
    let mut input = BufReader::new(socket);
    let mut buffer = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        match receive(&mut input, &mut buffer, MAX_CONTROL) {
            Ok(KEYS) => controls.apply_keys(&buffer[1..]),
            Ok(POINTER) => controls.pointer(&buffer[1..]),
            Ok(PING) if buffer.len() == 9 => {
                let _ = acknowledgement.try_send((PONG, buffer[1..].to_vec()));
            }
            Ok(PAD_BUTTONS) if buffer.len() == 3 => {
                if let Some(pad) = &pad
                    && let Err(error) = pad.lock().unwrap().buttons(
                        u16::from_be_bytes([buffer[1], buffer[2]]),
                        target.foreground(),
                    )
                {
                    eprintln!("{error:#}");
                    break;
                }
            }
            Ok(GAMEPAD) => {
                if let (Some(controller), Some(state)) = (&pad, PadState::decode(&buffer[1..])) {
                    let state = if target.foreground() {
                        state
                    } else {
                        PadState::default()
                    };
                    if let Err(error) = controller.lock().unwrap().update(state) {
                        eprintln!("{error:#}");
                        break;
                    }
                }
            }
            Ok(_) | Err(_) => break,
        }
    }
    stop.store(true, Ordering::Relaxed);
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        if std::env::args().len() == 1 {
            eprintln!("按回车关闭窗口");
            let _ = std::io::stdin().read_line(&mut String::new());
        }
        std::process::exit(1);
    }
}
