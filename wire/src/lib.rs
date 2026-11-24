use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::io::{self, Read, Write};

pub mod audio;
pub mod udp;

const LEGACY_MAGIC: [u8; 4] = *b"LGS2";
const MAGIC: [u8; 4] = *b"LGS3";
const FAST_MAGIC: [u8; 4] = *b"LGS4";
const NATIVE_RESOLUTION_MAGIC: [u8; 4] = *b"LGS5";
const DISCOVERY: [u8; 4] = *b"LGD2";
pub const PORT: u16 = 24865;
pub const MAX_MEDIA: usize = 2 * 1024 * 1024;
pub const MAX_CONTROL: usize = 64;
pub type Secret = [u8; 32];
pub type Nonce = [u8; 16];
pub const MAX_ID: usize = 32;
pub const INFO: u8 = 1;
pub const VIDEO: u8 = 2;
pub const AUDIO: u8 = 3;
pub const ERROR: u8 = 4;
pub const CHALLENGE: u8 = 5;
pub const AUTH: u8 = 6;
pub const CONTENT: u8 = 7;
pub const UDP_OFFER: u8 = 8;
pub const UDP_READY: u8 = 9;
pub const AUDIO_COMPACT: u8 = 10;
pub const KEEPALIVE: u8 = 11;
pub const PONG: u8 = 12;
pub const KEYS: u8 = 16;
pub const POINTER: u8 = 17;
pub const GAMEPAD: u8 = 18;
pub const PING: u8 = 19;
pub const PAD_BUTTONS: u8 = 20;
pub const REQUEST_IDR: u8 = 21;
pub const STANDARD_PAD_BUTTONS: u16 = 0xf3ff;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamInfo {
    pub game_id: String,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub encoder: String,
    pub phone_aspect: bool,
    #[serde(default)]
    pub udp_video: bool,
    #[serde(default)]
    pub low_latency: bool,
    #[serde(default)]
    pub udp_axes: bool,
    #[serde(default)]
    pub keyframe_feedback: bool,
}

pub struct Hello {
    pub game: String,
    pub nonce: Nonce,
    pub viewport: (u16, u16),
    pub content_updates: bool,
    pub fast_video: bool,
    pub native_resolution: bool,
}

