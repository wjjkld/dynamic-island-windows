#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Windows 灵动岛：置顶、不抢焦点、空闲时点击穿透的悬浮岛。

mod audio;
mod dragdrop;
mod island;
mod lyrics;
mod media;
mod render;
mod settings;
mod shelf;
mod spring;
mod system;
mod tray;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, IDropSource, OleInitialize, RegisterDragDrop, RevokeDragDrop,
};
use windows::Win32::System::Power::{POWERBROADCAST_SETTING, RegisterPowerSettingNotification};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::SystemServices::GUID_VIDEO_CURRENT_MONITOR_BRIGHTNESS;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetLastInputInfo, LASTINPUTINFO, ReleaseCapture, SetCapture, VK_LBUTTON,
};
use windows::Win32::UI::Shell::SHDoDragDrop;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, Result, w};

use dragdrop::{DropSink, DropTarget};
use audio::Audio;
use island::{Island, LevelKind, Pace, WIN_H, WIN_W};
use media::{Cmd, Media};
use render::Renderer;
use settings::Settings;
use shelf::Shelf;
use tray::{Tray, WM_APP_TRAY};

const WM_APP_WAKE: u32 = WM_APP + 1;
const MENU_QUIT: usize = 1;
const MENU_CLEAR_SHELF: usize = 2;
const MENU_LYRICS: usize = 3;
const MENU_LEVEL: usize = 4;
const MENU_FULLSCREEN: usize = 5;
const MENU_AUTOSTART: usize = 6;
/// 100 = 主显示器，101.. = 第 n 个显示器。
const MENU_MONITOR: usize = 100;
/// 往外拖文件时 OLE 接管消息循环，用这个定时器继续驱动动画。
const TIMER_DRAG: usize = 1;

struct App {
    hwnd: HWND,
    renderer: Renderer,
    island: Island,
    scale: f32,
    origin: POINT,
    click_through: bool,
    hidden: bool,
    monitor: HMONITOR,
    settings: Settings,
    audio: Option<Audio>,
    tray: Tray,
    /// 亮度通知：注册时会立刻来一次当前值，插拔电源时系统也会自己调亮度，这些都不弹。
    brightness: Option<u32>,
    power_changed: Instant,
    last_sys: Instant,
    lbutton: bool,
    /// 左键是在窗口外按下的，现在按着进了窗口：可能是在拖文件过来。
    drag_candidate: bool,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    // 拖放回调可能在 App 被借用时到达，事件先放这里，下一轮 step 再处理
    static DROP_HOVER: Cell<Option<bool>> = const { Cell::new(None) };
    static DROPPED: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
    static DRAGGING_OUT: Cell<bool> = const { Cell::new(false) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
}

struct Sink;

impl DropSink for Sink {
    fn drag_hover(&self, over: bool) {
        DROP_HOVER.set(Some(over));
    }
    fn dropped(&self, paths: Vec<PathBuf>) {
        DROPPED.with_borrow_mut(|d| d.extend(paths));
    }
    fn is_dragging_out(&self) -> bool {
        DRAGGING_OUT.get()
    }
}

impl App {
    fn new(hwnd: HWND) -> Result<App> {
        let settings = Settings::load();
        let (monitor, origin, scale) = placement(&settings.monitor);
        let (pw, ph) = phys(scale);
        unsafe {
            SetWindowPos(hwnd, Some(HWND_TOPMOST), origin.x, origin.y, pw, ph, SWP_NOACTIVATE)?;
        }
        let renderer = Renderer::new(hwnd, pw as u32, ph as u32, 96.0 * scale)?;
        let raw = hwnd.0 as isize;
        let wake = move || unsafe {
            let _ = PostMessageW(Some(HWND(raw as *mut _)), WM_APP_WAKE, WPARAM(0), LPARAM(0));
        };
        let media = Media::start(wake, settings.lyrics);
        let audio = Audio::new(wake).map_err(|e| eprintln!("[island] 音量监听失败: {e:?}")).ok();
        unsafe {
            let _ = RegisterPowerSettingNotification(HANDLE(hwnd.0), &GUID_VIDEO_CURRENT_MONITOR_BRIGHTNESS, DEVICE_NOTIFY_WINDOW_HANDLE);
        }
        Ok(App {
            hwnd,
            renderer,
            island: Island::new(media, Shelf::load()),
            scale,
            origin,
            click_through: true,
            hidden: false,
            monitor,
            settings,
            audio,
            tray: Tray::new(hwnd),
            brightness: None,
            power_changed: Instant::now(),
            last_sys: Instant::now(),
            lbutton: false,
            drag_candidate: false,
        })
    }

