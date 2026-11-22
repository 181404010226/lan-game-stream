use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JIntArray, JString};
use jni::sys::{jboolean, jbyteArray, jint, jlong, jstring};
use socket2::Socket;
use std::collections::HashMap;
use std::io::{self, BufReader};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpStream, UdpSocket};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use stream_wire::{
    AUDIO, AUDIO_COMPACT, AUTH, CHALLENGE, ERROR, GAMEPAD, Hello, INFO, KEYS, MAX_CONTROL,
    MAX_MEDIA, POINTER, PadState, Secret, UDP_OFFER, UDP_READY, discovery_request, nonce,
    parse_secret, receive, send, sign, valid_id, verifies,
};

struct Reader {
    socket: BufReader<TcpStream>,
    buffer: Vec<u8>,
    pcm: Vec<u8>,
}

struct Client {
    reader: Mutex<Reader>,
    control: TcpStream,
    commands: SyncSender<(u8, Vec<u8>)>,
    info: String,
    datagrams: Option<Mutex<Datagrams>>,
    active: AtomicBool,
    axes: Option<axes::AxesSender>,
}

impl Client {
    fn connect(
        address_hint: Option<Ipv4Addr>,
        port: u16,
        game: String,
        secret: Secret,
        viewport: (u16, u16),
        fast_video: bool,
    ) -> io::Result<Self> {
        let request = discovery_request(&game, nonce()?);
        let ip = discover(address_hint, port, &request, &secret)
            .or(address_hint)
            .ok_or_else(|| io::Error::other("等待电脑主机，正在局域网中查找。"))?;
        let mut socket =
            TcpStream::connect_timeout(&SocketAddr::from((ip, port)), Duration::from_millis(1250))
                .map_err(|_| io::Error::other("等待电脑主机，正在局域网中查找。"))?;
        socket.set_nodelay(true)?;
        socket.set_read_timeout(Some(Duration::from_secs(3)))?;
        socket.set_write_timeout(Some(Duration::from_millis(750)))?;
        let hello = Hello {
            game,
            nonce: nonce()?,
            viewport,
            content_updates: true,
            fast_video,
        };
        hello.write(&mut socket)?;
        let mut reader = Reader {
            socket: BufReader::with_capacity(64 * 1024, socket),
            buffer: Vec::new(),
            pcm: Vec::new(),
        };
        let kind = receive(&mut reader.socket, &mut reader.buffer, 8192)?;
        if kind == ERROR {
            return Err(io::Error::other(error_text(&reader.buffer)));
        }
        if kind != CHALLENGE || reader.buffer.len() != 49 {
            return Err(io::Error::other("电脑端串流协议版本不匹配"));
        }
        let hello_bytes = hello.encode();
        let challenge = &reader.buffer[1..17];
        if !verifies(
            &secret,
            &[b"host", &hello_bytes, challenge],
            &reader.buffer[17..],
        ) {
            return Err(io::Error::other("该主机未与这个 APK 配对"));
        }
        let proof = sign(&secret, &[b"phone", &hello_bytes, challenge]);
        send(&mut reader.socket.get_ref(), AUTH, &proof)?;
        reader
            .socket
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(9)))?;
        let mut kind = receive(&mut reader.socket, &mut reader.buffer, 8192)?;
        let datagrams = if kind == UDP_OFFER && fast_video {
            let lane = Datagrams::accept(ip, &reader.buffer[1..]);
            send(
                &mut reader.socket.get_ref(),
                UDP_READY,
                &[u8::from(lane.is_some())],
            )?;
            kind = receive(&mut reader.socket, &mut reader.buffer, 8192)?;
            lane
        } else {
            None
        };
        let info = error_text(&reader.buffer);
        if kind == ERROR {
            return Err(io::Error::other(info));
        }
        if kind != INFO {
            return Err(io::Error::other("主机返回了未知协议"));
        }
        reader
            .socket
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(5)))?;
        let control = reader.socket.get_ref().try_clone()?;
        let mut writer = control.try_clone()?;
        let (commands, queue) = sync_channel::<(u8, Vec<u8>)>(64);
        std::thread::spawn(move || {
            while let Ok((kind, payload)) = queue.recv() {
                if send(&mut writer, kind, &payload).is_err() {
                    let _ = writer.shutdown(Shutdown::Both);
                    break;
                }
            }
        });
        let axes = serde_json::from_str::<stream_wire::StreamInfo>(&info)
            .ok()
            .filter(|info| info.udp_axes)
            .and_then(|_| datagrams.as_ref())
            .and_then(|lane| {
                lane.socket
                    .try_clone()
                    .ok()
                    .and_then(|socket| axes::AxesSender::new(socket, lane.cookie).ok())
            });
        Ok(Self {
            reader: Mutex::new(reader),
            control,
            commands,
            info,
            datagrams: datagrams.map(Mutex::new),
            active: AtomicBool::new(true),
            axes,
        })
    }

    fn queue(&self, kind: u8, payload: Vec<u8>) {
        if let Err(TrySendError::Full((kind, bytes))) = self.commands.try_send((kind, payload))
            && (kind != POINTER || bytes.first() != Some(&0))
        {
            let _ = self.control.shutdown(Shutdown::Both);
        }
    }
}

