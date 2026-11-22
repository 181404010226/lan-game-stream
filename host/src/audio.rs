use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat, initialize_mta};

pub fn capture(
    pid: u32,
    stop: &AtomicBool,
    packets: SyncSender<(u8, Vec<u8>)>,
    compact: bool,
) -> anyhow::Result<()> {
    initialize_mta().ok()?;
    let format = WaveFormat::new(32, 32, SampleType::Float, 48000, 2, None);
    let mut client = AudioClient::new_application_loopback_client(pid, true)?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: 0,
    };
    client.initialize_client(&format, Direction::Capture, mode, None)?;
    let event = client.set_get_eventhandle()?;
    let capture = client.get_audiocaptureclient()?;
    let mut samples = VecDeque::new();
    let mut encoder = stream_wire::audio::CompactEncoder::default();
    let mut notified = false;
    client.start_stream()?;
    while !stop.load(Ordering::Relaxed) {
        while capture.get_next_packet_size()?.unwrap_or(0) != 0 {
            capture.read_from_device_to_deque(&mut samples)?;
        }
        // 10 ms of stereo, 48 kHz. Send little-endian PCM to Android AudioTrack.
        while samples.len() >= 3840 {
            let raw = samples.drain(..3840).collect::<Vec<_>>();
            let mut pcm = Vec::with_capacity(1920);
            for chunk in raw.chunks_exact(4) {
                let float = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                let sample = (float.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
                pcm.extend_from_slice(&sample.to_le_bytes());
            }
            let packet = if compact {
                (
                    stream_wire::AUDIO_COMPACT,
                    encoder.encode(&pcm).expect("complete stereo PCM"),
                )
            } else {
                (stream_wire::AUDIO, pcm)
            };
            match packets.try_send(packet) {
                Ok(()) => {
                    if !notified {
                        println!("已采集游戏进程音频");
                        notified = true;
                    }
                }
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => {
                    client.stop_stream()?;
                    return Ok(());
                }
            }
        }
        let _ = event.wait_for_event(100);
    }
    client.stop_stream()?;
    Ok(())
}
