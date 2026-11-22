use crate::gamepad::Gamepad;
use crate::window::Target;
use socket2::Socket;
use std::io::{self, ErrorKind};
use std::net::{TcpStream, UdpSocket};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use stream_wire::{UDP_OFFER, UDP_READY, nonce, receive, send, udp};

pub struct FastVideo {
    socket: UdpSocket,
    cookie: [u8; 8],
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, TcpListener};

    #[test]
    fn negotiates_datagrams_and_keeps_tcp_as_a_fallback() {
        for enable_udp in [true, false] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut tcp, _) = listener.accept().unwrap();
                tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                FastVideo::negotiate(&mut tcp).unwrap().is_some()
            });
            let mut tcp = TcpStream::connect(address).unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut offer = Vec::new();
            assert_eq!(receive(&mut tcp, &mut offer, 10).unwrap(), UDP_OFFER);
            if enable_udp {
                let port = u16::from_be_bytes([offer[1], offer[2]]);
                let cookie = &offer[3..];
                let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
                udp.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                udp.connect((Ipv4Addr::LOCALHOST, port)).unwrap();
                udp.send(cookie).unwrap();
                let mut returned = [0; 8];
                assert_eq!(udp.recv(&mut returned).unwrap(), 8);
                assert_eq!(returned, cookie);
            }
            send(&mut tcp, UDP_READY, &[u8::from(enable_udp)]).unwrap();
            assert_eq!(server.join().unwrap(), enable_udp);
        }
    }
}

impl FastVideo {
    pub fn input(&self) -> io::Result<FastInput> {
        let socket = self.socket.try_clone()?;
        socket.set_read_timeout(Some(Duration::from_millis(20)))?;
        Ok(FastInput {
            socket,
            cookie: self.cookie,
        })
    }
    pub fn negotiate(tcp: &mut TcpStream) -> io::Result<Option<Self>> {
        let cookie: [u8; 8] = nonce()?[..8].try_into().unwrap();
        let local_ip = tcp.local_addr()?.ip();
        let expected = tcp.peer_addr()?.ip();
        let socket = UdpSocket::bind((local_ip, 0)).ok();
        let mut offer = Vec::new();
        if let Some(socket) = &socket {
            offer.extend_from_slice(&socket.local_addr()?.port().to_be_bytes());
            offer.extend_from_slice(&cookie);
        }
        send(tcp, UDP_OFFER, &offer)?;
        let connected = socket.and_then(|socket| {
            socket
                .set_read_timeout(Some(Duration::from_millis(350)))
                .ok()?;
            let mut probe = [0; 32];
            let (length, peer) = socket.recv_from(&mut probe).ok()?;
            if peer.ip() != expected || probe[..length] != cookie {
                return None;
            }
            socket.connect(peer).ok()?;
            socket.send(&cookie).ok()?;
            let tuned = Socket::from(socket);
            let _ = tuned.set_send_buffer_size(512 * 1024);
            let socket: UdpSocket = tuned.into();
            socket
                .set_write_timeout(Some(Duration::from_millis(12)))
                .ok()?;
            Some(Self { socket, cookie })
        });
        let mut result = Vec::new();
        if receive(tcp, &mut result, 1)? != UDP_READY {
            return Err(io::Error::other("UDP 协商响应错误"));
        }
        Ok(if result[1..] == [1] { connected } else { None })
    }

    pub fn picture(&self, sequence: u32, bytes: &[u8]) -> io::Result<()> {
        let start = Instant::now();
        let mut sent = 0;
        udp::fragment(self.cookie, sequence, bytes, |packet| {
            self.socket.send(packet)?;
            sent += packet.len();
            // Spread a large IDR burst over at most three milliseconds.
            if bytes.len() > 24 * 1024 && sent % (8 * udp::MAX_DATAGRAM) < packet.len() {
                let wanted =
                    Duration::from_secs_f64(0.003 * (sent as f64 / bytes.len() as f64).min(1.0));
                if let Some(remaining) = wanted.checked_sub(start.elapsed()) {
                    std::thread::sleep(remaining);
                }
            }
            Ok(())
        })
        .or_else(|error| {
            if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
                || error.raw_os_error() == Some(10055)
            {
                Ok(())
            } else {
                Err(error)
            }
        })
    }
}

pub struct FastInput {
    socket: UdpSocket,
    cookie: [u8; 8],
}

impl FastInput {
    pub fn receive(&self, target: &Target, pad: &Mutex<Gamepad>, stop: &AtomicBool) {
        let mut buffer = [0; 64];
        let mut last = None;
        let mut updated = None;
        while !stop.load(Ordering::Relaxed) {
            match self.socket.recv(&mut buffer) {
                Ok(count) => {
                    if let Some(axes) = udp::Axes::parse(&buffer[..count], self.cookie)
                        && last.is_none_or(|previous| axes.follows(previous))
                    {
                        if last.is_none() {
                            println!("Xbox 摇杆已启用 UDP 最新状态");
                        }
                        last = Some(axes.sequence);
                        updated = Some(Instant::now());
                        if let Err(error) =
                            pad.lock()
                                .unwrap()
                                .axes(axes.x, axes.y, target.foreground())
                        {
                            eprintln!("{error:#}");
                            break;
                        }
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) => {}
                Err(_) => break,
            }
            if updated.is_some_and(|time| time.elapsed() > Duration::from_millis(130)) {
                updated = None;
                let _ = pad.lock().unwrap().axes(0, 0, target.foreground());
            }
        }
        let _ = pad.lock().unwrap().axes(0, 0, target.foreground());
    }
}