struct Datagrams {
    socket: UdpSocket,
    cookie: [u8; 8],
    video: stream_wire::udp::Assembler,
    last_picture: Option<Instant>,
}

impl Datagrams {
    fn accept(ip: Ipv4Addr, offer: &[u8]) -> Option<Self> {
        let [port_hi, port_low, rest @ ..] = offer else {
            return None;
        };
        let cookie: [u8; 8] = rest.try_into().ok()?;
        let port = u16::from_be_bytes([*port_hi, *port_low]);
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
        let tuned = Socket::from(socket);
        let _ = tuned.set_recv_buffer_size(2 * 1024 * 1024);
        let socket: UdpSocket = tuned.into();
        socket.connect((ip, port)).ok()?;
        socket
            .set_read_timeout(Some(Duration::from_millis(215)))
            .ok()?;
        socket.send(&cookie).ok()?;
        let mut reply = [0; 16];
        let length = socket.recv(&mut reply).ok()?;
        if reply[..length] != cookie {
            return None;
        }
        socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .ok()?;
        Some(Self {
            socket,
            cookie,
            video: stream_wire::udp::Assembler::new(cookie, Duration::from_millis(42)),
            last_picture: None,
        })
    }

    fn read(&mut self, active: &AtomicBool) -> io::Result<Option<stream_wire::udp::Picture>> {
        let mut payload = [0_u8; stream_wire::udp::MAX_DATAGRAM + 1];
        let started = *self.last_picture.get_or_insert_with(Instant::now);
        while active.load(Ordering::Relaxed) {
            let now = Instant::now();
            if let Some(picture) = self.video.ready(now) {
                self.last_picture = Some(now);
                return Ok(Some(picture));
            }
            if now.duration_since(started) > Duration::from_millis(1250) {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "UDP 视频超时"));
            }
            match self.socket.recv(&mut payload) {
                Ok(size) => self.video.packet(&payload[..size], Instant::now()),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }
}

fn error_text(buffer: &[u8]) -> String {
    String::from_utf8_lossy(&buffer[1..]).into_owned()
}

fn discover(hint: Option<Ipv4Addr>, port: u16, packet: &[u8], secret: &Secret) -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.set_broadcast(true).ok()?;
    if let Some(ip) = hint {
        let _ = socket.send_to(packet, (ip, port));
    }
    let _ = socket.send_to(packet, (Ipv4Addr::BROADCAST, port));
    let deadline = Instant::now() + Duration::from_millis(530);
    let mut reply = [0; 64];
    while let Some(remaining) = deadline
        .checked_duration_since(Instant::now())
        .filter(|time| !time.is_zero())
    {
        socket.set_read_timeout(Some(remaining)).ok()?;
        let (size, peer) = socket.recv_from(&mut reply).ok()?;
        if let IpAddr::V4(ip) = peer.ip()
            && (ip.is_private() || ip.is_loopback())
            && peer.port() == port
            && verifies(secret, &[b"discover", packet], &reply[..size])
        {
            return Some(ip);
        }
    }
    None
}

static NEXT_ID: AtomicI64 = AtomicI64::new(1);
static CLIENTS: OnceLock<Mutex<HashMap<i64, Arc<Client>>>> = OnceLock::new();

fn clients() -> &'static Mutex<HashMap<i64, Arc<Client>>> {
    CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn client(id: jlong) -> Option<Arc<Client>> {
    clients().lock().ok()?.get(&id).cloned()
}

