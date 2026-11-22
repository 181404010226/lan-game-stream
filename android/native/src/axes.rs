use std::net::UdpSocket;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use stream_wire::{PadState, udp::Axes};

struct Pending {
    axes: (i16, i16),
    revision: u32,
    last_buttons: Option<u16>,
    last_tcp: Instant,
    stopped: bool,
}

type Shared = Arc<(Mutex<Pending>, Condvar)>;

pub struct AxesSender {
    shared: Shared,
    worker: Option<JoinHandle<()>>,
}

impl AxesSender {
    pub fn new(socket: UdpSocket, cookie: [u8; 8]) -> std::io::Result<Self> {
        socket.set_write_timeout(Some(Duration::from_millis(4)))?;
        let shared = Arc::new((
            Mutex::new(Pending {
                axes: (0, 0),
                revision: 0,
                last_buttons: None,
                last_tcp: Instant::now(),
                stopped: false,
            }),
            Condvar::new(),
        ));
        let state = shared.clone();
        let worker = thread::spawn(move || send_axes(socket, cookie, &state));
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    pub fn record(&self, report: PadState) -> bool {
        let (state, ready) = &*self.shared;
        let mut state = state.lock().unwrap();
        if state.axes != (report.x, report.y) {
            state.axes = (report.x, report.y);
            state.revision = state.revision.wrapping_add(1);
            ready.notify_one();
        }
        if state.last_buttons != Some(report.buttons)
            || state.last_tcp.elapsed() >= Duration::from_millis(250)
        {
            state.last_buttons = Some(report.buttons);
            state.last_tcp = Instant::now();
            true
        } else {
            false
        }
    }

    pub fn stop(&self) {
        let (state, wake) = &*self.shared;
        state.lock().unwrap().stopped = true;
        wake.notify_one();
    }
}

impl Drop for AxesSender {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn send_axes(socket: UdpSocket, cookie: [u8; 8], shared: &Shared) {
    let (state, wake) = &**shared;
    let mut known = 0;
    let mut sequence = 0_u32;
    loop {
        let pending = state.lock().unwrap();
        let current = if pending.axes == (0, 0) {
            wake.wait_while(pending, |p| !p.stopped && p.revision == known)
                .unwrap()
        } else {
            wake.wait_timeout_while(pending, Duration::from_millis(24), |p| {
                !p.stopped && p.revision == known
            })
            .unwrap()
            .0
        };
        if current.stopped {
            break;
        }
        let (x, y) = current.axes;
        let changed = current.revision != known;
        known = current.revision;
        drop(current);
        let packet = Axes { sequence, x, y }.packet(cookie);
        let _ = socket.send(&packet);
        if changed && x == 0 && y == 0 {
            let _ = socket.send(&packet);
        }
        sequence = sequence.wrapping_add(1);
    }
}