    /// 显示器、DPI 变化后重新定位。
    fn reposition(&mut self) {
        let (monitor, origin, scale) = placement(&self.settings.monitor);
        let (pw, ph) = phys(scale);
        self.monitor = monitor;
        self.origin = origin;
        unsafe {
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), origin.x, origin.y, pw, ph, SWP_NOACTIVATE);
        }
        if (scale - self.scale).abs() > f32::EPSILON {
            self.scale = scale;
            if let Err(e) = self.renderer.resize(pw as u32, ph as u32, 96.0 * scale) {
                eprintln!("[island] resize 失败: {e:?}");
            }
        }
        self.island.dirty = true;
    }

    fn on_brightness(&mut self, percent: u32) {
        let first = self.brightness.is_none();
        let same = self.brightness == Some(percent);
        self.brightness = Some(percent);
        // 只在用户刚动过键盘鼠标时显示：排除空闲变暗、插拔电源时的自动调节
        let auto = self.power_changed.elapsed() < Duration::from_secs(2) || idle_ms() > 3000;
        if first || same || auto || !self.settings.level_osd {
            return;
        }
        self.island.show_level(LevelKind::Brightness, percent as f32 / 100.0, false);
    }

    fn to_logical(&self, x: i32, y: i32) -> (f32, f32) {
        (x as f32 / self.scale, y as f32 / self.scale)
    }

    fn set_click_through(&mut self, on: bool) {
        if self.click_through == on {
            return;
        }
        self.click_through = on;
        unsafe {
            let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            let t = WS_EX_TRANSPARENT.0 as isize;
            SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, if on { ex | t } else { ex & !t });
        }
    }

    /// 每轮循环调用：更新状态，必要时出一帧。返回空闲等待毫秒数（0 = 继续动画）。
    fn step(&mut self) -> u32 {
        let now = Instant::now();

        if now - self.last_sys >= Duration::from_secs(1) {
            self.last_sys = now;
            // ISLAND_ALWAYS_SHOW=1：调试用，全屏时也不隐藏
            let full = self.settings.hide_in_fullscreen
                && std::env::var_os("ISLAND_ALWAYS_SHOW").is_none()
                && system::fullscreen_app_running(self.monitor);
            if full != self.hidden {
                self.hidden = full;
                unsafe {
                    let _ = ShowWindow(self.hwnd, if full { SW_HIDE } else { SW_SHOWNOACTIVATE });
                }
            }
            if !full {
                // 开始菜单、任务栏等可能盖过来，周期性地重新置顶
                unsafe {
                    let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                }
            }
            self.island.poll_system();
        }
        if let Some((v, muted)) = self.audio.as_mut().and_then(|a| a.poll())
            && self.settings.level_osd
        {
            self.island.show_level(LevelKind::Volume, v, muted);
        }
        if self.hidden {
            return 250;
        }

        if let Some(over) = DROP_HOVER.take() {
            self.island.set_drop_hover(over);
        }
        let dropped = DROPPED.with_borrow_mut(std::mem::take);
        if !dropped.is_empty() {
            self.island.add_files(dropped);
        }

        let mut p = POINT::default();
        let _ = unsafe { GetCursorPos(&mut p) };
        let (x, y) = self.to_logical(p.x - self.origin.x, p.y - self.origin.y);
        let in_window = (0.0..WIN_W).contains(&x) && (0.0..WIN_H).contains(&y);
        let inside = self.island.hit(x, y);

        // 拖文件过来时，岛还是个小药丸，很难对准。所以只要"在窗口外按下左键、按着移进窗口"，
        // 就让整个窗口接收拖放；OLE 会在真的是文件拖拽时发 DragEnter，岛再展开成放置区。
        // 左键在窗口里按下的（比如点浏览器标签页）不算，避免截走别的程序的点击。
        let lbutton = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0;
        if lbutton && !self.lbutton {
            self.drag_candidate = !in_window;
        } else if !lbutton {
            self.drag_candidate = false;
        }
        self.lbutton = lbutton;
        let catch_drop = self.drag_candidate && in_window;

        self.set_click_through(!(inside || catch_drop || self.island.is_scrubbing()));
        self.island.on_mouse(inside, (x, y), now);
        self.island.tick(now);

        let (due, pace) = self.island.pace(now);
        #[cfg(debug_assertions)]
        trace(due, matches!(pace, Pace::Animating), self.hidden);
        if due && let Err(e) = self.island.render(&self.renderer) {
            eprintln!("[island] 渲染失败，重建设备: {e:?}");
            let (pw, ph) = phys(self.scale);
            match Renderer::new(self.hwnd, pw as u32, ph as u32, 96.0 * self.scale) {
                Ok(r) => self.renderer = r,
                Err(e) => eprintln!("[island] 重建失败: {e:?}"),
            }
        }
        match pace {
            Pace::Animating => {
                // 跟 DWM 合成节拍对齐，动画不撕裂也不空转
                unsafe {
                    let _ = DwmFlush();
                }
                0
            }
            Pace::Wait(ms) => ms,
        }
    }
}

