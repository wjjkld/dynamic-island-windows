//! 系统状态：电池、全屏检测、本地时间。

use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, MonitorFromWindow,
};
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

/// 有全屏应用（游戏、全屏视频、演示）时让位。
/// SHQueryUserNotificationState 的 QUNS_BUSY 误报很多，只信独占全屏和演示模式，
/// 其余情况自己判断：前台窗口是否盖满主显示器。
pub fn fullscreen_app_running() -> bool {
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
        if mon != MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) {
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
