const STEP: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];
const DELTA: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];
const HEADER: usize = 6;

#[derive(Clone, Copy, Default)]
struct Channel {
    sample: i32,
    index: i32,
}

impl Channel {
    fn advance(&mut self, code: u8) -> i16 {
        let step = STEP[self.index as usize];
        let mut change = step >> 3;
        if code & 4 != 0 {
            change += step;
        }
        if code & 2 != 0 {
            change += step >> 1;
        }
        if code & 1 != 0 {
            change += step >> 2;
        }
        self.sample =
            (self.sample + if code & 8 != 0 { -change } else { change }).clamp(-32768, 32767);
        self.index = (self.index + DELTA[(code & 7) as usize]).clamp(0, 88);
        self.sample as i16
    }

    fn code(&mut self, sample: i16) -> u8 {
        let mut distance = i32::from(sample) - self.sample;
        let mut code = if distance < 0 {
            distance = -distance;
            8
        } else {
            0
        };
        let step = STEP[self.index as usize];
        if distance >= step {
            code |= 4;
            distance -= step;
        }
        if distance >= step >> 1 {
            code |= 2;
            distance -= step >> 1;
        }
        if distance >= step >> 2 {
            code |= 1;
        }
        self.advance(code);
        code
    }
}

#[derive(Default)]
pub struct CompactEncoder {
    channels: [Channel; 2],
}

impl CompactEncoder {
    pub fn encode(&mut self, pcm: &[u8]) -> Option<Vec<u8>> {
        let first = pcm.get(..4)?;
        if !pcm.len().is_multiple_of(4) {
            return None;
        }
        let mut bytes = Vec::with_capacity(HEADER + pcm.len() / 4 - 1);
        for (channel, starting) in self.channels.iter_mut().zip(first.chunks_exact(2)) {
            channel.sample = i32::from(i16::from_le_bytes([starting[0], starting[1]]));
            bytes.extend_from_slice(starting);
            bytes.push(channel.index as u8);
        }
        for samples in pcm[4..].chunks_exact(4) {
            let left = self.channels[0].code(i16::from_le_bytes([samples[0], samples[1]]));
            let right = self.channels[1].code(i16::from_le_bytes([samples[2], samples[3]]));
            bytes.push(left | right << 4);
        }
        Some(bytes)
    }
}

pub fn decode(bytes: &[u8], pcm: &mut Vec<u8>) -> Option<()> {
    if !(HEADER..=1926).contains(&bytes.len()) || bytes[2] > 88 || bytes[5] > 88 {
        return None;
    }
    let mut channels = [
        Channel {
            sample: i16::from_le_bytes([bytes[0], bytes[1]]) as i32,
            index: bytes[2].into(),
        },
        Channel {
            sample: i16::from_le_bytes([bytes[3], bytes[4]]) as i32,
            index: bytes[5].into(),
        },
    ];
    pcm.extend_from_slice(&bytes[..2]);
    pcm.extend_from_slice(&bytes[3..5]);
    for &code in &bytes[HEADER..] {
        pcm.extend_from_slice(&channels[0].advance(code & 15).to_le_bytes());
        pcm.extend_from_slice(&channels[1].advance(code >> 4).to_le_bytes());
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_roundtrip_keeps_independent_stereo_packets() {
        let mut pcm = Vec::new();
        for index in 0..480 {
            for phase in [0.0_f64, 0.5] {
                pcm.extend_from_slice(
                    &(((f64::from(index) * 0.09 + phase).sin() * 12000.0) as i16).to_le_bytes(),
                );
            }
        }
        let mut encoder = CompactEncoder::default();
        let _ = encoder.encode(&pcm);
        let compressed = encoder.encode(&pcm).unwrap();
        assert!(compressed.len() < pcm.len() / 3);
        let mut decoded = Vec::new();
        decode(&compressed, &mut decoded).unwrap();
        assert_eq!(pcm.len(), decoded.len());
        let error: i64 = pcm
            .chunks_exact(2)
            .zip(decoded.chunks_exact(2))
            .map(|(a, b)| {
                i64::from(
                    (i32::from(i16::from_le_bytes([a[0], a[1]]))
                        - i32::from(i16::from_le_bytes([b[0], b[1]])))
                    .abs(),
                )
            })
            .sum();
        assert!(error / 960 < 500);
        assert_eq!(decode(&[0, 0, 89, 0, 0, 0], &mut Vec::new()), None);
    }
}