impl Hello {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(25 + self.game.len());
        bytes.extend_from_slice(if self.fast_video && self.native_resolution {
            &NATIVE_RESOLUTION_MAGIC
        } else if self.fast_video {
            &FAST_MAGIC
        } else if self.content_updates {
            &MAGIC
        } else {
            &LEGACY_MAGIC
        });
        bytes.extend_from_slice(&self.nonce);
        bytes.extend_from_slice(&self.viewport.0.to_be_bytes());
        bytes.extend_from_slice(&self.viewport.1.to_be_bytes());
        bytes.push(self.game.len() as u8);
        bytes.extend_from_slice(self.game.as_bytes());
        bytes
    }

    pub fn write(&self, output: &mut impl Write) -> io::Result<()> {
        output.write_all(&self.encode())
    }

    pub fn read(input: &mut impl Read) -> io::Result<Self> {
        let mut bytes = [0; 25];
        input.read_exact(&mut bytes)?;
        let native_resolution = bytes[..4] == NATIVE_RESOLUTION_MAGIC;
        let fast_video = native_resolution || bytes[..4] == FAST_MAGIC;
        let content_updates = bytes[..4] == MAGIC || fast_video;
        if !content_updates && bytes[..4] != LEGACY_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "协议版本不匹配"));
        }
        let length = usize::from(bytes[24]);
        if !(1..=MAX_ID).contains(&length) {
            return Err(io::Error::other("游戏标识长度错误"));
        }
        let mut game = vec![0; length];
        input.read_exact(&mut game)?;
        let game = std::str::from_utf8(&game)
            .ok()
            .filter(|game| valid_id(game))
            .ok_or_else(|| io::Error::other("无效的游戏标识"))?;
        Ok(Self {
            game: game.to_owned(),
            nonce: bytes[4..20].try_into().unwrap(),
            viewport: (
                u16::from_be_bytes([bytes[20], bytes[21]]),
                u16::from_be_bytes([bytes[22], bytes[23]]),
            ),
            content_updates,
            fast_video,
            native_resolution,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContentInsets {
    pub x: u16,
    pub y: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PadState {
    pub buttons: u16,
    pub x: i16,
    pub y: i16,
}

impl PadState {
    pub fn decode(payload: &[u8]) -> Option<Self> {
        let &[b0, b1, x0, x1, y0, y1] = payload else {
            return None;
        };
        Some(Self {
            buttons: u16::from_be_bytes([b0, b1]) & STANDARD_PAD_BUTTONS,
            x: i16::from_be_bytes([x0, x1]),
            y: i16::from_be_bytes([y0, y1]),
        })
    }

    pub fn encode(self) -> [u8; 6] {
        let [b0, b1] = (self.buttons & STANDARD_PAD_BUTTONS).to_be_bytes();
        let [x0, x1] = self.x.to_be_bytes();
        let [y0, y1] = self.y.to_be_bytes();
        [b0, b1, x0, x1, y0, y1]
    }
}

impl ContentInsets {
    pub fn encode(self) -> [u8; 4] {
        let [x0, x1] = self.x.to_be_bytes();
        let [y0, y1] = self.y.to_be_bytes();
        [x0, x1, y0, y1]
    }
}

pub fn valid_id(game: &str) -> bool {
    (1..=MAX_ID).contains(&game.len())
        && game.as_bytes()[0].is_ascii_lowercase()
        && game
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

pub fn parse_secret(text: &str) -> Option<Secret> {
    if text.len() != 64 || !text.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut key = [0; 32];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(key)
}

pub fn nonce() -> io::Result<Nonce> {
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(random)
}

pub fn discovery_request(game: &str, random: Nonce) -> Vec<u8> {
    [DISCOVERY.as_slice(), &random, game.as_bytes()].concat()
}

pub fn discovery_game(packet: &[u8]) -> Option<&str> {
    if !packet.starts_with(&DISCOVERY) || packet.len() < 21 {
        return None;
    }
    std::str::from_utf8(&packet[20..])
        .ok()
        .filter(|game| valid_id(game))
}

pub fn sign(key: &Secret, parts: &[&[u8]]) -> [u8; 32] {
    mac(key, parts).finalize().into_bytes().into()
}

pub fn verifies(key: &Secret, parts: &[&[u8]], signature: &[u8]) -> bool {
    mac(key, parts).verify_slice(signature).is_ok()
}

fn mac(key: &Secret, parts: &[&[u8]]) -> Hmac<Sha256> {
    let mut hmac = Hmac::<Sha256>::new_from_slice(key).expect("32-byte SHA-256 key");
    for part in parts {
        hmac.update(part);
    }
    hmac
}

pub fn send(output: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut header = [0; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    output.write_all(&header)?;
    output.write_all(payload)?;
    output.flush()
}

pub fn receive(input: &mut impl Read, buffer: &mut Vec<u8>, limit: usize) -> io::Result<u8> {
    let mut header = [0; 5];
    input.read_exact(&mut header)?;
    let bytes = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if bytes > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "封包过大"));
    }
    buffer.resize(bytes + 1, 0);
    buffer[0] = header[0];
    input.read_exact(&mut buffer[1..])?;
    Ok(header[0])
}

pub fn allowed_key(key: u8) -> bool {
    matches!(key, 0x01 | 0x02 | 0x04 | 0x08 | 0x09 | 0x0D | 0x10 | 0x11 | 0x1B
        | 0x20..=0x28 | 0x2D | 0x2E | 0x30..=0x39 | 0x41..=0x5A | 0x60..=0x7B
        | 0xBA..=0xC0 | 0xDB..=0xDE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_preserves_game_and_phone_size() {
        let mut bytes = Vec::new();
        let hello = Hello {
            game: "brotato".into(),
            nonce: [9; 16],
            viewport: (2800, 1260),
            content_updates: true,
            fast_video: true,
            native_resolution: true,
        };
        hello.write(&mut bytes).unwrap();
        let received = Hello::read(&mut bytes.as_slice()).unwrap();
        assert_eq!(received.game, hello.game);
        assert_eq!(received.nonce, hello.nonce);
        assert_eq!(received.viewport, hello.viewport);
        assert!(received.content_updates);
        assert!(received.fast_video);
        assert!(received.native_resolution);
        let original = Hello {
            content_updates: false,
            fast_video: false,
            native_resolution: false,
            ..hello
        };
        assert!(
            !Hello::read(&mut original.encode().as_slice())
                .unwrap()
                .content_updates
        );
    }

    #[test]
    fn proof_binds_role_and_game() {
        let key = [5; 32];
        let challenge = [0; 16];
        let payload = discovery_request("brotato", [2; 16]);
        let proof = sign(&key, &[b"host", &payload, &challenge]);
        assert!(verifies(&key, &[b"host", &payload, &challenge], &proof));
        assert!(!verifies(&key, &[b"phone", &payload, &challenge], &proof));
        assert_eq!(discovery_game(&payload), Some("brotato"));
        assert!(!verifies(
            &[0; 32],
            &[b"host", &payload, &challenge],
            &proof
        ));
    }

    #[test]
    fn gamepad_keeps_signed_axes_and_filters_system_buttons() {
        let pad = PadState {
            buttons: 0x3414,
            x: -1780,
            y: 23422,
        };
        assert_eq!(
            PadState::decode(&pad.encode()),
            Some(PadState {
                buttons: 0x3014,
                ..pad
            })
        );
        assert!(PadState::decode(&[0; 5]).is_none());
    }

    #[test]
    fn frames_keep_their_boundaries_and_enforce_limits() {
        let mut bytes = Vec::new();
        send(&mut bytes, KEYS, b"WA").unwrap();
        send(&mut bytes, KEYS, &[]).unwrap();
        let mut reader = bytes.as_slice();
        let mut buffer = Vec::new();
        assert_eq!(receive(&mut reader, &mut buffer, 2).unwrap(), KEYS);
        assert_eq!(buffer[1..], *b"WA");
        assert_eq!(receive(&mut reader, &mut buffer, 2).unwrap(), KEYS);
        assert_eq!(buffer.len(), 1);
        assert!(receive(&mut bytes.as_slice(), &mut buffer, 1).is_err());
    }
}
