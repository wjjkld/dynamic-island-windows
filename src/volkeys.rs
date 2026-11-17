//! 接管键盘上的音量键：按键不再交给系统（系统的音量浮窗就不会出现），由岛自己调音量并显示。
//! 低级键盘钩子在安装它的线程（UI 线程）取消息时回调，必须很快返回。

use std::cell::Cell;

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_VOLUME_DOWN, VK_VOLUME_MUTE, VK_VOLUME_UP};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN,
    WM_SYSKEYDOWN,
};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum VolKey {
    Up,
    Down,
    Mute,
}

/// 处理一次按下；返回 false 表示这次不接管，交还给系统。
pub type Handler = fn(VolKey) -> bool;

thread_local! {
    static HANDLER: Cell<Option<Handler>> = const { Cell::new(None) };
    /// 上一次按下是否被接管，对应的抬起也照此处理。
    static TAKEN: Cell<bool> = const { Cell::new(false) };
}

pub struct VolKeys(HHOOK);

impl VolKeys {
    pub fn install(handler: Handler) -> Option<VolKeys> {
        HANDLER.set(Some(handler));
        unsafe {
            let hinst = GetModuleHandleW(None).ok()?;
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(proc), Some(hinst.into()), 0).ok().map(VolKeys)
        }
    }
}

impl Drop for VolKeys {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.0);
        }
    }
}

fn key_of(vk: u32) -> Option<VolKey> {
    match vk {
        v if v == VK_VOLUME_UP.0 as u32 => Some(VolKey::Up),
        v if v == VK_VOLUME_DOWN.0 as u32 => Some(VolKey::Down),
        v if v == VK_VOLUME_MUTE.0 as u32 => Some(VolKey::Mute),
        _ => None,
    }
}

unsafe extern "system" fn proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let k = unsafe { &*(lp.0 as *const KBDLLHOOKSTRUCT) };
        if let Some(key) = key_of(k.vkCode) {
            let down = wp.0 as u32 == WM_KEYDOWN || wp.0 as u32 == WM_SYSKEYDOWN;
            let take = if down {
                let t = HANDLER.get().is_some_and(|h| h(key));
                TAKEN.set(t);
                t
            } else {
                TAKEN.get()
            };
            if take {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

/// 按一次音量键后的新音量：和系统一样每次 2%，并对齐到 2% 的格子上。
pub fn stepped(volume: f32, up: bool) -> f32 {
    let steps = (volume * 50.0).round() as i32 + if up { 1 } else { -1 };
    (steps.clamp(0, 50) as f32 / 50.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_like_windows() {
        assert_eq!(stepped(0.0, false), 0.0);
        assert_eq!(stepped(0.0, true), 0.02);
        assert_eq!(stepped(1.0, true), 1.0);
        assert!((stepped(0.5, false) - 0.48).abs() < 1e-6);
        // 不在格子上的值先对齐再走一步
        assert!((stepped(0.333, true) - 0.36).abs() < 1e-6);
    }

    #[test]
    fn only_volume_keys() {
        assert_eq!(key_of(VK_VOLUME_UP.0 as u32), Some(VolKey::Up));
        assert_eq!(key_of(0x41), None);
    }
}
