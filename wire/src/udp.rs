use crate::MAX_MEDIA;
use std::collections::BTreeMap;
use std::io;
use std::time::{Duration, Instant};

const HEADER: usize = 19;
const FRAGMENT: usize = 1136;
const GROUP: usize = 8;
const WINDOW: u32 = 7;
pub const MAX_DATAGRAM: usize = HEADER + FRAGMENT;

pub fn fragment(
    cookie: [u8; 8],
    sequence: u32,
    data: &[u8],
    mut output: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    if data.is_empty() || data.len() > MAX_MEDIA {
        return Err(io::Error::other("invalid UDP picture size"));
    }
    let mut packet = [0_u8; MAX_DATAGRAM];
    packet[..8].copy_from_slice(&cookie);
    packet[8..12].copy_from_slice(&sequence.to_be_bytes());
    packet[12..16].copy_from_slice(&(data.len() as u32).to_be_bytes());
    for (group, block) in data.chunks(FRAGMENT * GROUP).enumerate() {
        let mut parity = [0_u8; FRAGMENT];
        for (index, part) in block.chunks(FRAGMENT).enumerate() {
            packet[16..18].copy_from_slice(&((group * GROUP + index) as u16).to_be_bytes());
            packet[18] = 0;
            packet[HEADER..HEADER + part.len()].copy_from_slice(part);
            for (target, byte) in parity.iter_mut().zip(part) {
                *target ^= *byte;
            }
            output(&packet[..HEADER + part.len()])?;
        }
        packet[16..18].copy_from_slice(&(group as u16).to_be_bytes());
        packet[18] = 1;
        packet[HEADER..].copy_from_slice(&parity);
        output(&packet)?;
    }
    Ok(())
}

pub struct Picture {
    pub sequence: u32,
    pub data: Vec<u8>,
}

pub struct Assembler {
    cookie: [u8; 8],
    next: u32,
    expiry: Duration,
    pending: BTreeMap<u32, Partial>,
}

impl Assembler {
    pub fn new(cookie: [u8; 8], expiry: Duration) -> Self {
        Self {
            cookie,
            next: 0,
            expiry,
            pending: BTreeMap::new(),
        }
    }

    pub fn packet(&mut self, packet: &[u8], now: Instant) {
        if !(HEADER..=MAX_DATAGRAM).contains(&packet.len()) || packet[..8] != self.cookie {
            return;
        }
        let sequence = u32::from_be_bytes(packet[8..12].try_into().unwrap());
        if sequence < self.next {
            return;
        }
        let size = u32::from_be_bytes(packet[12..16].try_into().unwrap()) as usize;
        if !(1..=MAX_MEDIA).contains(&size) {
            return;
        }
        let index = u16::from_be_bytes(packet[16..18].try_into().unwrap()) as usize;
        let bytes = &packet[HEADER..];
        let count = size.div_ceil(FRAGMENT);
        let parity = match packet[18] {
            0 if index < count && bytes.len() == (size - index * FRAGMENT).min(FRAGMENT) => false,
            1 if index < count.div_ceil(GROUP) && bytes.len() == FRAGMENT => true,
            _ => return,
        };
        if sequence - self.next >= WINDOW {
            self.next = sequence - WINDOW + 1;
            self.pending.retain(|id, _| *id >= self.next);
        }
        let picture = self
            .pending
            .entry(sequence)
            .or_insert_with(|| Partial::new(size, count, now));
        if picture.bytes.len() != size || picture.missing == 0 {
            return;
        }
        let group = if parity {
            if picture.parity[index].is_none() {
                picture.parity[index] = Some(bytes.to_vec());
            }
            index
        } else {
            picture.write(index, bytes);
            index / GROUP
        };
        picture.repair(group);
    }

    pub fn ready(&mut self, now: Instant) -> Option<Picture> {
        while let Some((&sequence, first)) = self.pending.first_key_value() {
            if sequence == self.next && first.missing == 0 {
                let (_, picture) = self.pending.pop_first()?;
                self.next = sequence.wrapping_add(1);
                return Some(Picture {
                    sequence,
                    data: picture.bytes,
                });
            }
            if now.saturating_duration_since(first.started) < self.expiry {
                return None;
            }
            if sequence > self.next {
                self.next = sequence;
            } else {
                self.pending.pop_first();
                self.next = sequence.wrapping_add(1);
            }
        }
        None
    }
}

struct Partial {
    bytes: Vec<u8>,
    present: Vec<bool>,
    parity: Vec<Option<Vec<u8>>>,
    missing: usize,
    started: Instant,
}

impl Partial {
    fn new(size: usize, count: usize, started: Instant) -> Self {
        Self {
            bytes: vec![0; size],
            present: vec![false; count],
            parity: vec![None; count.div_ceil(GROUP)],
            missing: count,
            started,
        }
    }

    fn write(&mut self, index: usize, bytes: &[u8]) {
        if self.present[index] {
            return;
        }
        let start = index * FRAGMENT;
        self.bytes[start..start + bytes.len()].copy_from_slice(bytes);
        self.present[index] = true;
        self.missing -= 1;
    }

    fn repair(&mut self, group: usize) {
        if self.parity[group].is_none() {
            return;
        }
        let range = group * GROUP..((group + 1) * GROUP).min(self.present.len());
        let mut absent = range.clone().filter(|&index| !self.present[index]);
        let Some(missing) = absent.next() else {
            self.parity[group] = None;
            return;
        };
        if absent.next().is_some() {
            return;
        }
        let Some(mut restored) = self.parity[group].take() else {
            return;
        };
        for index in range.filter(|&index| index != missing) {
            let start = index * FRAGMENT;
            let end = (start + FRAGMENT).min(self.bytes.len());
            for (target, byte) in restored.iter_mut().zip(&self.bytes[start..end]) {
                *target ^= *byte;
            }
        }
        let length = (self.bytes.len() - missing * FRAGMENT).min(FRAGMENT);
        self.write(missing, &restored[..length]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(sequence: u32, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut result = Vec::new();
        fragment(*b"testcook", sequence, bytes, |packet| {
            result.push(packet.to_vec());
            Ok(())
        })
        .unwrap();
        result
    }

    #[test]
    fn reconstructs_out_of_order_fragments_with_one_missing_per_group() {
        let original: Vec<_> = (0..20501).map(|x| (x * 19 % 251) as u8).collect();
        let mut receiver = Assembler::new(*b"testcook", Duration::from_millis(42));
        let now = Instant::now();
        for (index, packet) in packets(0, &original).iter().enumerate().rev() {
            if index % 9 != 1 {
                receiver.packet(packet, now);
            }
        }
        assert_eq!(receiver.ready(now).unwrap().data, original);
        assert!(receiver.ready(now).is_none());
    }

    #[test]
    fn skips_an_expired_frame_and_ignores_wrong_session() {
        let now = Instant::now();
        let mut receiver = Assembler::new(*b"testcook", Duration::from_millis(42));
        let mut first = packets(0, b"late");
        first[0][0] = 0;
        receiver.packet(&first[0], now);
        for packet in packets(1, b"next") {
            receiver.packet(&packet, now);
        }
        assert!(receiver.ready(now).is_none());
        let next = receiver.ready(now + Duration::from_millis(43)).unwrap();
        assert_eq!(next.sequence, 1);
        assert_eq!(next.data, b"next");
    }
}
