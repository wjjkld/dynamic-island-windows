//! 测试工具：从独立进程做一次真实的文件拖拽，用来验证灵动岛的放置目标。
//!
//! 用法：cargo run --example drag_source -- <目标x> <目标y> <停留毫秒> <文件>...
//! 坐标是主显示器物理像素。工具会在屏幕中间开一个小窗口，用 SendInput 在上面按下左键开始拖拽，
//! 把光标移到目标点、停留一会儿再松开。点击只落在工具自己的窗口上。6 秒兜底超时。

use std::time::Duration;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{COLOR_HIGHLIGHT, HBRUSH};
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{DROPEFFECT, DROPEFFECT_COPY, DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{BHID_DataObject, ILFree, SHCreateShellItemArrayFromIDLists, SHParseDisplayName};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, HRESULT, HSTRING, Result, w};
use windows_core::implement;

#[implement(IDropSource)]
struct Source;

impl IDropSource_Impl for Source_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if keys.0 & MK_LBUTTON.0 == 0 {
            DRAGDROP_S_DROP
        } else {
            HRESULT(0)
        }
    }
    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

fn data_object(paths: &[String]) -> Result<IDataObject> {
    unsafe {
        let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
        for p in paths {
            let mut pidl = std::ptr::null_mut();
            SHParseDisplayName(&HSTRING::from(p.as_str()), None, &mut pidl, 0, None)?;
            pidls.push(pidl);
        }
        let consts: Vec<*const ITEMIDLIST> = pidls.iter().map(|&p| p as *const _).collect();
        let arr = SHCreateShellItemArrayFromIDLists(&consts);
        for p in pidls {
            ILFree(Some(p));
        }
        arr?.BindToHandler(None, &BHID_DataObject)
    }
}

static mut FILES: Vec<String> = Vec::new();

fn mouse(x: i32, y: i32, flags: MOUSE_EVENT_FLAGS) {
    unsafe {
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: x * 65535 / (sw - 1),
                    dy: y * 65535 / (sh - 1),
                    dwFlags: flags | MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                    ..Default::default()
                },
            },
        };
        SendInput(&[input], size_of::<INPUT>() as i32);
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_LBUTTONDOWN {
            #[allow(static_mut_refs)]
            let files = FILES.clone();
            match data_object(&files) {
                Ok(data) => {
                    let source: IDropSource = Source.into();
                    let mut effect = DROPEFFECT(0);
                    let hr = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
                    println!("DoDragDrop hr={hr:?} effect={}", effect.0);
                }
                Err(e) => println!("data object 失败: {e:?}"),
            }
            let _ = DestroyWindow(hwnd);
            return LRESULT(0);
        }
        if msg == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (tx, ty, hold): (i32, i32, u64) = (args[0].parse().unwrap(), args[1].parse().unwrap(), args[2].parse().unwrap());
    unsafe {
        FILES = args[3..].to_vec();
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        OleInitialize(None)?;
        let hinst = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: w!("IslandDragSource"),
            hbrBackground: HBRUSH((COLOR_HIGHLIGHT.0 + 1) as _),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let (sx, sy) = (GetSystemMetrics(SM_CXSCREEN) / 2 - 60, GetSystemMetrics(SM_CYSCREEN) / 2);
        let hwnd = CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW, w!("IslandDragSource"), w!("drag"), WS_POPUP | WS_VISIBLE, sx, sy, 120, 80, None, None, Some(hinst.into()), None)?;
        let mut old = POINT::default();
        let _ = GetCursorPos(&mut old);

        let (cx, cy) = (sx + 60, sy + 40);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            mouse(cx, cy, MOUSE_EVENT_FLAGS(0));
            std::thread::sleep(Duration::from_millis(80));
            mouse(cx, cy, MOUSEEVENTF_LEFTDOWN);
            std::thread::sleep(Duration::from_millis(120));
            // 先挪几步越过拖拽阈值，再一路移到目标
            for i in 1..=25 {
                let t = i as f32 / 25.0;
                mouse(cx + ((tx - cx) as f32 * t) as i32, cy + ((ty - cy) as f32 * t) as i32, MOUSE_EVENT_FLAGS(0));
                std::thread::sleep(Duration::from_millis(20));
            }
            let steps = (hold / 40).max(1);
            for k in 0..steps {
                mouse(tx + (k % 2) as i32, ty, MOUSE_EVENT_FLAGS(0));
                std::thread::sleep(Duration::from_millis(40));
            }
            mouse(tx, ty, MOUSEEVENTF_LEFTUP);
            std::thread::sleep(Duration::from_millis(300));
            mouse(old.x, old.y, MOUSE_EVENT_FLAGS(0));
        });
        // 兜底：6 秒还没结束就松开鼠标退出
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(6));
            mouse(0, 0, MOUSEEVENTF_LEFTUP);
            println!("超时退出");
            std::process::exit(2);
        });

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = hwnd;
    }
    Ok(())
}