/// 把暂存的文件拖出去。必须在 App 借用之外调用：SHDoDragDrop 会跑自己的消息循环。
fn drag_out(hwnd: HWND, path: PathBuf) {
    let Ok(data) = shelf::data_object(&[path.as_path()]) else { return };
    DRAGGING_OUT.set(true);
    unsafe {
        SetTimer(Some(hwnd), TIMER_DRAG, 15, None);
        let _ = SHDoDragDrop(Some(hwnd), &data, None::<&IDropSource>, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK);
        let _ = KillTimer(Some(hwnd), TIMER_DRAG);
    }
    DRAGGING_OUT.set(false);
    with_app(|a| a.island.drag_out_done());
}

/// 调试版：ISLAND_TRACE=1 时每秒打印一次循环、出帧、动画帧的次数，用来查空闲耗电。
#[cfg(debug_assertions)]
fn trace(rendered: bool, animating: bool, hidden: bool) {
    thread_local! {
        static T: Cell<(Instant, u32, u32, u32)> = Cell::new((Instant::now(), 0, 0, 0));
    }
    if std::env::var_os("ISLAND_TRACE").is_none() {
        return;
    }
    let (t, steps, frames, anim) = T.get();
    let next = (t, steps + 1, frames + rendered as u32, anim + animating as u32);
    if t.elapsed() >= Duration::from_secs(1) {
        eprintln!("[trace] steps={} frames={} animating={} hidden={hidden}", next.1, next.2, next.3);
        T.set((Instant::now(), 0, 0, 0));
    } else {
        T.set(next);
    }
}

/// 距离上次键盘鼠标输入多少毫秒。
fn idle_ms() -> u32 {
    let mut li = LASTINPUTINFO { cbSize: size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe {
        if !GetLastInputInfo(&mut li).as_bool() {
            return 0;
        }
        GetTickCount().wrapping_sub(li.dwTime)
    }
}

/// 选中显示器顶部居中的位置和缩放。
fn placement(monitor: &str) -> (HMONITOR, POINT, f32) {
    unsafe {
        let mon = system::monitor_named(monitor);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let scale = dx as f32 / 96.0;
        let (pw, _) = phys(scale);
        let rc = mi.rcMonitor;
        (mon, POINT { x: rc.left + (rc.right - rc.left - pw) / 2, y: rc.top }, scale)
    }
}

fn phys(scale: f32) -> (i32, i32) {
    ((WIN_W * scale).round() as i32, (WIN_H * scale).round() as i32)
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut a| a.as_mut().map(f)))
}

