use anyhow::{Context, Result};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use windows_capture::window::Window;
use windows_sys::Win32::Foundation::{CloseHandle, HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{
    ClientToScreen, DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW,
    MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW, MonitorFromWindow,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
    SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Debug)]
pub struct Target {
    raw: isize,
    pub pid: u32,
    pub title: String,
    pub exe: PathBuf,
    pub game_id: String,
}

impl Target {
    pub fn hwnd(&self) -> HWND {
        self.raw as HWND
    }

    pub fn exists(&self) -> bool {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(self.hwnd(), &mut pid) };
        pid == self.pid
    }

    pub fn foreground(&self) -> bool {
        unsafe { GetForegroundWindow() == self.hwnd() && self.exists() }
    }
}

pub fn enable_physical_pixels() {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

pub fn refresh_rate(target: &Target) -> u32 {
    let monitor = unsafe { MonitorFromWindow(target.hwnd(), MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    let mut mode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let valid = unsafe {
        GetMonitorInfoW(monitor, &mut info.monitorInfo) != 0
            && EnumDisplaySettingsW(info.szDevice.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) != 0
    };
    if valid { mode.dmDisplayFrequency } else { 60 }
}

pub fn list() -> Result<Vec<Target>> {
    let windows = Window::enumerate().context("读取窗口列表失败")?;
    let mut targets: Vec<_> = windows
        .into_iter()
        .filter_map(|window| {
            let pid = window.process_id().ok()?;
            if pid == std::process::id() {
                return None;
            }
            let title = window.title().ok()?;
            let exe = process_path(pid)?;
            let (width, height, _) = client_geometry(window.as_raw_hwnd())?;
            if title.trim().is_empty() || width < 240 || height < 160 {
                return None;
            }
            let game_id = steam_id(&exe).unwrap_or_else(|| path_id(&exe));
            Some(Target {
                raw: window.as_raw_hwnd() as isize,
                pid,
                title,
                exe,
                game_id,
            })
        })
        .collect();
    targets.sort_by_key(|game| {
        (
            !game.game_id.starts_with("steam-"),
            game.title.to_lowercase(),
        )
    });
    Ok(targets)
}

pub fn for_exe(exe: &Path) -> Result<Option<Target>> {
    let path = exe.to_string_lossy().to_lowercase();
    Ok(list()?
        .into_iter()
        .filter(|target| target.exe.to_string_lossy().to_lowercase() == path)
        .max_by_key(|target| {
            let area = client_geometry(target.hwnd()).map_or(0, |(w, h, _)| w * h);
            (target.foreground(), area)
        }))
}

fn process_path(pid: u32) -> Option<PathBuf> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut path = vec![0_u16; 32768];
    let mut length = path.len() as u32;
    let result = unsafe { QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut length) };
    unsafe { CloseHandle(process) };
    if result == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(
        &path[..length as usize],
    )))
}

fn steam_id(exe: &Path) -> Option<String> {
    let install_dir = exe.ancestors().find(|path| {
        path.parent().is_some_and(|parent| {
            parent
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("common"))
        })
    })?;
    let directory = install_dir.file_name()?.to_string_lossy();
    let manifests = install_dir.parent()?.parent()?.read_dir().ok()?;
    for manifest in manifests.flatten() {
        let filename = manifest.file_name();
        let filename = filename.to_string_lossy();
        let Some(appid) = filename
            .strip_prefix("appmanifest_")
            .and_then(|name| name.strip_suffix(".acf"))
        else {
            continue;
        };
        if !appid.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(config) = std::fs::read_to_string(manifest.path()) else {
            continue;
        };
        for line in config.lines() {
            let mut fields = line.split('"');
            if fields.nth(1) == Some("installdir")
                && fields
                    .nth(1)
                    .is_some_and(|value| value.eq_ignore_ascii_case(&directory))
            {
                return Some(format!("steam-{appid}"));
            }
        }
    }
    None
}

fn path_id(path: &Path) -> String {
    let hash = path
        .to_string_lossy()
        .to_lowercase()
        .bytes()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
    format!("exe-{hash:016x}")
}

pub fn client_geometry(hwnd: HWND) -> Option<(i32, i32, POINT)> {
    let mut bounds = RECT::default();
    let mut origin = POINT::default();
    let valid =
        unsafe { GetClientRect(hwnd, &mut bounds) != 0 && ClientToScreen(hwnd, &mut origin) != 0 };
    valid.then_some((bounds.right, bounds.bottom, origin))
}