fn throw(env: &mut JNIEnv<'_>, message: impl ToString) {
    let _ = env.throw_new("java/io/IOException", message.to_string());
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_lanstream_mobile_NativeBridge_open(
    mut env: JNIEnv,
    _class: JClass,
    host_hint: JString,
    port: jint,
    game: JString,
    key: JString,
    width: jint,
    height: jint,
    fast_video: jboolean,
) -> jlong {
    let get = |env: &mut JNIEnv<'_>, string: &JString<'_>| -> io::Result<String> {
        env.get_string(string)
            .map(Into::into)
            .map_err(|_| io::Error::other("无效的连接参数"))
    };
    let connect = (|| {
        let hint = get(&mut env, &host_hint)?
            .parse::<Ipv4Addr>()
            .ok()
            .filter(|ip| ip.is_private() || ip.is_loopback());
        let game = get(&mut env, &game)?;
        if !valid_id(&game) {
            return Err(io::Error::other("APK 未绑定游戏"));
        }
        let secret = parse_secret(&get(&mut env, &key)?)
            .ok_or_else(|| io::Error::other("APK 未绑定主机"))?;
        let size = (width.clamp(0, 8192) as u16, height.clamp(0, 8192) as u16);
        let port = u16::try_from(port)
            .ok()
            .filter(|port| *port > 0)
            .ok_or_else(|| io::Error::other("端口无效"))?;
        let fast = fast_video != 0;
        match Client::connect(hint, port, game.clone(), secret, size, fast) {
            Err(error)
                if fast
                    && matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                    ) =>
            {
                Client::connect(hint, port, game, secret, size, false)
            }
            result => result,
        }
    })();
    match connect {
        Ok(connection) => {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            clients().lock().unwrap().insert(id, Arc::new(connection));
            id
        }
        Err(error) => {
            throw(&mut env, error);
            0
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_lanstream_mobile_NativeBridge_info(
    env: JNIEnv,
    _class: JClass,
    id: jlong,
) -> jstring {
    client(id)
        .and_then(|client| env.new_string(&client.info).ok())
        .map_or(null_mut(), JString::into_raw)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_lanstream_mobile_NativeBridge_ping(
    _env: JNIEnv,
    _class: JClass,
    id: jlong,
    timestamp: jlong,
) {
    if let Some(client) = client(id) {
        client.queue(stream_wire::PING, timestamp.to_be_bytes().to_vec());
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_lanstream_mobile_NativeBridge_read(
    mut env: JNIEnv,
    _class: JClass,
    id: jlong,
) -> jbyteArray {
    let Some(client) = client(id) else {
        return null_mut();
    };
    let mut reader = client.reader.lock().unwrap();
    let Reader {
        socket,
        buffer,
        pcm,
    } = &mut *reader;
    match receive(socket, buffer, MAX_MEDIA) {
        Ok(AUDIO_COMPACT) => {
            pcm.clear();
            pcm.push(AUDIO);
            if stream_wire::audio::decode(&buffer[1..], pcm).is_none() {
                throw(&mut env, "音频封包无效");
                return null_mut();
            }
            env.byte_array_from_slice(pcm)
                .map_or(null_mut(), JByteArray::into_raw)
        }
        Ok(_) => env
            .byte_array_from_slice(buffer)
            .map_or(null_mut(), JByteArray::into_raw),
        Err(error) => {
            throw(&mut env, error);
            null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_lanstream_mobile_NativeBridge_readPicture(
    mut env: JNIEnv,
    _class: JClass,
    id: jlong,
) -> jbyteArray {
    let Some(client) = client(id) else {
        return null_mut();
    };
    let Some(lane) = &client.datagrams else {
        return null_mut();
    };
    match lane.lock().unwrap().read(&client.active) {
        Ok(Some(picture)) => {
            let mut packet = Vec::with_capacity(4 + picture.data.len());
            packet.extend_from_slice(&picture.sequence.to_be_bytes());
            packet.extend_from_slice(&picture.data);
            env.byte_array_from_slice(&packet)
                .map_or(null_mut(), JByteArray::into_raw)
        }
        Ok(None) => null_mut(),
        Err(error) => {
            throw(&mut env, error);
            null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_lanstream_mobile_NativeBridge_keys(
    env: JNIEnv,
    _class: JClass,
    id: jlong,
    values: JIntArray,
) {
    let Some(client) = client(id) else { return };
    let Ok(size) = env.get_array_length(&values) else {
        return;
    };
    if size as usize > MAX_CONTROL {
        return;
    }
    let mut keys = [0; MAX_CONTROL];
    let key_slice = &mut keys[..size as usize];
    if env.get_int_array_region(&values, 0, key_slice).is_err() {
        return;
    }
    let payload = key_slice
        .iter()
        .filter_map(|&key| u8::try_from(key).ok())
        .filter(|key| stream_wire::allowed_key(*key))
        .collect();
    client.queue(KEYS, payload);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_lanstream_mobile_NativeBridge_pointer(
    _env: JNIEnv,
    _class: JClass,
    id: jlong,
    kind: jint,
    x: jint,
    y: jint,
) {
    let Some(client) = client(id) else { return };
    if !(0..=2).contains(&kind) {
        return;
    }
    let x = (x.clamp(0, 65535) as u16).to_be_bytes();
    let y = (y.clamp(0, 65535) as u16).to_be_bytes();
    client.queue(POINTER, vec![kind as u8, x[0], x[1], y[0], y[1]]);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_lanstream_mobile_NativeBridge_close(
    _env: JNIEnv,
    _class: JClass,
    id: jlong,
) {
    let connection = clients().lock().unwrap().remove(&id);
    if let Some(connection) = connection {
        connection.active.store(false, Ordering::Relaxed);
        if let Some(axes) = &connection.axes {
            axes.stop();
        }
        let _ = connection.control.shutdown(Shutdown::Both);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_lanstream_mobile_NativeBridge_gamepad(
    _env: JNIEnv,
    _class: JClass,
    id: jlong,
    buttons: jint,
    x: jint,
    y: jint,
) {
    let Some(client) = client(id) else { return };
    let report = PadState {
        buttons: buttons as u16,
        x: x.clamp(-32767, 32767) as i16,
        y: y.clamp(-32767, 32767) as i16,
    };
    if let Some(axes) = &client.axes {
        if axes.record(report) {
            client.queue(
                stream_wire::PAD_BUTTONS,
                report.buttons.to_be_bytes().to_vec(),
            );
        }
    } else {
        client.queue(GAMEPAD, report.encode().to_vec());
    }
}
mod axes;
