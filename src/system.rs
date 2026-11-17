//! 系统状态：电池、全屏检测、本地时间。

use windows::Win32::Foundation::{LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MONITORINFOEXW, MonitorFromPoint, MonitorFromWindow,
};
use windows::core::BOOL;
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Shell::{QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, GetClassNameW, GetForegroundWindow, GetWindowLongW, GetWindowRect, IsZoomed, WS_CAPTION,
};

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
}

/// 没有电池（台式机）时返回 None。
pub fn battery() -> Option<Battery> {
    let mut s = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut s).ok()? };
    // 128 = 无电池，255 = 未知
    if s.BatteryFlag == 128 || s.BatteryFlag == 255 || s.BatteryLifePercent == 255 {
        return None;
    }
    Some(Battery {
        percent: s.BatteryLifePercent.min(100),
        charging: s.ACLineStatus == 1,
    })
}

/// 用电池时预计还能用多少秒；插着电或系统还没估算出来时返回 None。
pub fn battery_time() -> Option<u32> {
    let mut s = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut s).ok()? };
    (s.ACLineStatus == 0 && s.BatteryLifeTime != u32::MAX && s.BatteryLifeTime > 0).then_some(s.BatteryLifeTime)
}

/// 剩余时间的文字。
pub fn fmt_left(secs: u32) -> String {
    let m = secs / 60;
    if m >= 60 { format!("剩余 {} 小时 {} 分", m / 60, m % 60) } else { format!("剩余 {m} 分钟") }
}

/// 有全屏应用（游戏、全屏视频、演示）时让位。
/// SHQueryUserNotificationState 的 QUNS_BUSY 误报很多，只信独占全屏和演示模式，
/// 其余情况自己判断：前台窗口是否盖满岛所在的显示器。
pub fn fullscreen_app_running(on: HMONITOR) -> bool {
    if let Ok(s) = unsafe { SHQueryUserNotificationState() }
        && (s == QUNS_RUNNING_D3D_FULL_SCREEN || s == QUNS_PRESENTATION_MODE) {
            return true;
        }
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() {
            return false;
        }
        let mut buf = [0u16; 64];
        let n = GetClassNameW(fg, &mut buf) as usize;
        let class = String::from_utf16_lossy(&buf[..n]);
        if IGNORED_CLASSES.contains(&class.as_str()) {
            return false;
        }
        // 有标题栏的最大化窗口不算全屏（任务栏自动隐藏时它也会铺满屏幕）
        if IsZoomed(fg).as_bool() && (GetWindowLongW(fg, GWL_STYLE) as u32 & WS_CAPTION.0) == WS_CAPTION.0 {
            return false;
        }
        let mon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
        if mon != on {
            return false;
        }
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let mut r = RECT::default();
        if !GetMonitorInfoW(mon, &mut mi).as_bool() || GetWindowRect(fg, &mut r).is_err() {
            return false;
        }
        covers(&r, &mi.rcMonitor)
    }
}

const IGNORED_CLASSES: [&str; 5] = ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd", "WinDynamicIsland"];

fn covers(win: &RECT, mon: &RECT) -> bool {
    win.left <= mon.left && win.top <= mon.top && win.right >= mon.right && win.bottom >= mon.bottom
}

pub struct Monitor {
    pub handle: HMONITOR,
    /// 设备名，如 `\\.\DISPLAY2`，用来在设置里记住选择。
    pub name: String,
    pub rect: RECT,
    pub primary: bool,
}

pub fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn each(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let list = unsafe { &mut *(data.0 as *mut Vec<Monitor>) };
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(h, &mut mi as *mut _ as *mut MONITORINFO) }.as_bool() {
            let n = mi.szDevice.iter().position(|&c| c == 0).unwrap_or(mi.szDevice.len());
            list.push(Monitor {
                handle: h,
                name: String::from_utf16_lossy(&mi.szDevice[..n]),
                rect: mi.monitorInfo.rcMonitor,
                primary: mi.monitorInfo.dwFlags & 1 != 0,
            });
        }
        true.into()
    }
    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut list as *mut _ as isize));
    }
    list.sort_by_key(|m| (m.rect.left, m.rect.top));
    list
}

/// 按设备名找显示器，找不到（拔掉了）或没指定时用主显示器。
pub fn monitor_named(name: &str) -> HMONITOR {
    if !name.is_empty()
        && let Some(m) = monitors().into_iter().find(|m| m.name == name)
    {
        return m.handle;
    }
    unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) }
}

pub struct Clock {
    pub time: String,
    pub date: String,
}

pub fn clock() -> Clock {
    let t = unsafe { GetLocalTime() };
    const WEEK: [&str; 7] = ["日", "一", "二", "三", "四", "五", "六"];
    Clock {
        time: format!("{:02}:{:02}", t.wHour, t.wMinute),
        date: format!("{}月{}日 星期{}", t.wMonth, t.wDay, WEEK[(t.wDayOfWeek % 7) as usize]),
    }
}

pub fn fmt_secs(s: f64) -> String {
    let s = s.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_check() {
        let mon = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
        assert!(covers(&mon, &mon));
        assert!(covers(&RECT { left: -8, top: -8, right: 1928, bottom: 1088 }, &mon));
        assert!(!covers(&RECT { left: 0, top: 0, right: 1920, bottom: 1040 }, &mon));
    }

    #[test]
    fn formats_seconds() {
        assert_eq!(fmt_secs(0.0), "0:00");
        assert_eq!(fmt_secs(65.9), "1:05");
        assert_eq!(fmt_secs(-3.0), "0:00");
        assert_eq!(fmt_secs(3600.0), "60:00");
    }
}