fn lparam_xy(l: LPARAM) -> (i32, i32) {
    ((l.0 & 0xffff) as i16 as i32, ((l.0 >> 16) & 0xffff) as i16 as i32)
}

/// 右键岛或托盘图标弹出的菜单。
fn show_menu(hwnd: HWND) {
    let Some((has_files, st)) = with_app(|a| (!a.island.shelf.items.is_empty(), a.settings.clone())) else { return };
    let monitors = system::monitors();
    let check = |on: bool| if on { MF_STRING | MF_CHECKED } else { MF_STRING };
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        if has_files {
            let _ = AppendMenuW(menu, MF_STRING, MENU_CLEAR_SHELF, w!("清空暂存区"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        }
        let _ = AppendMenuW(menu, check(st.lyrics), MENU_LYRICS, w!("显示歌词"));
        let _ = AppendMenuW(menu, check(st.level_osd), MENU_LEVEL, w!("音量 / 亮度提示"));
        let _ = AppendMenuW(menu, check(st.hide_in_fullscreen), MENU_FULLSCREEN, w!("全屏时隐藏"));
        if monitors.len() > 1
            && let Ok(sub) = CreatePopupMenu()
        {
            let _ = AppendMenuW(sub, check(st.monitor.is_empty()), MENU_MONITOR, w!("主显示器"));
            for (i, m) in monitors.iter().enumerate() {
                let r = m.rect;
                let main = if m.primary { " · 主" } else { "" };
                let label = format!("显示器 {}（{}×{}）{main}", i + 1, r.right - r.left, r.bottom - r.top);
                let _ = AppendMenuW(sub, check(st.monitor == m.name), MENU_MONITOR + 1 + i, &HSTRING::from(label));
            }
            let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, w!("显示在"));
        }
        let _ = AppendMenuW(menu, check(settings::autostart_enabled()), MENU_AUTOSTART, w!("开机自启"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("退出灵动岛"));
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        // 菜单要求窗口在前台，否则点别处不会消失
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, None, hwnd, None);
        let _ = DestroyMenu(menu);
        let cmd = cmd.0 as usize;
        match cmd {
            0 => return,
            MENU_QUIT => {
                let _ = DestroyWindow(hwnd);
                return;
            }
            MENU_AUTOSTART => {
                settings::set_autostart(!settings::autostart_enabled());
                return;
            }
            _ => {}
        }
        with_app(|a| {
            match cmd {
                MENU_CLEAR_SHELF => a.island.shelf.clear(),
                MENU_LYRICS => {
                    a.settings.lyrics = !a.settings.lyrics;
                    a.island.media.send(Cmd::Lyrics(a.settings.lyrics));
                }
                MENU_LEVEL => a.settings.level_osd = !a.settings.level_osd,
                MENU_FULLSCREEN => a.settings.hide_in_fullscreen = !a.settings.hide_in_fullscreen,
                MENU_MONITOR => {
                    a.settings.monitor.clear();
                    a.reposition();
                }
                c if c > MENU_MONITOR => {
                    if let Some(m) = monitors.get(c - MENU_MONITOR - 1) {
                        // 选的就是主显示器时存空值，换主显示器后也跟着走
                        a.settings.monitor = if m.primary { String::new() } else { m.name.clone() };
                        a.reposition();
                    }
                }
                _ => {}
            }
            a.settings.save();
            a.island.dirty = true;
        });
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_MOUSEMOVE => {
            let (x, y) = lparam_xy(lp);
            with_app(|a| {
                let (x, y) = a.to_logical(x, y);
                match msg {
                    WM_LBUTTONDOWN => {
                        // 拖进度条时光标可能移出窗口，捕获鼠标
                        if a.island.mouse_down(x, y) {
                            unsafe { SetCapture(hwnd) };
                        }
                    }
                    WM_LBUTTONUP => {
                        a.island.mouse_up(x, y);
                        let _ = unsafe { ReleaseCapture() };
                    }
                    _ => a.island.mouse_move(x, y),
                }
            });
            return LRESULT(0);
        }
        WM_CAPTURECHANGED => {
            // 捕获被别人抢走：当作松手
            with_app(|a| {
                if a.island.is_scrubbing() {
                    a.island.mouse_up(-1.0, -1.0);
                }
            });
            return LRESULT(0);
        }
        WM_MOUSEWHEEL => {
            let delta = (wp.0 >> 16) as u16 as i16 as i32;
            with_app(|a| a.island.wheel(delta));
            return LRESULT(0);
        }
        WM_APP_TRAY => {
            let event = (lp.0 & 0xffff) as u32;
            if event == WM_RBUTTONUP || event == WM_LBUTTONUP {
                show_menu(hwnd);
            }
            return LRESULT(0);
        }
        WM_POWERBROADCAST => {
            match wp.0 as u32 {
                PBT_APMPOWERSTATUSCHANGE => {
                    with_app(|a| a.power_changed = Instant::now());
                }
                PBT_POWERSETTINGCHANGE => {
                    let setting = unsafe { (lp.0 as *const POWERBROADCAST_SETTING).as_ref() };
                    if let Some(s) = setting
                        && s.PowerSetting == GUID_VIDEO_CURRENT_MONITOR_BRIGHTNESS
                        && s.DataLength >= 4
                    {
                        let v = unsafe { std::ptr::read_unaligned(s.Data.as_ptr() as *const u32) };
                        with_app(|a| a.on_brightness(v));
                    }
                }
                _ => {}
            }
            return LRESULT(1);
        }
        WM_RBUTTONUP => {
            show_menu(hwnd);
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == TIMER_DRAG => {
            with_app(|a| a.step());
            return LRESULT(0);
        }
        WM_DPICHANGED | WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            with_app(|a| a.reposition());
            return LRESULT(0);
        }
        WM_APP_WAKE => return LRESULT(0),
        WM_DESTROY => {
            unsafe {
                let _ = RevokeDragDrop(hwnd);
                PostQuitMessage(0);
            }
            return LRESULT(0);
        }
        m if m != 0 && m == TASKBAR_CREATED.get() => {
            // 资源管理器重启后托盘图标要重新加
            with_app(|a| a.tray.add());
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn main() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // 拖放需要 OLE（单线程套间）
        OleInitialize(None)?;

        // 单实例
        let _mutex = CreateMutexW(None, true, w!("Local\\WinDynamicIsland"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }

        let hinst = GetModuleHandleW(None)?;
        let class = w!("WinDynamicIsland");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));

        let ex = WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP | WS_EX_LAYERED | WS_EX_TRANSPARENT;
        let hwnd = CreateWindowExW(ex, class, w!("灵动岛"), WS_POPUP, 0, 0, 1, 1, None, None, Some(hinst.into()), None)?;
        // 分层窗口需要设一次属性才会显示；配合 WS_EX_TRANSPARENT 实现跨进程点击穿透
        SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA)?;
        RegisterDragDrop(hwnd, &DropTarget::create(hwnd, Sink))?;

        #[allow(unused_mut)]
        let mut app = App::new(hwnd)?;
        // 调试版专用：ISLAND_DEBUG_DROP=1 直接显示放置区，方便截图检查样式
        #[cfg(debug_assertions)]
        if std::env::var_os("ISLAND_DEBUG_DROP").is_some() {
            app.island.set_drop_hover(true);
        }
        #[cfg(debug_assertions)]
        if let Some(p) = std::env::var_os("ISLAND_DEBUG") {
            app.island.debug_preset(&p.to_string_lossy());
        }
        APP.with(|a| *a.borrow_mut() = Some(app));
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        let mut msg = MSG::default();
        loop {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let wait = with_app(|a| a.step()).unwrap_or(100);
            if let Some(path) = with_app(|a| a.island.take_drag()).flatten() {
                drag_out(hwnd, path);
                continue;
            }
            if wait > 0 {
                MsgWaitForMultipleObjects(None, false, wait, QS_ALLINPUT);
            }
        }
    }
}