pub fn capture_crop(hwnd: HWND, frame_width: u32, frame_height: u32) -> (u32, u32, u32, u32) {
    let fallback = (0, 0, frame_width & !1, frame_height & !1);
    let Some((width, height, origin)) = client_geometry(hwnd) else {
        return fallback;
    };
    let mut bounds = RECT::default();
    let result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            (&mut bounds as *mut RECT).cast::<c_void>(),
            size_of::<RECT>() as u32,
        )
    };
    if result < 0 {
        return fallback;
    }
    let left = (origin.x - bounds.left).max(0) as u32;
    let top = (origin.y - bounds.top).max(0) as u32;
    let crop_width = (width.max(0) as u32).min(frame_width.saturating_sub(left)) & !1;
    let crop_height = (height.max(0) as u32).min(frame_height.saturating_sub(top)) & !1;
    if crop_width < 64 || crop_height < 64 {
        return fallback;
    }
    (left, top, left + crop_width, top + crop_height)
}

pub struct WindowResize {
    target: Target,
    original: RECT,
    applied: RECT,
}

impl WindowResize {
    pub fn apply(
        target: &Target,
        viewport: (u16, u16),
        capture_width: i32,
        exact_pixels: bool,
    ) -> Option<Self> {
        let aspect = f64::from(viewport.0) / f64::from(viewport.1);
        if !(1.3..=3.0).contains(&aspect) {
            return None;
        }
        let hwnd = target.hwnd();
        let flags = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32;
        let ex_flags = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
        if flags & WS_THICKFRAME == 0 || unsafe { IsZoomed(hwnd) != 0 || IsIconic(hwnd) != 0 } {
            return None;
        }
        let mut original = RECT::default();
        let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe {
            GetWindowRect(hwnd, &mut original) == 0 || GetMonitorInfoW(monitor, &mut info) == 0
        } {
            return None;
        }
        let work = info.rcWork;
        let (client_width, client_height, _) = client_geometry(hwnd)?;
        if !exact_pixels
            && client_height > 0
            && client_width <= capture_width
            && (f64::from(client_width) / f64::from(client_height) / aspect - 1.0).abs() < 0.015
        {
            return None;
        }
        let max_width = (work.right - work.left - 80).clamp(240, capture_width);
        let wanted_height = i32::from(viewport.1);
        let max_height = (work.bottom - work.top - 100)
            .clamp(180, if exact_pixels { wanted_height } else { 1080 });
        let preferred_height = if exact_pixels {
            wanted_height
        } else {
            client_height.max(wanted_height)
        };
        let height = preferred_height
            .min(max_height)
            .min((f64::from(max_width) / aspect) as i32)
            & !1;
        if height < 240 {
            return None;
        }
        let width = ((f64::from(height) * aspect).round() as i32) & !1;
        if (client_width, client_height) == (width, height) {
            return None;
        }
        let mut adjusted = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if unsafe { AdjustWindowRectExForDpi(&mut adjusted, flags, 0, ex_flags, dpi) } == 0 {
            return None;
        }
        let width = adjusted.right - adjusted.left;
        let height = adjusted.bottom - adjusted.top;
        let x = original
            .left
            .clamp(work.left, (work.right - width).max(work.left));
        let y = original
            .top
            .clamp(work.top, (work.bottom - height).max(work.top));
        if unsafe {
            SetWindowPos(
                hwnd,
                null_mut(),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_NOZORDER,
            )
        } == 0
        {
            return None;
        }
        let mut applied = RECT::default();
        unsafe { GetWindowRect(hwnd, &mut applied) };
        Some(Self {
            target: target.clone(),
            original,
            applied,
        })
    }
}

impl Drop for WindowResize {
    fn drop(&mut self) {
        if !self.target.exists() {
            return;
        }
        let mut current = RECT::default();
        if unsafe { GetWindowRect(self.target.hwnd(), &mut current) == 0 } {
            return;
        }
        if [current.left, current.top, current.right, current.bottom]
            != [
                self.applied.left,
                self.applied.top,
                self.applied.right,
                self.applied.bottom,
            ]
        {
            return;
        }
        let old = self.original;
        crate::teardown("restoring game window");
        unsafe {
            SetWindowPos(
                self.target.hwnd(),
                null::<c_void>() as HWND,
                old.left,
                old.top,
                old.right - old.left,
                old.bottom - old.top,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_ASYNCWINDOWPOS,
            )
        };
        crate::teardown("game window restore queued");
    }
}
