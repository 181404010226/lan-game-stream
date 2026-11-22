mod audio;
mod content;
mod gamepad;
mod input;
mod network;
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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::thread;
use std::time::{Duration, Instant};
use stream_wire::{
    AUTH, CHALLENGE, CONTENT, ContentInsets, ERROR, GAMEPAD, Hello, INFO, KEYS, MAX_CONTROL,
    POINTER, PORT, PadState, Secret, StreamInfo, VIDEO, discovery_game, nonce, parse_secret,
    receive, send, sign, valid_id, verifies,
};
use video::{Capture, Encoder};
use window::{Target, WindowResize};

struct Options {
    config: PathBuf,
    bind: Option<Ipv4Addr>,
    port: u16,
    list: bool,
}

fn options() -> Result<Option<Options>> {
    let mut result = Options {
        config: std::env::current_exe()?.with_file_name("lan-stream-games.json"),
        bind: None,
        port: PORT,
        list: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list" => result.list = true,
            "--config" => result.config = args.next().context("--config 缺少配置路径")?.into(),
            "--bind" => result.bind = Some(args.next().context("--bind 缺少 IPv4 地址")?.parse()?),
            "--port" => result.port = args.next().context("--port 缺少端口")?.parse()?,
            "--help" | "-h" => {
                println!(
                    "局域网游戏串流主机\n\nlan-stream-host.exe [--config 游戏配置路径] [--bind 局域网 IPv4] [--port 24865]\nlan-stream-host.exe --list"
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
    println!("编码     {} / 目标 {} fps", encoder.name(), video::FPS);
    println!("连接后请让游戏窗口保持在前台。Ctrl+C 结束主机。\n");
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                if !matches!(peer.ip(), IpAddr::V4(ip) if interface.contains(ip)) {
                    continue;
                }
                if let Err(error) = session(stream, &games, &encoder, &stop) {
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
    let _resize = WindowResize::apply(&target, hello.viewport);
    if _resize.is_some() {
        thread::sleep(Duration::from_millis(300));
    }
    let capture = match Capture::start(&target, hello.content_updates) {
        Ok(capture) => capture,
        Err(error) => {
            send(&mut socket, ERROR, error.to_string().as_bytes())?;
            return Err(error);
        }
    };
    let first = match capture.slot.first() {
        Ok(frame) => frame,
        Err(error) => {
            send(&mut socket, ERROR, error.to_string().as_bytes())?;
            return Err(error);
        }
    };
    let (width, height) = video::output_size(first.width, first.height);
    let mut child = match encoder.start(&first, width, height) {
        Ok(child) => child,
        Err(error) => {
            send(&mut socket, ERROR, error.to_string().as_bytes())?;
            return Err(error);
        }
    };
    let desired = f64::from(hello.viewport.0) / f64::from(hello.viewport.1);
    let active = f64::from(width) / f64::from(height);
    let info = StreamInfo {
        game_id: target.game_id.clone(),
        title: game.title.clone(),
        width,
        height,
        fps: video::FPS,
        encoder: encoder.name().into(),
        phone_aspect: desired.is_finite() && (active / desired - 1.0).abs() < 0.02,
    };
    let result = stream(
        &socket,
        &target,
        &capture,
        &first,
        &mut child,
        &info,
        hello.content_updates,
        pad,
        host_stop,
    );
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn stream(
    socket: &TcpStream,
    target: &Target,
    capture: &Capture,
    first: &video::Pixels,
    child: &mut std::process::Child,
    info: &StreamInfo,
    content_updates: bool,
    pad: Option<Gamepad>,
    host_stop: &AtomicBool,
) -> Result<()> {
    let mut output = BufWriter::with_capacity(128 * 1024, socket);
    send(&mut output, INFO, &serde_json::to_vec(info)?)?;
    println!(
        "已连接 {} · {}×{} · {}",
        socket.peer_addr()?,
        info.width,
        info.height,
        if info.phone_aspect {
            "窗口匹配手机画幅"
        } else {
            "游戏画幅"
        }
    );
    let stdin = child.stdin.take().context("编码器没有输入流")?;
    let stdout = child.stdout.take().context("编码器没有输出流")?;
    let stop = AtomicBool::new(false);
    let (tx, rx) = sync_channel::<(u8, Vec<u8>)>(12);
    thread::scope(|group| {
        let stop_ref = &stop;
        group.spawn(move || read_controls(socket, target, pad, stop_ref));
        let status = tx.clone();
        group.spawn(move || {
            if let Err(error) = video::feed(stdin, &capture.slot, first, stop_ref)
                && !stop_ref.load(Ordering::Relaxed)
            {
                let _ = status.try_send((ERROR, error.to_string().into_bytes()));
            }
        });
        let audio_sender = tx.clone();
        group.spawn(|| {
            if let Err(error) = audio::capture(target.pid, &stop, audio_sender) {
                eprintln!("游戏进程音频不可用：{error:#}");
            }
        });
        group.spawn(move || {
            let result = video::access_units(stdout, |encoded| {
                if stop_ref.load(Ordering::Relaxed) {
                    bail!("会话已结束");
                }
                tx.send((VIDEO, encoded))
                    .map_err(|_| anyhow!("会话已结束"))?;
                Ok(())
            });
            if !stop_ref.load(Ordering::Relaxed) {
                let text = result
                    .err()
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "H.264 编码已结束".into());
                let _ = tx.send((ERROR, text.into_bytes()));
            }
        });
        let mut last_video = Instant::now();
        let mut last_update = Instant::now();
        let mut content = ContentInsets::default();
        let result = loop {
            if stop.load(Ordering::Relaxed) || host_stop.load(Ordering::Relaxed) {
                break Ok(());
            }
            if !target.exists() || capture.ended() {
                break send(&mut output, ERROR, "游戏窗口已关闭".as_bytes());
            }
            if last_video.elapsed() > Duration::from_secs(7) {
                break send(&mut output, ERROR, "接收游戏画面超时".as_bytes());
            }
            if content_updates && last_update.elapsed() >= Duration::from_millis(100) {
                last_update = Instant::now();
                let measured = capture.slot.content();
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
        let _ = child.kill();
        println!("手机已断开");
        result.context("连接已结束")
    })
}

fn read_controls(socket: &TcpStream, target: &Target, mut pad: Option<Gamepad>, stop: &AtomicBool) {
    let mut controls = GameInput::new(target);
    let mut input = BufReader::new(socket);
    let mut buffer = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        match receive(&mut input, &mut buffer, MAX_CONTROL) {
            Ok(KEYS) => controls.apply_keys(&buffer[1..]),
            Ok(POINTER) => controls.pointer(&buffer[1..]),
            Ok(GAMEPAD) => {
                if let (Some(controller), Some(state)) = (&mut pad, PadState::decode(&buffer[1..]))
                {
                    let state = if target.foreground() {
                        state
                    } else {
                        PadState::default()
                    };
                    if let Err(error) = controller.update(state) {
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
