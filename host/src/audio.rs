use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};
use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat, initialize_mta};

static AUDIO_ACTIVE: AtomicBool = AtomicBool::new(false);

struct AudioLease;
impl Drop for AudioLease {
    fn drop(&mut self) {
        AUDIO_ACTIVE.store(false, Ordering::Release);
    }
}

pub struct AudioSession {
    stop: Arc<AtomicBool>,
}

impl AudioSession {
    pub fn start(pid: u32, packets: SyncSender<(u8, Vec<u8>)>, compact: bool) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = stop.clone();
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(500);
            while !cancelled.load(Ordering::Relaxed) {
                if AUDIO_ACTIVE
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    let _lease = AudioLease;
                    if let Err(error) = capture(pid, &cancelled, packets, compact) {
                        eprintln!("游戏进程音频不可用：{error:#}");
                    }
                    crate::teardown("audio stopped");
                    return;
                }
                if Instant::now() >= deadline {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        Self { stop }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for AudioSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn capture(
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
        while !stop.load(Ordering::Relaxed) && capture.get_next_packet_size()?.unwrap_or(0) != 0 {
            capture.read_from_device_to_deque(&mut samples)?;
        }
        // 10 ms of stereo, 48 kHz. Send little-endian PCM to Android AudioTrack.
        while !stop.load(Ordering::Relaxed) && samples.len() >= 3840 {
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
