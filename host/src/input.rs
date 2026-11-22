use crate::window::{Target, client_geometry};
use std::collections::BTreeSet;
use std::sync::LazyLock;
use stream_wire::allowed_key;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos;

pub struct GameInput<'a> {
    target: &'a Target,
    held: BTreeSet<u8>,
    touch_down: bool,
}

impl<'a> GameInput<'a> {
    pub fn new(target: &'a Target) -> Self {
        Self {
            target,
            held: BTreeSet::new(),
            touch_down: false,
        }
    }

    pub fn apply_keys(&mut self, keys: &[u8]) {
        if !self.target.foreground() {
            self.release();
            return;
        }
        let next: BTreeSet<_> = keys
            .iter()
            .copied()
            .filter(|key| allowed_key(*key))
            .collect();
        for key in self.held.difference(&next) {
            if *key != 1 || !self.touch_down {
                send_key(*key, false);
            }
        }
        for key in next.difference(&self.held) {
            if *key != 1 || !self.touch_down {
                send_key(*key, true);
            }
        }
        self.held = next;
    }

    pub fn pointer(&mut self, data: &[u8]) {
        if !self.target.foreground() {
            self.release();
            return;
        }
        let [action @ 0..=2, x1, x2, y1, y2] = data else {
            return;
        };
        let Some((width, height, origin)) = client_geometry(self.target.hwnd()) else {
            return;
        };
        let x = i32::from(u16::from_be_bytes([*x1, *x2]));
        let y = i32::from(u16::from_be_bytes([*y1, *y2]));
        unsafe {
            SetCursorPos(
                origin.x + ((width - 1).max(0) * x) / 65535,
                origin.y + ((height - 1).max(0) * y) / 65535,
            )
        };
        if *action != 0 {
            let next = *action == 1;
            if self.touch_down != next && !self.held.contains(&1) {
                send_key(1, next);
            }
            self.touch_down = next;
        }
    }

    fn release(&mut self) {
        if self.touch_down && !self.held.contains(&1) {
            send_key(1, false);
        }
        self.touch_down = false;
        for key in std::mem::take(&mut self.held) {
            send_key(key, false);
        }
    }
}

impl Drop for GameInput<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

fn send_key(key: u8, down: bool) {
    let mouse_flags = match (key, down) {
        (1, true) => MOUSEEVENTF_LEFTDOWN,
        (1, false) => MOUSEEVENTF_LEFTUP,
        (2, true) => MOUSEEVENTF_RIGHTDOWN,
        (2, false) => MOUSEEVENTF_RIGHTUP,
        (4, true) => MOUSEEVENTF_MIDDLEDOWN,
        (4, false) => MOUSEEVENTF_MIDDLEUP,
        _ => 0,
    };
    let input = if mouse_flags != 0 {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dwFlags: mouse_flags,
                    ..Default::default()
                },
            },
        }
    } else {
        let scan = unsafe { MapVirtualKeyW(u32::from(key), MAPVK_VK_TO_VSC_EX) } as u16;
        let mut flags = if down { 0 } else { KEYEVENTF_KEYUP };
        if scan != 0 {
            flags |= KEYEVENTF_SCANCODE;
        }
        if scan & 0xff00 == 0xe000 {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: if scan == 0 { u16::from(key) } else { 0 },
                    wScan: scan & 255,
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        }
    };
    let delivered = unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) };
    static TRACE_INPUT: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("LAN_STREAM_TRACE_INPUT").is_some());
    if *TRACE_INPUT {
        println!(
            "input {} vk={key:02x} sent={delivered}",
            if down { "down" } else { "up" }
        );
    }
}
