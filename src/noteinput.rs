//! 岛上的输入框（新建便签、设置天气城市）：一个盖在岛上第一行的小窗口，里面放系统的 EDIT 控件，
//! 这样中文输入法、复制粘贴、光标移动都是现成的。
//! 回车保存、Esc 取消、点别处（失去焦点）也保存；结果用 WM_APP_NOTE 发回岛的窗口。

use std::sync::OnceLock;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET, DEFAULT_PITCH, DeleteObject,
    FW_NORMAL, HBRUSH, HDC, HFONT, OUT_DEFAULT_PRECIS, SetBkColor, SetTextColor,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::notes;

/// wParam = 1 保存，0 取消。
pub const WM_APP_NOTE: u32 = WM_APP + 5;
/// EM_LIMITTEXT：限制输入长度。
const EM_LIMITTEXT: u32 = 0x00C5;
/// EM_SETSEL：选中一段文字。
const EM_SETSEL: u32 = 0x00B1;

/// 输入框底色，和岛上画的那一行一样（0.17, 0.17, 0.18）。
const BG: COLORREF = COLORREF(0x002E2B2B);
const FG: COLORREF = COLORREF(0x00F5F5F5);

pub struct NoteInput {
    hwnd: HWND,
    edit: HWND,
    font: HFONT,
}

fn brush() -> HBRUSH {
    static B: OnceLock<isize> = OnceLock::new();
    HBRUSH(*B.get_or_init(|| unsafe { CreateSolidBrush(BG).0 as isize }) as *mut _)
}

fn class() -> PCWSTR {
    static REG: OnceLock<()> = OnceLock::new();
    let name = w!("WinDynamicIslandNote");
    REG.get_or_init(|| unsafe {
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(popup_proc),
            hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
            lpszClassName: name,
            hbrBackground: brush(),
            hCursor: LoadCursorW(None, IDC_IBEAM).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    });
    name
}

impl NoteInput {
    /// (x, y, w, h)：屏幕物理像素。
    pub fn open(owner: HWND, x: i32, y: i32, w: i32, h: i32, scale: f32, initial: &str) -> Option<NoteInput> {
        unsafe {
            let hinst = GetModuleHandleW(None).ok()?;
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class(),
                w!("便签"),
                WS_POPUP,
                x,
                y,
                w,
                h,
                Some(owner),
                None,
                Some(hinst.into()),
                None,
            )
            .map_err(|e| eprintln!("[note] popup failed: {e:?}"))
            .ok()?;
            let round = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const _ as _, 4);
            let none = DWMWA_COLOR_NONE;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &none as *const _ as _, 4);

            let font_px = (14.0 * scale).round() as i32;
            let font = CreateFontW(
                -font_px,
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                DEFAULT_PITCH.0 as u32,
                w!("Microsoft YaHei UI"),
            );
            // 文字和下面便签的文字左对齐
            let pad = (22.0 * scale) as i32;
            let eh = font_px + (6.0 * scale) as i32;
            let edit = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                None,
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                pad,
                (h - eh) / 2,
                w - 2 * pad,
                eh,
                Some(hwnd),
                None,
                Some(hinst.into()),
                None,
            )
            .map_err(|e| eprintln!("[note] edit failed: {e:?}"))
            .ok()?;
            SendMessageW(edit, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            SendMessageW(edit, EM_LIMITTEXT, Some(WPARAM(notes::MAX_CHARS)), None);
            if !initial.is_empty() {
                let _ = SetWindowTextW(edit, &windows::core::HSTRING::from(initial));
                // 全选，直接打字就替换
                SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
            }
            let _ = SetWindowSubclass(edit, Some(edit_proc), 1, owner.0 as usize);
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            let _ = SetFocus(Some(edit));
            Some(NoteInput { hwnd, edit, font })
        }
    }

    pub fn text(&self) -> String {
        unsafe {
            let mut buf = vec![0u16; notes::MAX_CHARS * 2 + 2];
            let n = GetWindowTextW(self.edit, &mut buf) as usize;
            String::from_utf16_lossy(&buf[..n])
        }
    }
}

impl Drop for NoteInput {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            let _ = DeleteObject(self.font.into());
        }
    }
}

extern "system" fn popup_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CTLCOLOREDIT => {
                let dc = HDC(wp.0 as *mut _);
                SetTextColor(dc, FG);
                SetBkColor(dc, BG);
                LRESULT(brush().0 as isize)
            }
            // 点输入框外面（框的边距）时把焦点还给输入框
            WM_LBUTTONDOWN => {
                if let Ok(edit) = GetWindow(hwnd, GW_CHILD) {
                    let _ = SetFocus(Some(edit));
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, owner: usize) -> LRESULT {
    let done = |save: bool| unsafe {
        let _ = PostMessageW(Some(HWND(owner as *mut _)), WM_APP_NOTE, WPARAM(save as usize), LPARAM(0));
    };
    match msg {
        WM_KEYDOWN if wp.0 == VK_RETURN.0 as usize => {
            done(true);
            return LRESULT(0);
        }
        WM_KEYDOWN if wp.0 == VK_ESCAPE.0 as usize => {
            done(false);
            return LRESULT(0);
        }
        // 单行输入框收到回车 / Esc 字符会响一声，吞掉
        WM_CHAR if wp.0 == 13 || wp.0 == 27 => return LRESULT(0),
        WM_KILLFOCUS => done(true),
        _ => {}
    }
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}
