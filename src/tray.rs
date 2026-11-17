//! 通知区域（托盘）图标。图标是代码画的一颗黑色药丸，不需要资源文件。

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, DeleteObject,
};
use windows::Win32::UI::Shell::{NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, GetSystemMetrics, HICON, ICONINFO, SM_CXSMICON, WM_APP,
};

/// 托盘图标的回调消息，lparam 是鼠标消息（WM_LBUTTONUP、WM_RBUTTONUP…）。
pub const WM_APP_TRAY: u32 = WM_APP + 2;

pub struct Tray {
    hwnd: HWND,
    icon: Option<HICON>,
}

impl Tray {
    pub fn new(hwnd: HWND) -> Tray {
        let t = Tray { hwnd, icon: make_icon() };
        t.add();
        t
    }

    fn data(&self) -> NOTIFYICONDATAW {
        let mut d = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_APP_TRAY,
            ..Default::default()
        };
        if let Some(icon) = self.icon {
            d.hIcon = icon;
            d.uFlags |= NIF_ICON;
        }
        for (dst, src) in d.szTip.iter_mut().zip("灵动岛".encode_utf16()) {
            *dst = src;
        }
        d
    }

    /// 资源管理器重启（TaskbarCreated）后要重新添加。
    pub fn add(&self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &self.data());
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data());
            if let Some(i) = self.icon.take() {
                let _ = DestroyIcon(i);
            }
        }
    }
}

/// 白边黑色药丸，深浅色任务栏上都看得清。
fn make_icon() -> Option<HICON> {
    let n = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64);
    let px = pill_pixels(n as usize);
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: n,
                biHeight: -n,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u8, px.len());
        let mask_bits = vec![0u8; (n as usize).div_ceil(16) * 2 * n as usize];
        let mask = CreateBitmap(n, n, 1, 1, Some(mask_bits.as_ptr() as _));
        let info = ICONINFO { fIcon: true.into(), hbmMask: mask, hbmColor: color, ..Default::default() };
        let icon = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

/// n×n 的 BGRA（非预乘）像素。
fn pill_pixels(n: usize) -> Vec<u8> {
    let s = n as f32;
    let (hw, hh) = (s * 0.47, s * 0.24);
    let r = hh;
    let border = (s / 16.0).max(1.0);
    let mut out = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let (px, py) = (x as f32 + 0.5 - s / 2.0, y as f32 + 0.5 - s / 2.0);
            // 圆角矩形的有向距离
            let qx = px.abs() - (hw - r);
            let qy = py.abs() - (hh - r);
            let d = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r;
            let alpha = (0.5 - d).clamp(0.0, 1.0);
            let white = (d + border + 0.5).clamp(0.0, 1.0);
            let v = (white * 235.0) as u8;
            let i = (y * n + x) * 4;
            out[i..i + 4].copy_from_slice(&[v, v, v, (alpha * 255.0) as u8]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pill_is_opaque_in_middle_and_clear_in_corners() {
        let n = 32;
        let px = pill_pixels(n);
        let at = |x: usize, y: usize| &px[(y * n + x) * 4..(y * n + x) * 4 + 4];
        assert_eq!(at(16, 16), &[0, 0, 0, 255]);
        assert_eq!(at(0, 0)[3], 0);
        // 上边缘是白边
        assert!(at(16, 9)[0] > 200);
    }
}
