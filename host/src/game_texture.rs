use crate::color_gpu::Nv12Converter;
use anyhow::{Result, bail};
use std::ffi::c_void;
use std::ptr::null_mut;
use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::core::Interface;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    Window,
    Auto,
    Game,
    GameCopy,
}

impl FromStr for CaptureMode {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "window" => Ok(Self::Window),
            "auto" => Ok(Self::Auto),
            "game" => Ok(Self::Game),
            "game-copy" => Ok(Self::GameCopy),
            _ => bail!("--capture 应为 window、auto、game 或 game-copy"),
        }
    }
}
impl CaptureMode {
    pub fn required(self) -> bool {
        matches!(self, Self::Game | Self::GameCopy)
    }
}
unsafe extern "C" {
    fn lan_game_open(
        device: *mut c_void,
        pid: u32,
        width: u32,
        height: u32,
        mode: i32,
        out: *mut *mut c_void,
        actual: *mut i32,
    ) -> i32;
    fn lan_game_texture(reader: *mut c_void, slot: u32) -> *mut c_void;
    fn lan_game_acquire(reader: *mut c_void) -> i32;
    fn lan_game_release(reader: *mut c_void) -> i32;
    fn lan_game_close(reader: *mut c_void);
    fn lan_game_event(reader: *mut c_void) -> *mut c_void;
}

pub struct GameTextures {
    raw: *mut c_void,
    pub textures: Vec<ID3D11Texture2D>,
    pub encoder_name: &'static str,
}

impl GameTextures {
    pub fn event(&self) -> *mut c_void {
        unsafe { lan_game_event(self.raw) }
    }
    pub fn open(
        device: &ID3D11Device,
        pid: u32,
        size: (u32, u32),
        mode: CaptureMode,
    ) -> Result<Option<Self>> {
        let (mut raw, mut actual) = (null_mut(), 0);
        let requested = if mode == CaptureMode::GameCopy { 2 } else { 1 };
        let code = unsafe {
            lan_game_open(
                device.as_raw(),
                pid,
                size.0,
                size.1,
                requested,
                &mut raw,
                &mut actual,
            )
        };
        if code == 1 {
            return Ok(None);
        }
        if code != 0 || raw.is_null() {
            bail!("游戏纹理协商失败（{code}）");
        }
        let mut reader = Self {
            raw,
            textures: Vec::with_capacity(3),
            encoder_name: if actual == 1 {
                "nvenc-game-direct"
            } else {
                "nvenc-game-copy"
            },
        };
        for i in 0..3 {
            let ptr = unsafe { lan_game_texture(raw, i) };
            if ptr.is_null() {
                bail!("缺少游戏共享纹理 {i}");
            }
            reader
                .textures
                .push(unsafe { ID3D11Texture2D::from_raw(ptr) });
        }
        Ok(Some(reader))
    }

    pub fn convert(
        &self,
        converter: &Nv12Converter,
        context: &ID3D11DeviceContext,
        slot: usize,
        wait: Duration,
    ) -> Result<bool> {
        let deadline = Instant::now() + wait;
        loop {
            match unsafe { lan_game_acquire(self.raw) } {
                input @ 0..=2 => {
                    let result = converter.convert_input(input as usize, slot);
                    unsafe { context.Flush() };
                    let released = unsafe { lan_game_release(self.raw) };
                    result?;
                    if released != 0 {
                        bail!("释放游戏纹理失败（{released}）");
                    }
                    return Ok(true);
                }
                -1 if Instant::now() >= deadline => return Ok(false),
                -1 => thread::sleep(Duration::from_millis(1)),
                code => bail!("游戏纹理会话已结束（{code}）"),
            }
        }
    }
}
impl Drop for GameTextures {
    fn drop(&mut self) {
        unsafe { lan_game_close(self.raw) };
    }
}
