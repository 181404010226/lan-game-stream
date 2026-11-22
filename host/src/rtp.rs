use anyhow::{Result, bail};
use socket2::Socket;
use std::io::{self, ErrorKind};
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub struct RtpVideo {
    socket: UdpSocket,
}

impl RtpVideo {
    pub fn bind() -> io::Result<Self> {
        let socket = Socket::from(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?);
        let _ = socket.set_recv_buffer_size(2 * 1024 * 1024);
        let socket: UdpSocket = socket.into();
        socket.set_read_timeout(Some(Duration::from_millis(20)))?;
        Ok(Self { socket })
    }

    pub fn endpoint(&self) -> io::Result<String> {
        Ok(format!(
            "rtp://{}?pkt_size=1200&rtcpport=9",
            self.socket.local_addr()?
        ))
    }

    pub fn access_units(
        self,
        stop: &AtomicBool,
        mut emit: impl FnMut(Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        let mut parser = Depacketizer::default();
        let mut packet = [0; 1500];
        while !stop.load(Ordering::Relaxed) {
            match self.socket.recv(&mut packet) {
                Ok(length) => {
                    if let Some(picture) = parser.push(&packet[..length]) {
                        emit(picture)?;
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted
                    ) => {}
                Err(error) => bail!(error),
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct Depacketizer {
    bytes: Vec<u8>,
    timestamp: Option<u32>,
    sequence: Option<u16>,
    fragmented: bool,
    ready_for_idr: bool,
    idr: bool,
    invalid: bool,
}

impl Depacketizer {
    fn push(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
        if packet.len() < 13 || packet[0] != 0x80 || packet[1] & 0x7f != 96 {
            return None;
        }
        let sequence = u16::from_be_bytes([packet[2], packet[3]]);
        let stamp = u32::from_be_bytes(packet[4..8].try_into().unwrap());
        let payload = &packet[12..];
        if self.timestamp != Some(stamp) {
            if self.timestamp.is_some() {
                self.ready_for_idr = false;
            }
            self.bytes.clear();
            self.timestamp = Some(stamp);
            self.fragmented = false;
            self.idr = false;
            self.invalid = false;
        }
        if self
            .sequence
            .is_some_and(|previous| previous.wrapping_add(1) != sequence)
        {
            self.invalid = true;
        }
        self.sequence = Some(sequence);
        if !self.invalid && !self.nal(payload) {
            self.invalid = true;
        }
        if self.bytes.len() > stream_wire::MAX_MEDIA {
            self.bytes.clear();
            self.invalid = true;
        }
        if packet[1] & 0x80 == 0 {
            return None;
        }
        self.timestamp = None;
        if self.invalid || self.fragmented || self.bytes.is_empty() {
            self.ready_for_idr = false;
        } else if self.ready_for_idr || self.idr {
            self.ready_for_idr = true;
            return Some(std::mem::take(&mut self.bytes));
        }
        self.bytes.clear();
        None
    }

    fn nal(&mut self, payload: &[u8]) -> bool {
        match payload[0] & 31 {
            kind @ 1..=23 if !self.fragmented => {
                self.append(kind, payload);
                true
            }
            24 if !self.fragmented => {
                let mut rem = &payload[1..];
                while !rem.is_empty() {
                    if rem.len() < 3 {
                        return false;
                    }
                    let length = usize::from(u16::from_be_bytes([rem[0], rem[1]]));
                    if length == 0 || length > rem.len() - 2 {
                        return false;
                    }
                    self.append(rem[2] & 31, &rem[2..2 + length]);
                    rem = &rem[2 + length..];
                }
                true
            }
            28 if payload.len() > 2 => {
                let control = payload[1];
                let start = control & 0x80 != 0;
                let end = control & 0x40 != 0;
                if start == self.fragmented {
                    return false;
                }
                if start {
                    let kind = control & 31;
                    self.append(kind, &[(payload[0] & 0xe0) | kind]);
                    self.fragmented = true;
                }
                self.bytes.extend_from_slice(&payload[2..]);
                if end {
                    self.fragmented = false;
                }
                true
            }
            _ => false,
        }
    }

    fn append(&mut self, kind: u8, bytes: &[u8]) {
        self.idr |= kind == 5;
        self.bytes.extend_from_slice(&[0, 0, 0, 1]);
        self.bytes.extend_from_slice(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(sequence: u16, timestamp: u32, marker: bool, data: &[u8]) -> Vec<u8> {
        let mut p = vec![0x80, 96 | if marker { 128 } else { 0 }];
        p.extend_from_slice(&sequence.to_be_bytes());
        p.extend_from_slice(&timestamp.to_be_bytes());
        p.extend_from_slice(&[0, 0, 0, 0]);
        p.extend_from_slice(data);
        p
    }

    #[test]
    fn completes_a_fragmented_picture_on_its_marker() {
        let mut parser = Depacketizer::default();
        assert!(
            parser
                .push(&packet(4, 0, false, &[24, 0, 2, 103, 17, 0, 2, 104, 17]))
                .is_none()
        );
        assert!(
            parser
                .push(&packet(5, 0, false, &[124, 133, 4, 5]))
                .is_none()
        );
        let frame = parser.push(&packet(6, 0, true, &[124, 69, 6, 7])).unwrap();
        assert_eq!(
            frame,
            [
                0, 0, 0, 1, 103, 17, 0, 0, 0, 1, 104, 17, 0, 0, 0, 1, 101, 4, 5, 6, 7
            ]
        );
        assert!(parser.push(&packet(8, 750, true, &[65, 32])).is_none());
        assert!(parser.push(&packet(9, 1500, true, &[101, 31])).is_some());
    }
}
