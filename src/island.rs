//! 灵动岛本体：状态机、弹簧形变、各状态的内容绘制、命中测试。
//! 所有坐标都是逻辑像素（DIP），窗口内坐标系。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::UI::WindowsAndMessaging::MB_ICONASTERISK;
use windows_numerics::{Matrix3x2, Vector2};

use crate::media::{ART, Cmd, Media, MediaInfo};
use crate::notes::{self, Notes};
use crate::privacy::InUse;
use crate::render::{Color, Image, Rect, Renderer, rgba};
use crate::shelf::{self, ICON, Shelf};
use crate::spring::Spring;
use crate::system::{self, Battery};
use crate::weather::{self, Icon};

/// 窗口逻辑尺寸：容纳最大展开态 + 回弹余量。
pub const WIN_W: f32 = 460.0;
pub const WIN_H: f32 = 340.0;
const TOP: f32 = 8.0;
/// 展开态底部暂存区那一行的高度。
const SHELF_ROW: f32 = 92.0;
/// 展开态媒体页的歌词行高度（单行 / 带翻译两行）。
const LYRIC_ROW: f32 = 28.0;
const LYRIC_ROW2: f32 = 46.0;
/// 便签页每条的行高。
const NOTE_ROW: f32 = 28.0;
/// 便签页一次显示几条，多了用滚轮翻。
const NOTE_ROWS: usize = 5;
/// 展开态的宽度。
const EXPANDED_W: f32 = 400.0;
/// 有两个播放器时，右边小圆泡和岛之间的空隙；岛最宽多少（给小圆泡留位置）。
const BUBBLE_GAP: f32 = 6.0;
const SPLIT_MAX_W: f32 = 380.0;
/// 滚轮翻页的间隔，免得触控板一划翻过好几页。
const PAGE_DEBOUNCE: Duration = Duration::from_millis(300);
/// 细条形态贴着屏幕顶边。
const SLIM_TOP: f32 = 2.0;
const TILE_W: f32 = 68.0;
const TILE_GAP: f32 = 4.0;
/// 按住文件拖多远算"拖出去"。
const DRAG_SLOP: f32 = 5.0;

const HOVER_DELAY: Duration = Duration::from_millis(140);
const LEAVE_DELAY: Duration = Duration::from_millis(320);
const PAUSED_LINGER: Duration = Duration::from_secs(30);
const LYRIC_ANIM: f32 = 0.28;
/// 收起时鼠标指上去，岛先鼓起来这么多。
const HOVER_LIFT: f32 = 1.04;
/// 翻页时内容滑动的距离。
const SLIDE: f32 = 26.0;

const BLACK: Color = rgba(0.0, 0.0, 0.0, 1.0);
const WHITE: Color = rgba(1.0, 1.0, 1.0, 1.0);
const GREEN: Color = rgba(0.29, 0.86, 0.39, 1.0);
const BLUE: Color = rgba(0.30, 0.62, 1.0, 1.0);
const ORANGE: Color = rgba(1.0, 0.62, 0.04, 1.0);
const RED: Color = rgba(1.0, 0.27, 0.23, 1.0);
const YELLOW: Color = rgba(1.0, 0.8, 0.2, 1.0);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    /// files：暂存区里有文件，药丸上显示个数；slim：收成贴顶的细条，少挡浏览器标签页。
    Idle { files: bool, slim: bool },
    /// lyric：加宽，中间显示当前歌词。
    Media { lyric: bool },
    TrackToast,
    Charging,
    /// 音量 / 亮度条。
    Level,
    /// 计时器倒计时（没有音乐时）。
    Timer,
    TimerDone,
    LowBattery,
    /// 默认输出设备换了（插耳机、连蓝牙耳机）。
    Device,
    /// 有应用开始用麦克风 / 摄像头。
    Privacy,
    /// 正在往岛上拖文件。
    Drop,
    /// shelf：底部多一行暂存的文件；lyric：歌词行数（0 没有，1 原文，2 原文 + 翻译）。
    ExpandedMedia { shelf: bool, lyric: u8 },
    ExpandedDash { shelf: bool },
    /// rows：便签行数（含正在输入的那一行）。
    ExpandedNotes { shelf: bool, rows: u8 },
}

impl Layout {
    /// (宽, 高, 圆角)
    pub fn dims(self) -> (f32, f32, f32) {
        let row = |s: bool| if s { SHELF_ROW } else { 0.0 };
        match self {
            Layout::Idle { slim: true, .. } => (100.0, 5.0, 2.5),
            Layout::Idle { files: false, .. } => (120.0, 30.0, 15.0),
            Layout::Idle { files: true, .. } => (150.0, 30.0, 15.0),
            Layout::Media { lyric: false } => (250.0, 34.0, 17.0),
            Layout::Media { lyric: true } => (420.0, 34.0, 17.0),
            Layout::TrackToast => (360.0, 34.0, 17.0),
            Layout::Charging => (300.0, 34.0, 17.0),
            Layout::Level => (320.0, 34.0, 17.0),
            Layout::Timer => (190.0, 34.0, 17.0),
            Layout::TimerDone => (300.0, 34.0, 17.0),
            Layout::LowBattery => (300.0, 34.0, 17.0),
            Layout::Device | Layout::Privacy => (350.0, 34.0, 17.0),
            Layout::Drop => (400.0, 120.0, 40.0),
            Layout::ExpandedMedia { shelf, .. } => (EXPANDED_W, self.base_height() + row(shelf), 40.0),
            Layout::ExpandedDash { shelf } | Layout::ExpandedNotes { shelf, .. } => {
                (EXPANDED_W, self.base_height() + row(shelf), 36.0)
            }
        }
    }

    /// 展开态上半部分（不含暂存区那一行）的高度。
    fn base_height(self) -> f32 {
        match self {
            Layout::ExpandedMedia { lyric, .. } => {
                150.0
                    + match lyric {
                        0 => 0.0,
                        1 => LYRIC_ROW,
                        _ => LYRIC_ROW2,
                    }
            }
            Layout::ExpandedDash { .. } => 150.0,
            Layout::ExpandedNotes { rows, .. } => (62.0 + rows as f32 * NOTE_ROW + 14.0).max(150.0),
            _ => self.dims().1,
        }
    }

    /// 岛上边缘离屏幕顶的距离。
    fn top(self) -> f32 {
        if matches!(self, Layout::Idle { slim: true, .. }) { SLIM_TOP } else { TOP }
    }

    fn has_shelf(self) -> bool {
        matches!(
            self,
            Layout::ExpandedMedia { shelf: true, .. } | Layout::ExpandedDash { shelf: true } | Layout::ExpandedNotes { shelf: true, .. }
        )
    }

    fn is_expanded(self) -> bool {
        matches!(self, Layout::ExpandedMedia { .. } | Layout::ExpandedDash { .. } | Layout::ExpandedNotes { .. })
    }

    /// 旁边可以挂另一个播放器的小圆泡的紧凑形态。
    fn can_split(self) -> bool {
        matches!(self, Layout::Media { .. } | Layout::TrackToast | Layout::Timer | Layout::Idle { slim: false, .. })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Toast {
    Track,
    Charging,
    Level,
    TimerDone,
    LowBattery,
    Device,
    Privacy,
}

impl Toast {
    fn duration(self) -> Duration {
        match self {
            Toast::Level => Duration::from_millis(1600),
            Toast::TimerDone => Duration::from_secs(6),
            Toast::LowBattery => Duration::from_secs(4),
            Toast::Device | Toast::Privacy => Duration::from_secs(3),
            _ => Duration::from_millis(2800),
        }
    }
}

/// 选择形态时需要的输入。
#[derive(Clone, Copy, Default)]
pub struct State {
    pub expanded: bool,
    pub drop_hover: bool,
    pub toast: Option<Toast>,
    pub media_active: bool,
    pub media_visible: bool,
    pub files: bool,
    /// 展开时看的是哪一页（滚轮或点页码切换）。
    pub page: Page,
    /// 便签页的行数。
    pub notes: u8,
    /// 当前歌曲的歌词：0 没有，1 有，2 有并且带翻译。
    pub lyrics: u8,
    pub timer: bool,
    /// 设置：空闲时收成细条。
    pub slim: bool,
    /// 有应用在用麦克风或摄像头（要显示指示点，不能收成细条）。
    pub privacy: bool,
}

/// 展开态的页：音乐（有播放器时才有）、仪表盘、便签。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Page {
    #[default]
    Media,
    Dash,
    Notes,
}

/// 选择当前应显示的形态。纯函数，方便测试。
pub fn choose_layout(s: State) -> Layout {
    if s.drop_hover {
        return Layout::Drop;
    }
    if s.expanded {
        return match s.page {
            Page::Notes => Layout::ExpandedNotes { shelf: s.files, rows: s.notes },
            Page::Media if s.media_active => Layout::ExpandedMedia { shelf: s.files, lyric: s.lyrics },
            _ => Layout::ExpandedDash { shelf: s.files },
        };
    }
    match s.toast {
        Some(Toast::Level) => return Layout::Level,
        Some(Toast::TimerDone) => return Layout::TimerDone,
        Some(Toast::Privacy) => return Layout::Privacy,
        Some(Toast::LowBattery) => return Layout::LowBattery,
        Some(Toast::Device) => return Layout::Device,
        Some(Toast::Charging) => return Layout::Charging,
        Some(Toast::Track) if s.media_visible => return Layout::TrackToast,
        _ => {}
    }
    if s.media_visible {
        Layout::Media { lyric: s.lyrics > 0 }
    } else if s.timer {
        Layout::Timer
    } else {
        Layout::Idle { files: s.files, slim: s.slim && !s.files && !s.privacy }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Btn {
    Prev,
    Toggle,
    Next,
    StartTimer(u32),
    AddMinute,
    CancelTimer,
    Page,
    Swap,
    NewNote,
    CopyNote(usize),
    DelNote(usize),
    SetCity,
    /// 点封面或歌名：把播放器切到前台。
    OpenPlayer,
    /// 点歌词：从这一句开头重听。
    Lyric,
    /// 点时钟：打开系统「时钟」。
    OpenClock,
    /// 点电量：打开电源和电池设置。
    OpenBattery,
}

/// 岛上弹出的输入框是用来输入什么的。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Input {
    Note,
    City,
}

/// 暂存区里一个文件的格子。
struct Tile {
    id: u64,
    rect: Rect,
    close: Rect,
}

/// 按下时落在了哪里。
#[derive(Clone, Copy, PartialEq, Debug)]
enum PressOn {
    Tile(u64),
    Close(u64),
    Seek,
    Bubble,
    Btn(Btn),
    Other,
}

/// 按钮按下的弹簧：欠阻尼，松手时会略微弹过头。
fn squish_spring() -> Spring {
    Spring::new(0.0, 0.22, 0.55)
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LevelKind {
    Volume,
    Brightness,
}

struct Level {
    kind: LevelKind,
    muted: bool,
    fill: Spring,
}

struct Timer {
    end: Instant,
    total: f32,
}

impl Timer {
    fn remaining(&self, now: Instant) -> f32 {
        self.end.saturating_duration_since(now).as_secs_f32()
    }
}

/// 歌词换行时的上滑过渡。
struct LyricAnim {
    idx: Option<usize>,
    track: u64,
    prev: String,
    prev_trans: String,
    changed: Instant,
}

pub struct Island {
    w: Spring,
    h: Spring,
    r: Spring,
    press: Spring,
    top: Spring,
    target: Layout,
    shown: Layout,
    content_alpha: f32,

    hover_since: Option<Instant>,
    leave_since: Option<Instant>,
    expanded: bool,
    pressed: bool,
    page: Page,
    page_changed: Instant,
    cursor: (f32, f32),

    pub media: Media,
    info: MediaInfo,
    art: Option<(u64, Image)>,
    /// 另一个播放器的封面（小圆泡里画）。
    other_art: Option<(u64, Image)>,
    /// 0..1：小圆泡从岛里分出来的程度。
    split: Spring,
    last_track: u64,
    last_playing: Option<Instant>,
    lyric: LyricAnim,
    seek_bar: Option<Rect>,
    bar_hover: bool,
    /// 正在拖进度条：目标位置占比。
    scrub: Option<f32>,

    battery: Option<Battery>,
    toast: Option<(Toast, Instant)>,
    level: Option<Level>,
    timer: Option<Timer>,
    /// 设置：空闲时收成细条。
    pub slim: bool,
    privacy: InUse,
    privacy_msg: (String, bool),
    /// 设备名、右侧说明（音量或「已连接」）、蓝牙耳机电量（有的话代替说明显示）。
    device: (String, String, Option<u8>),
    pub weather: weather::State,
    pub notes: Notes,
    /// 正在新建便签（输入框是主循环开的独立小窗口，盖在第一行上）。
    editing: Option<Input>,
    input_request: Option<Input>,
    /// 便签页从第几条开始显示。
    note_scroll: usize,
    /// 刚复制的便签：(第几条, 时刻)，显示「已复制」。
    copied: Option<(usize, Instant)>,

    pub shelf: Shelf,
    icons: HashMap<u64, Image>,
    tiles: Vec<Tile>,
    drop_hover: bool,
    /// 指着的文件格子，和是否指在它的关闭按钮上。
    hovered_tile: Option<(u64, bool)>,
    press_on: Option<(PressOn, f32, f32)>,
    /// 要拖出去的文件 id，由主循环在借用之外发起拖放。
    pending_drag: Option<u64>,

    buttons: Vec<(Btn, Rect)>,
    /// 鼠标正指着的按钮（可能有嵌套，比如便签行里的删除）。
    hot: Vec<Btn>,
    /// 按钮悬停高亮的程度（0..1），指上去淡入、离开淡出。
    glow: Vec<(Btn, f32)>,
    /// 最后按下的按钮和它的按压程度（0..1，松手后弹回）。
    squish: (Option<Btn>, Spring),
    /// 进度条粗细：悬停或拖动时变粗。
    bar_thick: Spring,
    /// 小圆泡悬停时放大一点。
    bubble_hot: Spring,
    /// 当前帧内容的变换（岛的缩放、抖动、翻页滑动），按钮自己缩放时叠在它上面。
    base: Matrix3x2,
    /// 翻页方向（+1 往后、-1 往前、0 没在翻），内容跟着左右滑。
    slide: f32,
    /// 滚轮翻到头时岛左右轻轻一顶（水平位移）。
    nudge: Spring,
    /// 调试：保持展开，不因鼠标离开而收起。
    pin: bool,
    /// 调试：用假的媒体信息，不跟系统同步。
    fake_media: bool,
    anim_t: f32,
    last_tick: Instant,
    last_render: Instant,
    pub dirty: bool,
}

/// 下一帧怎么安排。
pub enum Pace {
    /// 动画进行中，按垂直同步连续出帧。
    Animating,
    /// 静态，这么久之后再看一眼（毫秒）。
    Wait(u32),
}

impl Island {
    pub fn new(media: Media, shelf: Shelf, notes: Notes) -> Self {
        let start = Layout::Idle { files: !shelf.items.is_empty(), slim: false };
        let (w, h, r) = start.dims();
        let now = Instant::now();
        Island {
            w: Spring::new(w, 0.42, 0.78),
            h: Spring::new(h, 0.42, 0.82),
            r: Spring::new(r, 0.42, 0.9),
            press: Spring::new(1.0, 0.25, 0.6),
            top: Spring::new(TOP, 0.42, 0.9),
            target: start,
            shown: start,
            content_alpha: 1.0,
            hover_since: None,
            leave_since: None,
            expanded: false,
            pressed: false,
            page: Page::Media,
            page_changed: now,
            cursor: (-1.0, -1.0),
            media,
            info: MediaInfo::default(),
            art: None,
            other_art: None,
            split: Spring::new(0.0, 0.32, 0.85),
            last_track: 0,
            last_playing: None,
            lyric: LyricAnim { idx: None, track: 0, prev: String::new(), prev_trans: String::new(), changed: now },
            seek_bar: None,
            bar_hover: false,
            scrub: None,
            battery: system::battery(),
            toast: None,
            level: None,
            timer: None,
            slim: false,
            privacy: InUse::default(),
            privacy_msg: (String::new(), false),
            device: (String::new(), String::new(), None),
            weather: weather::State::Unknown,
            notes,
            editing: None,
            input_request: None,
            note_scroll: 0,
            copied: None,
            shelf,
            icons: HashMap::new(),
            tiles: Vec::new(),
            drop_hover: false,
            hovered_tile: None,
            press_on: None,
            pending_drag: None,
            buttons: Vec::new(),
            hot: Vec::new(),
            glow: Vec::new(),
            squish: (None, squish_spring()),
            bar_thick: Spring::new(5.0, 0.25, 0.8),
            bubble_hot: Spring::new(1.0, 0.3, 0.6),
            base: Matrix3x2::identity(),
            slide: 0.0,
            nudge: Spring::new(0.0, 0.32, 0.45),
            pin: false,
            fake_media: false,
            anim_t: 0.0,
            last_tick: now,
            last_render: now,
            dirty: true,
        }
    }

    /// 岛当前（动画中）的外框。
    pub fn rect(&self) -> Rect {
        let (w, h) = (self.w.value, self.h.value);
        // 有小圆泡时整组居中，岛往左让出一半位置
        let shift = self.split.value.clamp(0.0, 1.0) * (BUBBLE_GAP + h) / 2.0;
        Rect::new((WIN_W - w) / 2.0 - shift, self.top.value, w, h)
    }

    /// 另一个播放器的小圆泡：从岛的右端分出来，往右滑到旁边。
    fn bubble_rect(&self) -> Option<Rect> {
        let s = self.split.value;
        if s < 0.02 {
            return None;
        }
        let m = self.rect();
        let d = m.h;
        let cx = m.x + m.w - d / 2.0 + s * (BUBBLE_GAP + d);
        let k = d * (0.5 + 0.5 * s.min(1.0));
        Some(Rect::new(cx - k / 2.0, m.y + (d - k) / 2.0, k, k))
    }

    fn on_bubble(&self, x: f32, y: f32) -> bool {
        self.split.target > 0.5 && self.bubble_rect().is_some_and(|b| b.inflate(3.0).contains(x, y))
    }

    /// 鼠标是否在岛上（用于决定是否穿透点击）。
    pub fn hit(&self, x: f32, y: f32) -> bool {
        self.rect().inflate(3.0).contains(x, y) || self.on_bubble(x, y)
    }

    /// 某个形态的目标尺寸：旁边挂着小圆泡时岛不能太宽。
    fn dims_of(&self, l: Layout) -> (f32, f32, f32) {
        let (w, h, r) = l.dims();
        if self.split.target > 0.5 { (w.min(SPLIT_MAX_W), h, r) } else { (w, h, r) }
    }

    /// 有几页、当前是第几页。
    fn pages(&self) -> Vec<Page> {
        if self.info.active { vec![Page::Media, Page::Dash, Page::Notes] } else { vec![Page::Dash, Page::Notes] }
    }

    fn page_index(&self) -> usize {
        let pages = self.pages();
        let cur = if self.page == Page::Media && !self.info.active { Page::Dash } else { self.page };
        pages.iter().position(|p| *p == cur).unwrap_or(0)
    }

    pub fn is_scrubbing(&self) -> bool {
        self.scrub.is_some()
    }

    pub fn poll_system(&mut self) {
        let now_batt = system::battery();
        if let (Some(old), Some(new)) = (self.battery, now_batt) {
            if !old.charging && new.charging {
                self.show_toast(Toast::Charging);
            }
            // 没插电时降到 20%、10% 各提醒一次
            if !new.charging && [20, 10].iter().any(|&t| old.percent > t && new.percent <= t) {
                self.show_toast(Toast::LowBattery);
            }
            if old != new {
                self.dirty = true;
            }
        }
        self.battery = now_batt;
    }

    fn show_toast(&mut self, t: Toast) {
        self.toast = Some((t, Instant::now()));
        self.dirty = true;
    }

    // ---------- 鼠标 ----------

    pub fn on_mouse(&mut self, inside: bool, cursor: (f32, f32), now: Instant) {
        // 便签行的悬停、进度条上的时间气泡都跟着光标走
        if self.expanded && cursor != self.cursor && (matches!(self.shown, Layout::ExpandedNotes { .. }) || self.bar_hover) {
            self.dirty = true;
        }
        self.cursor = cursor;
        // 指着小圆泡时不展开（要能点它），也不收起
        if !self.expanded && inside && self.on_bubble(cursor.0, cursor.1) {
            self.hover_since = None;
            self.leave_since = None;
            return;
        }
        let hovered = if inside { self.tile_at(cursor.0, cursor.1) } else { None };
        if hovered != self.hovered_tile {
            self.hovered_tile = hovered;
            self.dirty = true;
        }
        // 收起时指上去先微微鼓起来，紧接着再展开
        if !self.pressed {
            self.press.target = if inside && !self.expanded { HOVER_LIFT } else { 1.0 };
        }
        let bar_hover = inside && self.seek_bar.is_some_and(|b| seek_zone(b).contains(cursor.0, cursor.1)) && self.info.can_seek;
        if bar_hover != self.bar_hover {
            self.bar_hover = bar_hover;
            self.dirty = true;
        }
        if inside || self.scrub.is_some() {
            self.leave_since = None;
            let since = *self.hover_since.get_or_insert(now);
            if !self.expanded && now - since >= HOVER_DELAY {
                self.expand();
            }
        } else {
            self.hover_since = None;
            if self.pressed {
                self.pressed = false;
                self.press.target = 1.0;
                self.squish.1.target = 0.0;
            }
            if self.expanded && !self.pin && self.editing.is_none() {
                let since = *self.leave_since.get_or_insert(now);
                if now - since >= LEAVE_DELAY {
                    // 页面和便签滚动位置都留着，下次展开还在这里
                    self.expanded = false;
                    self.leave_since = None;
                }
            }
        }
    }

    fn expand(&mut self) {
        self.expanded = true;
        // 暂存的文件可能已经被移走或删掉
        if self.shelf.prune_missing() {
            self.dirty = true;
        }
    }

    /// 返回 (文件 id, 是否点在关闭按钮上)。
    fn tile_at(&self, x: f32, y: f32) -> Option<(u64, bool)> {
        let t = self.tiles.iter().find(|t| t.rect.contains(x, y) || t.close.contains(x, y))?;
        Some((t.id, t.close.contains(x, y)))
    }

    /// 跳回当前这句歌词的开头。
    fn replay_line(&mut self) {
        let t = self.info.lyrics.as_ref().zip(self.lyric.idx).and_then(|(l, i)| l.lines.get(i)).map(|l| l.t);
        if let Some(t) = t
            && self.info.can_seek
        {
            // 稍微往后一点：播放器跳过去常常差几十毫秒，正好落在上一句末尾会闪一下上一句
            let t = t + 0.05;
            self.media.send(Cmd::Seek(t));
            // 乐观更新：进度和歌词立刻跳回去
            self.info.position = t;
            self.info.sampled_at = Some(Instant::now());
        }
    }

    /// 展开态下 (x, y) 处的按钮（嵌套时取先画的那个，和点击判定一致）。
    fn btn_at(&self, x: f32, y: f32) -> Option<Btn> {
        if !self.expanded {
            return None;
        }
        self.buttons.iter().find(|(_, r)| r.contains(x, y)).map(|b| b.0)
    }

    /// 光标在这里时显示成手形：点了会有反应的地方。
    pub fn clickable(&self, x: f32, y: f32) -> bool {
        if self.scrub.is_some() || (!self.expanded && self.on_bubble(x, y)) {
            return true;
        }
        self.expanded
            && (self.btn_at(x, y).is_some()
                || self.tile_at(x, y).is_some()
                || (self.info.can_seek && self.seek_bar.is_some_and(|b| seek_zone(b).contains(x, y))))
    }

    fn scrub_frac(&self, x: f32) -> f32 {
        self.seek_bar.map(|b| ((x - b.x) / b.w).clamp(0.0, 1.0)).unwrap_or(0.0)
    }

    /// 返回 true 表示要捕获鼠标（拖进度条时光标可能移出窗口）。
    pub fn mouse_down(&mut self, x: f32, y: f32) -> bool {
        self.pressed = true;
        let on_bar = matches!(self.shown, Layout::ExpandedMedia { .. })
            && self.info.can_seek
            && self.info.duration > 1.0
            && self.seek_bar.is_some_and(|b| seek_zone(b).contains(x, y));
        let on = if on_bar {
            self.scrub = Some(self.scrub_frac(x));
            self.dirty = true;
            PressOn::Seek
        } else if !self.expanded && self.on_bubble(x, y) {
            PressOn::Bubble
        } else if let Some(b) = self.btn_at(x, y) {
            if self.squish.0 != Some(b) {
                self.squish = (Some(b), squish_spring());
            }
            self.squish.1.target = 1.0;
            PressOn::Btn(b)
        } else {
            match self.tile_at(x, y) {
                Some((id, true)) => PressOn::Close(id),
                Some((id, false)) => PressOn::Tile(id),
                None => PressOn::Other,
            }
        };
        // 按在按钮、文件或进度条上时岛不缩，只有按钮自己缩
        if on == PressOn::Other {
            // 展开后岛很大，缩同样的比例会显得太猛
            self.press.target = if self.expanded { 0.985 } else { 0.95 };
        }
        self.press_on = Some((on, x, y));
        self.dirty = true;
        on_bar
    }

    pub fn mouse_move(&mut self, x: f32, y: f32) {
        if self.scrub.is_some() {
            self.scrub = Some(self.scrub_frac(x));
            self.dirty = true;
            return;
        }
        // 按着按钮移开：按钮先弹回，松手也不触发；移回来再按下去
        if let Some((PressOn::Btn(b), ..)) = self.press_on {
            self.squish.1.target = if self.btn_at(x, y) == Some(b) { 1.0 } else { 0.0 };
        }
        if let Some((PressOn::Tile(id), px, py)) = self.press_on
            && (x - px).hypot(y - py) >= DRAG_SLOP
        {
            self.press_on = None;
            self.pressed = false;
            self.press.target = 1.0;
            self.pending_drag = Some(id);
        }
    }

    /// 滚轮翻页：往下滚到下一页，往上滚到上一页。便签多的时候先滚便签，滚到头再翻页。
    pub fn wheel(&mut self, delta: i32) {
        if !self.expanded || self.editing.is_some() {
            return;
        }
        let now = Instant::now();
        if self.page == Page::Notes {
            let max = self.notes.items.len().saturating_sub(NOTE_ROWS);
            let next = if delta < 0 { (self.note_scroll + 1).min(max) } else { self.note_scroll.saturating_sub(1) };
            if next != self.note_scroll {
                self.note_scroll = next;
                // 滚完停一下才会翻页，免得一路滚过头
                self.page_changed = now;
                self.dirty = true;
                return;
            }
        }
        if now - self.page_changed < PAGE_DEBOUNCE {
            return;
        }
        let pages = self.pages();
        let i = self.page_index();
        let j = if delta < 0 { (i + 1).min(pages.len() - 1) } else { i.saturating_sub(1) };
        if j != i {
            self.page = pages[j];
            self.page_changed = Instant::now();
            self.slide = if j > i { 1.0 } else { -1.0 };
        } else {
            // 翻到头了：往滚动方向顶一下再弹回来
            self.page_changed = now;
            self.nudge.velocity += if delta < 0 { -170.0 } else { 170.0 };
        }
        self.dirty = true;
    }

    pub fn take_drag(&mut self) -> Option<PathBuf> {
        let id = self.pending_drag.take()?;
        self.shelf.get(id).map(|i| i.path.clone())
    }

    /// 往外拖结束：文件被移走的话就从暂存区去掉。
    pub fn drag_out_done(&mut self) {
        self.shelf.prune_missing();
        self.dirty = true;
    }

    pub fn set_drop_hover(&mut self, over: bool) {
        self.drop_hover = over;
        self.dirty = true;
    }

    pub fn add_files(&mut self, paths: Vec<PathBuf>) {
        if self.shelf.add(paths, true) > 0 {
            // 放下后直接展开给用户看一眼
            self.expand();
            self.hover_since = Some(Instant::now());
            self.dirty = true;
        }
    }

    pub fn mouse_up(&mut self, x: f32, y: f32) {
        #[cfg(debug_assertions)]
        if std::env::var_os("ISLAND_TRACE").is_some() {
            eprintln!("[click] up at ({x:.0}, {y:.0}) expanded={} buttons={:?}", self.expanded, self.buttons.iter().map(|b| b.0).collect::<Vec<_>>());
        }
        self.pressed = false;
        self.press.target = 1.0;
        self.squish.1.target = 0.0;
        let press = self.press_on.take();
        self.dirty = true;
        if let Some(frac) = self.scrub.take() {
            let target = frac as f64 * self.info.duration;
            self.media.send(Cmd::Seek(target));
            // 乐观更新，免得松手后进度条先跳回去
            self.info.position = target;
            self.info.sampled_at = Some(Instant::now());
            self.dirty = true;
            return;
        }
        if let Some((PressOn::Bubble, ..)) = press {
            if self.on_bubble(x, y) {
                self.media.send(Cmd::Swap);
            }
            return;
        }
        if !self.expanded {
            // 点击直接展开，不用等悬停
            self.expand();
            return;
        }
        match (press, self.tile_at(x, y)) {
            (Some((PressOn::Close(a), ..)), Some((b, true))) if a == b => {
                self.shelf.remove(a);
                self.dirty = true;
                return;
            }
            (Some((PressOn::Tile(a), ..)), Some((b, false))) if a == b => {
                if let Some(item) = self.shelf.get(a) {
                    shelf::open(&item.path);
                }
                return;
            }
            _ => {}
        }
        let Some(&(b, _)) = self.buttons.iter().find(|(_, r)| r.contains(x, y)) else { return };
        // 在一个按钮上按下、到另一个上松手，不算点击
        if let Some((PressOn::Btn(a), ..)) = press
            && a != b
        {
            return;
        }
        match b {
            Btn::Prev => self.media.send(Cmd::Prev),
            Btn::Next => self.media.send(Cmd::Next),
            Btn::Toggle => {
                self.media.send(Cmd::Toggle);
                // 乐观更新，让按钮立刻有反馈
                self.info.playing = !self.info.playing;
            }
            Btn::StartTimer(secs) => self.timer = Some(Timer { end: Instant::now() + Duration::from_secs(secs as u64), total: secs as f32 }),
            Btn::AddMinute => {
                if let Some(t) = &mut self.timer {
                    t.end += Duration::from_secs(60);
                    t.total += 60.0;
                }
            }
            Btn::CancelTimer => self.timer = None,
            Btn::Page => {
                let pages = self.pages();
                let i = self.page_index();
                let j = (i + 1) % pages.len();
                self.page = pages[j];
                self.page_changed = Instant::now();
                self.slide = if j > i { 1.0 } else { -1.0 };
            }
            Btn::Swap => self.media.send(Cmd::Swap),
            Btn::NewNote => self.request_input(Input::Note),
            Btn::SetCity => self.request_input(Input::City),
            Btn::OpenPlayer => system::focus_app(&self.info.app_id),
            Btn::Lyric => self.replay_line(),
            Btn::OpenClock => system::open_uri("ms-clock:"),
            Btn::OpenBattery => system::open_uri("ms-settings:batterysaver"),
            Btn::CopyNote(i) => {
                if let Some(t) = self.notes.items.get(i)
                    && notes::copy(t)
                {
                    self.copied = Some((i, Instant::now()));
                }
            }
            Btn::DelNote(i) => {
                self.notes.remove(i);
                self.note_scroll = self.note_scroll.min(self.notes.items.len().saturating_sub(NOTE_ROWS));
                self.copied = None;
            }
        }
        self.dirty = true;
    }

    // ---------- 外部事件 ----------

    pub fn show_level(&mut self, kind: LevelKind, value: f32, muted: bool) {
        let value = value.clamp(0.0, 1.0);
        let showing = matches!(self.toast, Some((Toast::Level, _)));
        match &mut self.level {
            Some(l) if showing && l.kind == kind => {
                l.fill.target = value;
                l.muted = muted;
            }
            _ => {
                let mut fill = Spring::new(value, 0.3, 1.0);
                fill.target = value;
                self.level = Some(Level { kind, muted, fill });
            }
        }
        self.show_toast(Toast::Level);
    }

    /// 调试版专用：ISLAND_DEBUG=level|bright|timer|dash 直接进入某个形态，方便截图。
    #[cfg(debug_assertions)]
    pub fn debug_preset(&mut self, name: &str) {
        // 截图用的假封面：两色对角渐变加一个柔光圆，BGRA 不透明
        fn fake_art(a: [u8; 3], b: [u8; 3]) -> Vec<u8> {
            let n = ART as usize;
            let mut px = Vec::with_capacity(n * n * 4);
            for y in 0..n {
                for x in 0..n {
                    let t = (x + y) as f32 / (2 * n - 2) as f32;
                    let (dx, dy) = (x as f32 - n as f32 * 0.68, y as f32 - n as f32 * 0.32);
                    let glow = (1.0 - (dx * dx + dy * dy).sqrt() / (n as f32 * 0.42)).clamp(0.0, 1.0).powi(2) * 0.55;
                    let c = |i: usize| {
                        let v = a[i] as f32 * (1.0 - t) + b[i] as f32 * t;
                        (v + (255.0 - v) * glow) as u8
                    };
                    px.extend_from_slice(&[c(2), c(1), c(0), 255]);
                }
            }
            px
        }

        match name {
            "level" => self.show_level(LevelKind::Volume, 0.62, false),
            "bright" => self.show_level(LevelKind::Brightness, 0.4, false),
            "timer" => self.timer = Some(Timer { end: Instant::now() + Duration::from_secs(300), total: 300.0 }),
            "dash" => {
                self.timer = Some(Timer { end: Instant::now() + Duration::from_secs(300), total: 600.0 });
                self.expanded = true;
                self.pin = true;
            }
            "media" | "media_x" => {
                let lrc = "[01:30.00]路灯一盏一盏亮起来
[01:34.50]晚风把云吹成海
[01:38.00]我数着站台等你来
[01:41.50]这城市刚好不太快";
                self.info = MediaInfo {
                    active: true,
                    title: "晚风站台".into(),
                    artist: "示例歌手".into(),
                    playing: true,
                    position: 95.0,
                    sampled_at: Some(Instant::now()),
                    duration: 269.0,
                    can_seek: true,
                    lyrics: Some(std::sync::Arc::new(crate::lyrics::build(lrc, None))),
                    track_id: 7,
                    app_id: "Demo.exe".into(),
                    art: Some(std::sync::Arc::new(fake_art([255, 138, 101], [94, 53, 177]))),
                    ..Default::default()
                };
                self.info.accent = crate::media::accent_of(self.info.art.as_deref().unwrap());
                self.last_track = 7;
                self.fake_media = true;
                if name == "media_x" {
                    self.expanded = true;
                    self.pin = true;
                    self.bar_hover = true;
                }
            }
            "trans" => {
                self.debug_preset("media_x");
                let lrc = "[01:30.00]Paper boats on a silver tide\n[01:34.50]Carry every word I never said\n[01:38.00]Somewhere the morning waits for us";
                let tl = "[01:30.00]纸船漂在银色的潮水上\n[01:34.50]载着我没说出口的话\n[01:38.00]清晨在某处等着我们";
                self.info.lyrics = Some(std::sync::Arc::new(crate::lyrics::build(lrc, Some(tl))));
                self.info.title = "Paper Boats".into();
                self.info.artist = "Demo Artist".into();
            }
            "privacy" => self.set_privacy(InUse { mic: vec!["WeChat".into()], cam: vec![] }),
            "dots" => {
                self.set_privacy(InUse { mic: vec!["WeChat".into()], cam: vec!["Zoom".into()] });
                self.toast = None;
            }
            "device" => self.show_device("耳机 (WH-1000XM5)".into(), "36%".into(), None),
            "connected" => self.show_device("耳机 (EDIFIER W830NB)".into(), "已连接".into(), Some(80)),
            "lowbatt" => {
                self.battery = Some(Battery { percent: 19, charging: false });
                self.show_toast(Toast::LowBattery);
            }
            "slim" => self.slim = true,
            "notes" => {
                self.notes.items = ["周五前把报销单交了", "灵动岛：试试把天气放到细条上", "买咖啡豆", "周末看《沙丘》", "给妈妈打电话", "整理照片", "续费域名"]
                    .map(String::from)
                    .to_vec();
                self.page = Page::Notes;
                self.expanded = true;
                self.pin = true;
                self.cursor = (230.0, 8.0 + 50.0 + NOTE_ROW * 1.5);
            }
            "weather" => {
                self.weather = weather::State::Ready(weather::Now { temp: 17.4, hi: 20.0, lo: 11.0, desc: "多云", icon: Icon::PartlyCloudy, is_day: true, city: Some("上海".into()) });
                self.page = Page::Dash;
                self.expanded = true;
                self.pin = true;
            }
            "split" | "split_x" => {
                self.debug_preset("media");
                self.info.other = Some(crate::media::Other { app: "Chrome".into(), title: "视频".into(), art: Some(std::sync::Arc::new(fake_art([77, 208, 225], [26, 35, 126]))), art_id: 1 });
                self.info.app = "网易云音乐".into();
                if name == "split_x" {
                    self.expanded = true;
                    self.pin = true;
                }
            }
            "dash0" => {
                self.expanded = true;
                self.pin = true;
            }
            _ => {}
        }
        self.dirty = true;
    }

    /// 输出设备换了或连上了：显示设备名和右侧说明；蓝牙耳机显示电量。
    pub fn show_device(&mut self, name: String, note: String, battery: Option<u8>) {
        self.device = (name, note, battery);
        self.show_toast(Toast::Device);
    }

    fn request_input(&mut self, kind: Input) {
        if self.editing.is_none() {
            self.editing = Some(kind);
            self.input_request = Some(kind);
            self.note_scroll = 0;
        }
    }

    /// 要弹输入框时返回 (用途, 位置：窗口内逻辑坐标)，只返回一次。
    pub fn take_input_request(&mut self) -> Option<(Input, Rect)> {
        let kind = self.input_request.take()?;
        // 展开态总是宽 EXPANDED_W、贴着 TOP
        let r = Rect::new((WIN_W - EXPANDED_W) / 2.0, TOP, EXPANDED_W, 0.0);
        let at = match kind {
            Input::Note => note_row(r, 0).inflate(-2.0),
            Input::City => city_field(r),
        };
        Some((kind, at))
    }

    /// 输入结束：text 为 None 表示取消。城市由主循环存进设置，这里只管便签。
    pub fn end_input(&mut self, kind: Input, text: Option<&str>) {
        self.editing = None;
        if let (Input::Note, Some(t)) = (kind, text) {
            self.notes.add(t);
        }
        self.copied = None;
        self.dirty = true;
    }

    /// 麦克风 / 摄像头占用情况变了；有新应用开始用时弹一下。
    pub fn set_privacy(&mut self, now: InUse) {
        #[cfg(debug_assertions)]
        if std::env::var_os("ISLAND_TRACE").is_some() {
            eprintln!("[privacy] {now:?}");
        }
        let new_cam = now.cam.iter().find(|a| !self.privacy.cam.contains(a));
        let new_mic = now.mic.iter().find(|a| !self.privacy.mic.contains(a));
        if let Some(app) = new_cam {
            self.privacy_msg = (format!("{app} 正在使用摄像头"), true);
            self.show_toast(Toast::Privacy);
        } else if let Some(app) = new_mic {
            self.privacy_msg = (format!("{app} 正在使用麦克风"), false);
            self.show_toast(Toast::Privacy);
        }
        self.privacy = now;
        self.dirty = true;
    }

    // ---------- 每帧 ----------

    pub fn tick(&mut self, now: Instant) {
        let dt = (now - self.last_tick).as_secs_f32();
        self.last_tick = now;

        if self.fake_media {
            self.info.sampled_at = Some(now - Duration::from_secs_f64(self.info.position - 95.0));
        } else if let Some(info) = self.media.snapshot_if_newer(self.info.version) {
            if info.track_id != self.last_track {
                if self.last_track != 0 && info.playing && info.active {
                    self.show_toast(Toast::Track);
                }
                self.last_track = info.track_id;
            }
            self.info = info;
            self.dirty = true;
        }
        if self.info.playing {
            self.last_playing = Some(now);
        }
        if let Some((t, at)) = self.toast
            && now - at >= t.duration()
        {
            self.toast = None;
        }
        if self.timer.as_ref().is_some_and(|t| t.remaining(now) <= 0.0) {
            self.timer = None;
            self.show_toast(Toast::TimerDone);
            unsafe {
                let _ = MessageBeep(MB_ICONASTERISK);
            }
        }
        self.update_lyric(now);

        let media_visible = self.info.active && self.last_playing.is_some_and(|t| now - t < PAUSED_LINGER);
        let target = choose_layout(State {
            expanded: self.expanded,
            drop_hover: self.drop_hover,
            toast: self.toast.map(|t| t.0),
            media_active: self.info.active,
            media_visible,
            files: !self.shelf.items.is_empty(),
            page: self.page,
            notes: (self.notes.items.len() + (self.editing == Some(Input::Note)) as usize).min(NOTE_ROWS) as u8,
            lyrics: match &self.info.lyrics {
                None => 0,
                Some(l) if l.lines.iter().any(|x| x.trans.is_some()) => 2,
                Some(_) => 1,
            },
            timer: self.timer.is_some(),
            slim: self.slim,
            privacy: self.privacy.any(),
        });
        let split = self.info.other.is_some() && target.can_split();
        let split_t = if split { 1.0 } else { 0.0 };
        let split_changed = self.split.target != split_t;
        self.split.target = split_t;
        if target != self.target || split_changed {
            self.target = target;
            let (w, h, r) = self.dims_of(target);
            let grow = w > self.w.target + 1.0 || h > self.h.target + 1.0;
            let (resp, damp) = if grow { (0.44, 0.7) } else { (0.36, 0.9) };
            self.w.tune(resp, damp);
            self.h.tune(resp, (damp + 0.05).min(1.0));
            self.w.target = w;
            self.h.target = h;
            self.r.target = r;
            self.top.target = target.top();
        }

        // 内容先淡出，换成新形态的内容后，随尺寸接近目标再淡入。
        // 只是展开态内部变高变矮（多了暂存行、歌词行）时内容不用重来。
        if self.shown != self.target {
            let same_page = std::mem::discriminant(&self.shown) == std::mem::discriminant(&self.target) && self.target.is_expanded();
            if same_page {
                self.shown = self.target;
            } else {
                self.content_alpha -= dt / 0.07;
                if self.content_alpha <= 0.0 {
                    self.content_alpha = 0.0;
                    self.shown = self.target;
                }
            }
        } else if self.content_alpha < 1.0 {
            self.content_alpha = (self.content_alpha + dt / 0.22).min(1.0);
        }

        if self.copied.is_some_and(|(_, t)| now - t > Duration::from_millis(1400)) {
            self.copied = None;
            self.dirty = true;
        }
        // 按钮高亮：指着的淡入，离开的淡出（按钮位置用上一帧画出来的）
        let (cx, cy) = self.cursor;
        self.hot = if self.expanded { self.buttons.iter().filter(|(_, r)| r.contains(cx, cy)).map(|b| b.0).collect() } else { Vec::new() };
        for &b in &self.hot {
            if !self.glow.iter().any(|g| g.0 == b) {
                self.glow.push((b, 0.0));
            }
        }
        for (b, v) in &mut self.glow {
            *v = if self.hot.contains(b) { (*v + dt / 0.08).min(1.0) } else { (*v - dt / 0.2).max(0.0) };
        }
        let hot = &self.hot;
        self.glow.retain(|(b, v)| *v > 0.0 || hot.contains(b));
        self.bar_thick.target = if self.scrub.is_some() || self.bar_hover { 8.0 } else { 5.0 };
        self.bubble_hot.target = if !self.expanded && self.on_bubble(cx, cy) { 1.1 } else { 1.0 };

        if self.shown == self.target && self.content_alpha >= 1.0 {
            self.slide = 0.0;
        }
        for s in [&mut self.w, &mut self.h, &mut self.r, &mut self.press, &mut self.top, &mut self.split, &mut self.squish.1, &mut self.bar_thick, &mut self.bubble_hot, &mut self.nudge] {
            s.step(dt);
        }
        if let Some(l) = &mut self.level {
            l.fill.step(dt);
        }
        self.anim_t += dt;
    }

    fn lyric_text(&self, idx: Option<usize>) -> &str {
        match (&self.info.lyrics, idx) {
            (Some(l), Some(i)) => l.lines.get(i).map(|l| l.text.as_str()).unwrap_or(""),
            _ => "",
        }
    }

    fn lyric_trans(&self, idx: Option<usize>) -> &str {
        match (&self.info.lyrics, idx) {
            (Some(l), Some(i)) => l.lines.get(i).and_then(|l| l.trans.as_deref()).unwrap_or(""),
            _ => "",
        }
    }

    fn update_lyric(&mut self, now: Instant) {
        let idx = self.info.lyrics.as_ref().and_then(|l| l.index_at(self.info.position_now()));
        if idx != self.lyric.idx || self.lyric.track != self.info.track_id {
            let same_track = self.lyric.track == self.info.track_id;
            let (prev, prev_trans) = if same_track {
                (self.lyric_text(self.lyric.idx).to_string(), self.lyric_trans(self.lyric.idx).to_string())
            } else {
                Default::default()
            };
            self.lyric.prev = prev;
            self.lyric.prev_trans = prev_trans;
            self.lyric.idx = idx;
            self.lyric.track = self.info.track_id;
            self.lyric.changed = now;
            self.dirty = true;
        }
    }

    fn animating(&self) -> bool {
        let level_moving = self.shown == Layout::Level && self.level.as_ref().is_some_and(|l| !l.fill.settled());
        let fx = [&self.squish.1, &self.bar_thick, &self.bubble_hot, &self.nudge];
        !(self.w.settled() && self.h.settled() && self.r.settled() && self.press.settled() && self.top.settled() && self.split.settled())
            || fx.iter().any(|s| !s.settled())
            || self.glow.iter().any(|(b, v)| !self.hot.contains(b) || *v < 1.0)
            || self.shown != self.target
            || self.content_alpha < 1.0
            || level_moving
            || self.lyric.changed.elapsed().as_secs_f32() < LYRIC_ANIM
    }

    /// 判断要不要现在出一帧，以及空闲时多久后再来。
    pub fn pace(&mut self, now: Instant) -> (bool, Pace) {
        if self.animating() {
            return (true, Pace::Animating);
        }
        let playing = self.info.playing;
        let timer = self.timer.is_some();
        let periodic = match self.shown {
            Layout::Media { .. } | Layout::TrackToast | Layout::ExpandedMedia { .. } if playing => Some(33),
            Layout::ExpandedNotes { .. } if self.copied.is_some() => Some(250),
            Layout::Media { .. } | Layout::Timer if timer => Some(250),
            Layout::ExpandedDash { .. } if timer => Some(250),
            Layout::ExpandedDash { .. } => Some(1000),
            _ => None,
        };
        let since = (now - self.last_render).as_millis() as u32;
        let due = self.dirty || periodic.is_some_and(|p| since >= p);
        let wait = periodic.map(|p| p.saturating_sub(if due { 0 } else { since })).unwrap_or(40).clamp(1, 40);
        (due, Pace::Wait(wait))
    }

    // ---------- 绘制 ----------

    pub fn render(&mut self, g: &Renderer) -> windows::core::Result<()> {
        self.last_render = Instant::now();
        self.dirty = false;

        // 封面换了就重建图像
        if let Some(px) = &self.info.art {
            if self.art.as_ref().map(|a| a.0) != Some(self.info.track_id) {
                self.art = g.image(px, ART).ok().map(|i| (self.info.track_id, i));
            }
        } else {
            self.art = None;
        }
        match self.info.other.as_ref().and_then(|o| o.art.as_ref().map(|px| (o.art_id, px))) {
            Some((id, px)) if self.other_art.as_ref().map(|a| a.0) != Some(id) => {
                self.other_art = g.image(px, ART).ok().map(|i| (id, i));
            }
            Some(_) => {}
            None => self.other_art = None,
        }
        // 暂存文件的图标：新的建、删掉的丢
        self.icons.retain(|id, _| self.shelf.items.iter().any(|i| i.id == *id));
        for item in &self.shelf.items {
            if let (false, Some(px)) = (self.icons.contains_key(&item.id), &item.icon)
                && let Ok(img) = g.image(px, ICON)
            {
                self.icons.insert(item.id, img);
            }
        }

        g.begin();
        let r = self.rect();
        let center = Vector2 { X: r.x + r.w / 2.0, Y: r.y + r.h / 2.0 };
        let s = self.press.value;
        let island = Matrix3x2::scale_around(s, s, center) * Matrix3x2::translation(self.nudge.value, 0.0);
        self.base = island;
        g.set_transform(island);

        let radius = self.r.value.min(r.h / 2.0).max(0.0);
        if let Some(b) = self.bubble_rect() {
            self.draw_bubble(g, b);
        }
        g.fill_rrect(r, radius, BLACK);
        g.stroke_rrect(r.inflate(-0.5), radius, 1.0, rgba(1.0, 1.0, 1.0, 0.07));

        // 尺寸还差得远时不画内容，避免内容溢出到形变中的岛外
        let (tw, th, _) = self.dims_of(self.shown);
        let gap = (r.w - tw).abs() + (r.h - th).abs();
        let a = self.content_alpha * (1.0 - gap / 90.0).clamp(0.0, 1.0);
        self.buttons.clear();
        self.tiles.clear();
        self.seek_bar = None;
        if a > 0.01 {
            // 形变途中内容不能溢出岛外；上下各让出圆角的一部分
            // 形变结束后不再让，按钮的悬停底色才能完整画出来
            let inset = (radius * 0.3).min(8.0) * (gap / 12.0).min(1.0);
            g.push_clip(Rect::new(r.x, r.y + inset, r.w, r.h - 2.0 * inset));
            // 翻页：旧页往反方向滑出，新页从翻页方向滑入
            if self.slide != 0.0 {
                let k = 1.0 - self.content_alpha;
                let dx = if self.shown != self.target { -self.slide } else { self.slide } * SLIDE * k * k;
                self.base = Matrix3x2::translation(dx, 0.0) * island;
                g.set_transform(self.base);
            }
            match self.shown {
                Layout::Idle { files, slim } => {
                    if files {
                        self.draw_idle_files(g, r, a);
                    }
                    if !slim {
                        self.draw_privacy_dots(g, r.x + 14.0, r.y + r.h / 2.0, a);
                    }
                }
                Layout::Media { lyric } => self.draw_compact(g, r, a, if lyric { Middle::Lyric } else { Middle::None }),
                Layout::TrackToast => self.draw_compact(g, r, a, Middle::Title),
                Layout::Charging => self.draw_charging(g, r, a),
                Layout::Level => self.draw_level(g, r, a),
                Layout::Timer => self.draw_timer_compact(g, r, a),
                Layout::TimerDone => self.draw_timer_done(g, r, a),
                Layout::LowBattery => self.draw_low_battery(g, r, a),
                Layout::Device => self.draw_device(g, r, a),
                Layout::Privacy => self.draw_privacy_toast(g, r, a),
                Layout::Drop => self.draw_drop(g, r, a),
                Layout::ExpandedMedia { lyric, .. } => self.draw_expanded_media(g, r, a, lyric),
                Layout::ExpandedDash { .. } => self.draw_dash(g, r, a),
                Layout::ExpandedNotes { .. } => self.draw_notes(g, r, a),
            }
            // 页码和暂存行不跟着翻页滑
            self.base = island;
            g.set_transform(island);
            if self.shown.is_expanded() {
                self.draw_page_dots(g, r, a);
            }
            if self.shown.has_shelf() {
                let base = self.shown.base_height();
                self.draw_shelf_row(g, Rect::new(r.x, r.y + base - 6.0, r.w, r.h - base + 6.0), a);
            }
            g.pop_clip();
        }
        g.set_transform(Matrix3x2::identity());
        g.end()
    }

    /// 简笔文件图标（折角纸）。
    fn draw_doc_glyph(&self, g: &Renderer, x: f32, y: f32, k: f32, c: Color) {
        let (w, h, fold) = (10.0 * k, 13.0 * k, 3.5 * k);
        g.fill_poly(&[(x, y), (x + w - fold, y), (x + w, y + fold), (x + w, y + h), (x, y + h)], c);
        g.fill_poly(&[(x + w - fold, y), (x + w - fold, y + fold), (x + w, y + fold)], with_alpha(c, 0.5));
    }

    fn draw_idle_files(&self, g: &Renderer, r: Rect, a: f32) {
        let cy = r.y + r.h / 2.0;
        let x = r.x + r.w - 40.0;
        self.draw_doc_glyph(g, x, cy - 6.5, 1.0, with_alpha(BLUE, a));
        g.text(&self.shelf.items.len().to_string(), &g.fonts.small, Rect::new(x + 14.0, r.y, 20.0, r.h), with_alpha(WHITE, a));
    }

    fn draw_drop(&self, g: &Renderer, r: Rect, a: f32) {
        let zone = Rect::new(r.x + 14.0, r.y + 14.0, r.w - 28.0, r.h - 28.0);
        g.fill_rrect(zone, 26.0, with_alpha(BLUE, 0.12 * a));
        g.stroke_rrect(zone, 26.0, 1.5, with_alpha(BLUE, 0.7 * a));
        // 向下的箭头 + 托盘
        let (cx, cy) = (zone.x + zone.w / 2.0, zone.y + 30.0);
        let c = with_alpha(BLUE, a);
        g.fill_rrect(Rect::new(cx - 1.5, cy - 12.0, 3.0, 14.0), 1.5, c);
        g.fill_poly(&[(cx - 7.0, cy), (cx + 7.0, cy), (cx, cy + 7.0)], c);
        g.fill_rrect(Rect::new(cx - 12.0, cy + 10.0, 24.0, 3.0), 1.5, c);
        g.text("松手放进暂存区", &g.fonts.title_center, Rect::new(zone.x, cy + 18.0, zone.w, 22.0), with_alpha(WHITE, a));
    }

    fn draw_shelf_row(&mut self, g: &Renderer, row: Rect, a: f32) {
        let pad = 20.0;
        g.fill_rrect(Rect::new(row.x + pad, row.y, row.w - 2.0 * pad, 1.0), 0.5, with_alpha(WHITE, 0.1 * a));
        let mut x = row.x + pad - 2.0;
        let y = row.y + 8.0;
        let ids: Vec<u64> = self.shelf.items.iter().map(|i| i.id).collect();
        for id in ids {
            let tile = Rect::new(x, y, TILE_W, 74.0);
            let hovered = self.hovered_tile.is_some_and(|t| t.0 == id);
            let held = matches!(self.press_on, Some((PressOn::Tile(t), ..)) if t == id);
            if hovered || held {
                g.fill_rrect(tile, 12.0, with_alpha(WHITE, if held { 0.15 } else { 0.09 } * a));
            }
            // 按住时图标缩一点，像被按下去
            let k = if held { 40.0 } else { 44.0 };
            let icon = Rect::new(x + (TILE_W - k) / 2.0, y + 6.0 + (44.0 - k) / 2.0, k, k);
            match self.icons.get(&id) {
                Some(img) => g.draw_image(img, icon, 6.0, a),
                None => self.draw_doc_glyph(g, icon.x + (k - 20.0) / 2.0, icon.y + (k - 26.0) / 2.0, k / 22.0, with_alpha(WHITE, 0.6 * a)),
            }
            let name = self.shelf.get(id).map(|i| i.name.as_str()).unwrap_or("");
            g.text(name, &g.fonts.tiny_center, Rect::new(x + 2.0, y + 54.0, TILE_W - 4.0, 16.0), with_alpha(WHITE, 0.8 * a));

            let close = Rect::new(x + TILE_W - 20.0, y + 1.0, 18.0, 18.0);
            if hovered {
                let (cx, cy) = (close.x + 9.0, close.y + 9.0);
                let on = self.hovered_tile == Some((id, true));
                let held = matches!(self.press_on, Some((PressOn::Close(t), ..)) if t == id);
                let d = if held { 15.0 } else { 18.0 };
                g.fill_rrect(Rect::new(cx - d / 2.0, cy - d / 2.0, d, d), d / 2.0, with_alpha(if on { RED } else { rgba(0.35, 0.35, 0.38, 1.0) }, a));
                let c = with_alpha(WHITE, a);
                g.line((cx - 3.5, cy - 3.5), (cx + 3.5, cy + 3.5), 1.6, c);
                g.line((cx - 3.5, cy + 3.5), (cx + 3.5, cy - 3.5), 1.6, c);
            }
            self.tiles.push(Tile { id, rect: tile, close });
            x += TILE_W + TILE_GAP;
        }
    }

    /// 按钮的悬停高亮程度（0..1）。
    fn glow(&self, b: Btn) -> f32 {
        self.glow.iter().find(|g| g.0 == b).map_or(0.0, |g| g.1)
    }

    /// 按钮被按下的程度（0..1）。
    fn pressing(&self, b: Btn) -> f32 {
        if self.squish.0 == Some(b) { self.squish.1.value.clamp(0.0, 1.0) } else { 0.0 }
    }

    /// 以 (cx, cy) 为中心，按这个按钮的按压程度缩小（弹回时略大于 1）。画完调 unsquish。
    fn squish_at(&self, g: &Renderer, b: Btn, cx: f32, cy: f32) {
        if self.squish.0 == Some(b) {
            // 宽的按钮（整行歌词）缩同样比例会显得太猛，少缩一点
            let depth = if b == Btn::Lyric { 0.035 } else { 0.12 };
            let k = 1.0 - depth * self.squish.1.value;
            g.set_transform(Matrix3x2::scale_around(k, k, Vector2 { X: cx, Y: cy }) * self.base);
        }
    }

    fn unsquish(&self, g: &Renderer) {
        g.set_transform(self.base);
    }

    fn draw_page_dots(&mut self, g: &Renderer, r: Rect, a: f32) {
        let cx = r.x + r.w / 2.0;
        let y = r.y + 9.0;
        let (n, cur) = (self.pages().len(), self.page_index());
        let x0 = cx - (n as f32 * 10.0 - 5.0) / 2.0;
        let glow = self.glow(Btn::Page);
        if glow > 0.0 {
            g.fill_rrect(Rect::new(x0 - 11.0, y - 5.5, n as f32 * 10.0 + 17.0, 16.0), 8.0, with_alpha(WHITE, 0.16 * glow * a));
        }
        for i in 0..n {
            let x = x0 + i as f32 * 10.0;
            g.fill_rrect(Rect::new(x, y, 5.0, 5.0), 2.5, with_alpha(WHITE, if i == cur { 0.85 } else { 0.25 } * a));
        }
        self.buttons.push((Btn::Page, Rect::new(cx - 24.0, r.y, 48.0, 20.0)));
    }

    fn draw_art(&self, g: &Renderer, r: Rect, radius: f32, a: f32) {
        match &self.art {
            Some((_, img)) => g.draw_image(img, r, radius, a),
            None => {
                g.fill_rrect(r, radius, with_alpha(rgba(0.22, 0.22, 0.25, 1.0), a));
                let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
                let k = r.w / 64.0;
                // 音符
                g.fill_rrect(Rect::new(cx + 4.0 * k, cy - 14.0 * k, 3.0 * k, 22.0 * k), 1.0, with_alpha(WHITE, 0.6 * a));
                g.fill_rrect(Rect::new(cx - 6.0 * k, cy + 2.0 * k, 13.0 * k, 10.0 * k), 5.0 * k, with_alpha(WHITE, 0.6 * a));
            }
        }
    }

    fn accent(&self) -> Color {
        let [r, g, b] = self.info.accent;
        rgba(r, g, b, 1.0)
    }

    fn draw_bars(&self, g: &Renderer, right: f32, cy: f32, max_h: f32, a: f32) {
        let c = with_alpha(self.accent(), a);
        let t = self.anim_t;
        let (bw, gap) = (3.0, 2.5);
        for i in 0..4 {
            let fi = i as f32;
            let h = if self.info.playing {
                let v = ((t * (5.1 + fi * 1.7) + fi * 1.3).sin() * 0.5 + 0.5) * 0.6
                    + ((t * (8.3 - fi * 0.9) + fi * 2.1).sin() * 0.5 + 0.5) * 0.4;
                3.0 + v * (max_h - 3.0)
            } else {
                3.0
            };
            let x = right - (4.0 - fi) * (bw + gap) + gap;
            g.fill_rrect(Rect::new(x, cy - h / 2.0, bw, h), bw / 2.0, c);
        }
    }

    /// 当前歌词，换行时旧行上滑淡出、新行从下方滑入。trans：在下面再画一行翻译。
    fn draw_lyric(&self, g: &Renderer, rect: Rect, a: f32, c: Color, trans: bool) {
        let p = (self.lyric.changed.elapsed().as_secs_f32() / LYRIC_ANIM).clamp(0.0, 1.0);
        let e = 1.0 - (1.0 - p) * (1.0 - p);
        let shift = 9.0;
        let cur = self.lyric_text(self.lyric.idx);
        let cur = if cur.is_empty() { "♪" } else { cur };
        let cur_trans = self.lyric_trans(self.lyric.idx);
        let trans_rect = |r: Rect| Rect::new(r.x, r.y + 20.0, r.w, 18.0);
        let tc = with_alpha(c, 0.6);
        if p < 1.0 && !self.lyric.prev.is_empty() {
            let old = Rect::new(rect.x, rect.y - e * shift, rect.w, rect.h);
            g.text(&self.lyric.prev, &g.fonts.lyric_center, old, with_alpha(c, a * (1.0 - e)));
            if trans && !self.lyric.prev_trans.is_empty() {
                g.text(&self.lyric.prev_trans, &g.fonts.small_center, trans_rect(old), with_alpha(tc, a * (1.0 - e)));
            }
        }
        let new = Rect::new(rect.x, rect.y + (1.0 - e) * shift, rect.w, rect.h);
        g.text(cur, &g.fonts.lyric_center, new, with_alpha(c, a * e));
        if trans && !cur_trans.is_empty() {
            g.text(cur_trans, &g.fonts.small_center, trans_rect(new), with_alpha(tc, a * e));
        }
    }

    /// 隐私指示点：绿色 = 摄像头，橙色 = 麦克风（和 macOS 一致）。返回占用的宽度。
    fn draw_privacy_dots(&self, g: &Renderer, x: f32, cy: f32, a: f32) -> f32 {
        let mut dx = 0.0;
        for (on, c) in [(!self.privacy.cam.is_empty(), GREEN), (!self.privacy.mic.is_empty(), ORANGE)] {
            if on {
                g.fill_rrect(Rect::new(x + dx, cy - 3.0, 6.0, 6.0), 3.0, with_alpha(c, a));
                dx += 10.0;
            }
        }
        dx
    }

    fn timer_text(&self) -> String {
        let s = self.timer.as_ref().map(|t| t.remaining(Instant::now())).unwrap_or(0.0);
        system::fmt_secs(s.ceil() as f64)
    }

    fn timer_frac(&self) -> f32 {
        self.timer.as_ref().map(|t| t.remaining(Instant::now()) / t.total.max(1.0)).unwrap_or(0.0)
    }
}

/// 紧凑态中间放什么。
#[derive(Clone, Copy, PartialEq)]
enum Middle {
    None,
    Title,
    Lyric,
}

/// 进度条的可点击区域：上下放宽，方便点中。
fn seek_zone(bar: Rect) -> Rect {
    Rect::new(bar.x - 6.0, bar.y - 9.0, bar.w + 12.0, bar.h + 18.0)
}

impl Island {
    fn draw_compact(&self, g: &Renderer, r: Rect, a: f32, middle: Middle) {
        let art = Rect::new(r.x + 6.0, r.y + (r.h - 22.0) / 2.0, 22.0, 22.0);
        self.draw_art(g, art, 6.0, a);
        if self.timer.is_some() {
            // 有计时器时，右边显示倒计时而不是频谱
            g.text(&self.timer_text(), &g.fonts.small_right, Rect::new(r.x + r.w - 58.0, r.y, 46.0, r.h), with_alpha(ORANGE, a));
        } else {
            self.draw_bars(g, r.x + r.w - 14.0, r.y + r.h / 2.0, 14.0, a);
        }
        let dots = self.draw_privacy_dots(g, art.x + art.w + 7.0, r.y + r.h / 2.0, a);
        let mid = Rect::new(r.x + 38.0 + dots, r.y, r.w - 38.0 - dots - 62.0, r.h);
        match middle {
            Middle::None => {}
            Middle::Title => g.text(&self.info.title, &g.fonts.small, mid, with_alpha(WHITE, a)),
            Middle::Lyric => self.draw_lyric(g, Rect::new(mid.x + 4.0, r.y, mid.w - 8.0, r.h), a, WHITE, false),
        }
    }

    fn draw_charging(&self, g: &Renderer, r: Rect, a: f32) {
        let cy = r.y + r.h / 2.0;
        let x = r.x + 16.0;
        // 闪电
        g.fill_poly(&[(x + 6.0, cy - 9.0), (x, cy + 1.5), (x + 4.5, cy + 1.5), (x + 3.0, cy + 9.0), (x + 9.5, cy - 2.0), (x + 5.0, cy - 2.0), (x + 7.5, cy - 9.0)], with_alpha(GREEN, a));
        g.text("正在充电", &g.fonts.small, Rect::new(x + 16.0, r.y, 120.0, r.h), with_alpha(WHITE, a));
        if let Some(b) = self.battery {
            let bx = r.x + r.w - 16.0 - 26.0;
            self.draw_battery(g, bx, cy, b, a);
            g.text(&format!("{}%", b.percent), &g.fonts.small_right, Rect::new(bx - 66.0, r.y, 60.0, r.h), with_alpha(GREEN, a));
        }
    }

    fn draw_battery(&self, g: &Renderer, x: f32, cy: f32, b: Battery, a: f32) {
        let body = Rect::new(x, cy - 6.0, 23.0, 12.0);
        g.stroke_rrect(body, 3.5, 1.2, with_alpha(WHITE, 0.45 * a));
        g.fill_rrect(Rect::new(x + 24.0, cy - 2.5, 2.0, 5.0), 1.0, with_alpha(WHITE, 0.45 * a));
        let fill_c = if b.charging { GREEN } else if b.percent <= 20 { rgba(1.0, 0.27, 0.23, 1.0) } else { WHITE };
        let w = (19.0 * b.percent as f32 / 100.0).max(1.5);
        g.fill_rrect(Rect::new(x + 2.0, cy - 4.0, w, 8.0), 2.0, with_alpha(fill_c, a));
    }

    fn draw_level(&self, g: &Renderer, r: Rect, a: f32) {
        let Some(l) = &self.level else { return };
        let cy = r.y + r.h / 2.0;
        let x = r.x + 18.0;
        let c = with_alpha(WHITE, a);
        match l.kind {
            LevelKind::Volume => {
                // 喇叭
                g.fill_rrect(Rect::new(x, cy - 3.5, 4.0, 7.0), 1.0, c);
                g.fill_poly(&[(x + 3.0, cy - 3.5), (x + 9.0, cy - 8.0), (x + 9.0, cy + 8.0), (x + 3.0, cy + 3.5)], c);
                if l.muted || l.fill.target <= 0.001 {
                    g.line((x + 12.0, cy - 3.5), (x + 19.0, cy + 3.5), 1.6, c);
                    g.line((x + 12.0, cy + 3.5), (x + 19.0, cy - 3.5), 1.6, c);
                } else {
                    g.line((x + 12.5, cy - 3.0), (x + 13.5, cy), 1.6, c);
                    g.line((x + 13.5, cy), (x + 12.5, cy + 3.0), 1.6, c);
                    if l.fill.target > 0.5 {
                        g.line((x + 15.5, cy - 6.0), (x + 17.5, cy), 1.6, c);
                        g.line((x + 17.5, cy), (x + 15.5, cy + 6.0), 1.6, c);
                    }
                }
            }
            LevelKind::Brightness => {
                // 太阳
                let (sx, sy) = (x + 8.0, cy);
                g.fill_rrect(Rect::new(sx - 3.5, sy - 3.5, 7.0, 7.0), 3.5, c);
                for i in 0..8 {
                    let ang = i as f32 * std::f32::consts::FRAC_PI_4;
                    let (s, co) = ang.sin_cos();
                    g.line((sx + co * 6.0, sy + s * 6.0), (sx + co * 8.5, sy + s * 8.5), 1.5, c);
                }
            }
        }
        let bar = Rect::new(x + 32.0, cy - 3.0, r.w - 32.0 - 18.0 - 32.0 - 44.0, 6.0);
        g.fill_rrect(bar, 3.0, with_alpha(WHITE, 0.18 * a));
        let v = l.fill.value.clamp(0.0, 1.0);
        let fc = if l.muted { with_alpha(WHITE, 0.35 * a) } else { c };
        if v > 0.0 {
            g.fill_rrect(Rect::new(bar.x, bar.y, (bar.w * v).max(6.0), bar.h), 3.0, fc);
        }
        let pct = if l.muted { "静音".to_string() } else { format!("{}", (l.fill.target * 100.0).round() as i32) };
        g.text(&pct, &g.fonts.small_right, Rect::new(r.x + r.w - 18.0 - 44.0, r.y, 44.0, r.h), c);
    }

    fn draw_timer_compact(&self, g: &Renderer, r: Rect, a: f32) {
        let cy = r.y + r.h / 2.0;
        let cx = r.x + 20.0;
        g.arc(cx, cy, 8.0, 1.0, 2.5, with_alpha(ORANGE, 0.25 * a));
        g.arc(cx, cy, 8.0, self.timer_frac(), 2.5, with_alpha(ORANGE, a));
        self.draw_privacy_dots(g, cx + 16.0, cy, a);
        g.text(&self.timer_text(), &g.fonts.title, Rect::new(r.x + r.w - 16.0 - 80.0, r.y, 80.0, r.h), with_alpha(ORANGE, a));
    }

    fn draw_low_battery(&self, g: &Renderer, r: Rect, a: f32) {
        let cy = r.y + r.h / 2.0;
        g.text("电量不足", &g.fonts.small, Rect::new(r.x + 18.0, r.y, 120.0, r.h), with_alpha(WHITE, a));
        if let Some(b) = self.battery {
            let bx = r.x + r.w - 16.0 - 26.0;
            self.draw_battery(g, bx, cy, b, a);
            g.text(&format!("{}%", b.percent), &g.fonts.small_right, Rect::new(bx - 66.0, r.y, 60.0, r.h), with_alpha(RED, a));
        }
    }

    /// 耳机图标 + 设备名 + 音量 / 电量。
    fn draw_device(&self, g: &Renderer, r: Rect, a: f32) {
        let cy = r.y + r.h / 2.0;
        let (hx, c) = (r.x + 24.0, with_alpha(WHITE, a));
        // 头梁：只画圆的上半部分
        g.push_clip(Rect::new(hx - 9.0, cy - 9.0, 18.0, 9.5));
        g.arc(hx, cy + 0.5, 7.0, 1.0, 1.8, c);
        g.pop_clip();
        g.fill_rrect(Rect::new(hx - 8.5, cy, 4.0, 7.0), 1.5, c);
        g.fill_rrect(Rect::new(hx + 4.5, cy, 4.0, 7.0), 1.5, c);
        let (name, note, battery) = &self.device;
        g.text(name, &g.fonts.small, Rect::new(hx + 16.0, r.y, r.w - 24.0 - 16.0 - 100.0, r.h), c);
        match battery {
            Some(p) => {
                let bx = r.x + r.w - 16.0 - 26.0;
                let b = Battery { percent: *p, charging: false };
                self.draw_battery(g, bx, cy, b, a);
                let pc = if *p <= 20 { RED } else { GREEN };
                g.text(&format!("{p}%"), &g.fonts.small_right, Rect::new(bx - 56.0, r.y, 50.0, r.h), with_alpha(pc, a));
            }
            None => g.text(note, &g.fonts.small_right, Rect::new(r.x + r.w - 18.0 - 60.0, r.y, 60.0, r.h), with_alpha(WHITE, 0.6 * a)),
        }
    }

    fn draw_privacy_toast(&self, g: &Renderer, r: Rect, a: f32) {
        let cy = r.y + r.h / 2.0;
        let (msg, cam) = &self.privacy_msg;
        let c = if *cam { GREEN } else { ORANGE };
        g.fill_rrect(Rect::new(r.x + 16.0, cy - 4.5, 9.0, 9.0), 4.5, with_alpha(c, a));
        g.text(msg, &g.fonts.small, Rect::new(r.x + 36.0, r.y, r.w - 36.0 - 16.0, r.h), with_alpha(WHITE, a));
    }

    fn draw_timer_done(&self, g: &Renderer, r: Rect, a: f32) {
        let cy = r.y + r.h / 2.0;
        let cx = r.x + 20.0;
        // 铃铛一样跳动的圆环
        let pulse = 1.0 + 0.12 * (self.anim_t * 9.0).sin().abs();
        g.arc(cx, cy, 8.0 * pulse, 1.0, 2.5, with_alpha(ORANGE, a));
        g.text("计时结束", &g.fonts.small, Rect::new(cx + 18.0, r.y, 140.0, r.h), with_alpha(WHITE, a));
    }

    fn draw_expanded_media(&mut self, g: &Renderer, r: Rect, a: f32, lyric: u8) {
        let pad = 20.0;
        let art = Rect::new(r.x + pad, r.y + pad, 62.0, 62.0);
        let tx = art.x + art.w + 14.0;
        let tw = r.x + r.w - pad - 34.0 - tx;
        // 封面和歌名一起是个按钮：悬停时封面浮起一点、歌手名变亮，按下缩一下
        let can_open = !self.info.app_id.is_empty();
        let glow = self.glow(Btn::OpenPlayer);
        self.squish_at(g, Btn::OpenPlayer, art.x + art.w / 2.0, art.y + art.h / 2.0);
        self.draw_art(g, art.inflate(2.0 * glow), 14.0 + glow, a);
        self.unsquish(g);
        g.text(&self.info.title, &g.fonts.title, Rect::new(tx, art.y + 8.0, tw, 22.0), with_alpha(WHITE, a));
        g.text(&self.info.artist, &g.fonts.body, Rect::new(tx, art.y + 32.0, tw, 20.0), with_alpha(WHITE, (0.55 + 0.3 * glow) * a));
        self.draw_bars(g, r.x + r.w - pad, art.y + 19.0, 16.0, a);
        if let Some(o) = &self.info.other {
            // 右上角：另一个播放器，点一下互换
            let chip = Rect::new(r.x + r.w - 34.0 - 104.0, r.y + 6.0, 104.0, 18.0);
            self.squish_at(g, Btn::Swap, chip.x + chip.w / 2.0, chip.y + chip.h / 2.0);
            g.fill_rrect(chip, 9.0, with_alpha(WHITE, (0.12 + 0.16 * self.glow(Btn::Swap)) * a));
            let dot = Rect::new(chip.x + 4.0, chip.y + 3.0, 12.0, 12.0);
            match &self.other_art {
                Some((_, img)) => g.draw_image(img, dot, 6.0, a),
                None => g.fill_rrect(dot, 6.0, with_alpha(self.accent(), a)),
            }
            let label = format!("切到 {}", o.app);
            g.text(&label, &g.fonts.small, Rect::new(chip.x + 21.0, chip.y, chip.w - 27.0, chip.h), with_alpha(WHITE, (0.8 + 0.2 * self.glow(Btn::Swap)) * a));
            self.unsquish(g);
            self.buttons.push((Btn::Swap, chip.inflate(3.0)));
        }
        // 放在互换按钮后面，重叠处优先互换
        if can_open {
            self.buttons.push((Btn::OpenPlayer, Rect::new(art.x, art.y, tx + tw - art.x, art.h)));
        }

        let mut row_y = art.y + art.h + 14.0;
        if lyric > 0 {
            let two = lyric >= 2;
            // 能跳进度、而且正唱到某一句时，歌词可以点：悬停垫底色、右边出个「重听」小图标，按下时整行轻轻缩一下
            let row = Rect::new(r.x + pad - 6.0, row_y - 9.0, r.w - 2.0 * pad + 12.0, if two { 46.0 } else { 28.0 });
            if self.info.can_seek && self.lyric.idx.is_some() {
                let glow = self.glow(Btn::Lyric);
                self.squish_at(g, Btn::Lyric, row.x + row.w / 2.0, row.y + row.h / 2.0);
                if glow + self.pressing(Btn::Lyric) > 0.0 {
                    g.fill_rrect(row, 12.0, with_alpha(WHITE, (0.1 * glow + 0.08 * self.pressing(Btn::Lyric)) * a));
                    let (ix, iy) = (row.x + row.w - 16.0, row.y + 14.0);
                    let ic = with_alpha(WHITE, 0.6 * glow * a);
                    g.arc(ix, iy, 5.5, 0.78, 1.6, ic);
                    // 圆弧从正上方顺时针画，箭头在起点往左指，像「倒回去」
                    g.fill_poly(&[(ix + 0.5, iy - 8.5), (ix - 3.5, iy - 5.5), (ix + 0.5, iy - 2.5)], ic);
                }
                self.unsquish(g);
                self.buttons.push((Btn::Lyric, row));
            }
            self.draw_lyric(g, Rect::new(r.x + pad, row_y - 6.0, r.w - 2.0 * pad, 24.0), a, self.accent(), two);
            row_y += if two { LYRIC_ROW2 } else { LYRIC_ROW };
        }
        if self.info.duration > 1.0 {
            let pos = match self.scrub {
                Some(f) => f as f64 * self.info.duration,
                None => self.info.position_now(),
            };
            let bh = self.bar_thick.value.max(3.0);
            // 0..1：变粗的程度，悬停/拖动时进度条和时间都更亮
            let hot = ((bh - 5.0) / 3.0).clamp(0.0, 1.0);
            let bar = Rect::new(r.x + pad + 40.0, row_y + 8.5 - bh / 2.0, r.w - 2.0 * pad - 80.0, bh);
            g.fill_rrect(bar, bh / 2.0, with_alpha(WHITE, (0.18 + 0.06 * hot) * a));
            let p = (pos / self.info.duration).clamp(0.0, 1.0) as f32;
            g.fill_rrect(Rect::new(bar.x, bar.y, (bar.w * p).max(bh), bar.h), bh / 2.0, with_alpha(WHITE, (0.85 + 0.15 * hot) * a));
            if hot > 0.0 {
                // 拖柄：随进度条变粗一起长出来，拖动时再大一点
                let d = bh + 5.0 * hot + if self.scrub.is_some() { 2.0 } else { 0.0 };
                let kx = bar.x + bar.w * p;
                g.fill_rrect(Rect::new(kx - d / 2.0, bar.y + bh / 2.0 - d / 2.0, d, d), d / 2.0, with_alpha(WHITE, hot * a));
                // 指着或拖动时，上方冒个小气泡显示那一点的时间
                let f = self.scrub.unwrap_or(((self.cursor.0 - bar.x) / bar.w).clamp(0.0, 1.0));
                let (tw, th) = (48.0, 20.0);
                let tx = (bar.x + bar.w * f - tw / 2.0).clamp(r.x + pad, r.x + r.w - pad - tw);
                let tip = Rect::new(tx, bar.y + bh / 2.0 - 34.0 + 4.0 * (1.0 - hot), tw, th);
                g.fill_rrect(tip, th / 2.0, with_alpha(rgba(0.2, 0.2, 0.22, 1.0), hot * a));
                g.text(&system::fmt_secs(f as f64 * self.info.duration), &g.fonts.small_center, tip, with_alpha(WHITE, hot * a));
            }
            let lc = with_alpha(WHITE, (0.5 + 0.3 * hot) * a);
            g.text(&system::fmt_secs(pos), &g.fonts.small, Rect::new(r.x + pad, row_y, 36.0, 17.0), lc);
            g.text(&format!("-{}", system::fmt_secs(self.info.duration - pos)), &g.fonts.small_right, Rect::new(r.x + r.w - pad - 40.0, row_y, 40.0, 17.0), lc);
            self.seek_bar = Some(Rect::new(bar.x, row_y + 6.0, bar.w, 5.0));
            row_y += 20.0;
        }

        let cy = row_y + 13.0;
        let cx = r.x + r.w / 2.0;
        let c = with_alpha(WHITE, a);
        for (btn, bx) in [(Btn::Prev, cx - 64.0), (Btn::Toggle, cx), (Btn::Next, cx + 64.0)] {
            // 悬停时背后浮出一个胶囊（离岛底边近，放不下圆），按下时整个按钮缩一下、底色更亮
            let (glow, held) = (self.glow(btn), self.pressing(btn));
            self.squish_at(g, btn, bx, cy);
            if glow + held > 0.0 {
                let w = if btn == Btn::Toggle { 58.0 } else { 54.0 };
                g.fill_rrect(Rect::new(bx - w / 2.0, cy - 15.0, w, 30.0), 15.0, with_alpha(WHITE, (0.16 * glow + 0.1 * held) * a));
            }
            match btn {
                Btn::Toggle if self.info.playing => {
                    g.fill_rrect(Rect::new(bx - 7.5, cy - 9.0, 5.0, 18.0), 1.5, c);
                    g.fill_rrect(Rect::new(bx + 2.5, cy - 9.0, 5.0, 18.0), 1.5, c);
                }
                Btn::Toggle => g.fill_poly(&[(bx - 6.0, cy - 10.0), (bx + 10.0, cy), (bx - 6.0, cy + 10.0)], c),
                Btn::Next => {
                    g.fill_poly(&[(bx - 11.0, cy - 7.0), (bx, cy), (bx - 11.0, cy + 7.0)], c);
                    g.fill_poly(&[(bx, cy - 7.0), (bx + 11.0, cy), (bx, cy + 7.0)], c);
                }
                _ => {
                    g.fill_poly(&[(bx + 11.0, cy - 7.0), (bx, cy), (bx + 11.0, cy + 7.0)], c);
                    g.fill_poly(&[(bx, cy - 7.0), (bx - 11.0, cy), (bx, cy + 7.0)], c);
                }
            }
            self.unsquish(g);
            self.buttons.push((btn, Rect::new(bx - 24.0, cy - 18.0, 48.0, 36.0)));
        }
    }

    /// 胶囊按钮。
    fn pill(&mut self, g: &Renderer, r: Rect, label: &str, c: Color, btn: Btn, a: f32) {
        self.squish_at(g, btn, r.x + r.w / 2.0, r.y + r.h / 2.0);
        let bg = 0.18 + 0.2 * self.glow(btn) + 0.1 * self.pressing(btn);
        g.fill_rrect(r, r.h / 2.0, with_alpha(c, bg * a));
        g.text(label, &g.fonts.small_center, r, with_alpha(c, a));
        self.unsquish(g);
        self.buttons.push((btn, r));
    }

    fn draw_dash(&mut self, g: &Renderer, r: Rect, a: f32) {
        let clock = system::clock();
        let pad = 24.0;
        // 悬停底色贴着时间和日期的实际宽度，四周留 12 的边
        let cw = g.text_width(&clock.time, &g.fonts.clock).max(g.text_width(&clock.date, &g.fonts.body) + 2.0).min(200.0);
        let clock_area = Rect::new(r.x + pad - 12.0, r.y + 10.0, cw + 24.0, 78.0);
        let glow = self.glow(Btn::OpenClock);
        if glow > 0.0 {
            g.fill_rrect(clock_area, 12.0, with_alpha(WHITE, 0.1 * glow * a));
        }
        self.buttons.push((Btn::OpenClock, clock_area));
        g.text(&clock.time, &g.fonts.clock, Rect::new(r.x + pad, r.y + 14.0, 160.0, 46.0), with_alpha(WHITE, a));
        g.text(&clock.date, &g.fonts.body, Rect::new(r.x + pad + 2.0, r.y + 58.0, 200.0, 22.0), with_alpha(WHITE, 0.55 * a));
        let right = r.x + r.w - pad;
        let field = city_field(r);
        if self.editing == Some(Input::City) {
            // 输入框（独立小窗口）盖在这里，画个底
            g.fill_rrect(field, 8.0, with_alpha(rgba(0.17, 0.17, 0.18, 1.0), a));
        } else {
            match &self.weather {
                weather::State::Ready(n) => {
                    let (desc, icon) = (n.desc, n.icon);
                    let line = match &n.city {
                        Some(c) => format!("{c} · {desc}  {}° / {}°", n.lo.round() as i32, n.hi.round() as i32),
                        None => format!("{desc}  {}° / {}°", n.lo.round() as i32, n.hi.round() as i32),
                    };
                    // 点天气可以设置城市（自动定位的也能改成手动）；悬停底色贴着文字宽度。
                    // 上边留给页码圆点（它的点击区到 r.y + 20），下边别碰到电量
                    let tw = g.text_width(&line, &g.fonts.small_right).clamp(70.0, 200.0);
                    let area = Rect::new(right - tw, r.y + 22.0, tw, 38.0);
                    let glow = self.glow(Btn::SetCity);
                    if glow > 0.0 {
                        g.fill_rrect(Rect::new(area.x - 12.0, r.y + 14.0, tw + 24.0, 51.0), 12.0, with_alpha(WHITE, 0.1 * glow * a));
                    }
                    let temp = format!("{}°", n.temp.round() as i32);
                    g.text(&temp, &g.fonts.title_right, Rect::new(right - 60.0, r.y + 18.0, 60.0, 24.0), with_alpha(WHITE, a));
                    draw_weather_icon(g, right - 52.0, r.y + 30.0, icon, n.is_day, a);
                    g.text(&line, &g.fonts.small_right, Rect::new(right - 200.0, r.y + 42.0, 200.0, 18.0), with_alpha(WHITE, 0.55 * a));
                    self.buttons.push((Btn::SetCity, area));
                }
                weather::State::NoLocation => {
                    let hint = Rect::new(right - 190.0, r.y + 22.0, 190.0, 18.0);
                    let glow = self.glow(Btn::SetCity);
                    g.text("定位不可用，点这里设置城市 ›", &g.fonts.small_right, hint, with_alpha(WHITE, (0.4 + 0.45 * glow) * a));
                    self.buttons.push((Btn::SetCity, hint.inflate(4.0)));
                }
                weather::State::Unknown => {}
            }
        }
        if let Some(b) = self.battery {
            let bx = right - 26.0;
            // 和上面天气的悬停底色错开一点
            let cy = r.y + 74.0;
            let area = Rect::new(bx - 62.0, cy - 13.0, 102.0, 26.0);
            let glow = self.glow(Btn::OpenBattery);
            if glow > 0.0 {
                g.fill_rrect(area, 13.0, with_alpha(WHITE, 0.1 * glow * a));
            }
            self.draw_battery(g, bx, cy, b, a);
            let c = if b.charging { GREEN } else { WHITE };
            g.text(&format!("{}%", b.percent), &g.fonts.small_right, Rect::new(bx - 52.0, cy - 9.0, 46.0, 18.0), with_alpha(c, a));
            if let Some(left) = system::battery_time() {
                g.text(&system::fmt_left(left), &g.fonts.small_right, Rect::new(bx - 52.0 - 130.0, cy - 9.0, 126.0, 18.0), with_alpha(WHITE, 0.45 * a));
            }
            self.buttons.push((Btn::OpenBattery, area));
        }

        // 计时器一行
        let row = Rect::new(r.x + pad, r.y + 100.0, r.w - 2.0 * pad, 28.0);
        if self.timer.is_some() {
            let cy = row.y + row.h / 2.0;
            g.arc(row.x + 11.0, cy, 9.0, 1.0, 2.5, with_alpha(ORANGE, 0.25 * a));
            g.arc(row.x + 11.0, cy, 9.0, self.timer_frac(), 2.5, with_alpha(ORANGE, a));
            g.text(&self.timer_text(), &g.fonts.title, Rect::new(row.x + 30.0, row.y, 90.0, row.h), with_alpha(ORANGE, a));
            let right = row.x + row.w;
            self.pill(g, Rect::new(right - 160.0, row.y, 78.0, row.h), "+1 分钟", ORANGE, Btn::AddMinute, a);
            self.pill(g, Rect::new(right - 74.0, row.y, 74.0, row.h), "取消", WHITE, Btn::CancelTimer, a);
        } else {
            let presets = [(60, "1 分钟"), (300, "5 分钟"), (900, "15 分钟"), (1500, "25 分钟")];
            let w = (row.w - 3.0 * 8.0) / 4.0;
            for (i, (secs, label)) in presets.into_iter().enumerate() {
                let pr = Rect::new(row.x + i as f32 * (w + 8.0), row.y, w, row.h);
                self.pill(g, pr, label, ORANGE, Btn::StartTimer(secs), a);
            }
        }
    }
}

impl Island {
    /// 另一个播放器的小圆泡：封面，没有封面时画个音符。
    fn draw_bubble(&self, g: &Renderer, b: Rect) {
        let s = self.split.value.clamp(0.0, 1.0);
        // 悬停时放大一点，按下时缩回去
        let k = self.bubble_hot.value * if matches!(self.press_on, Some((PressOn::Bubble, ..))) { 0.92 } else { 1.0 };
        let b = Rect::new(b.x + b.w * (1.0 - k) / 2.0, b.y + b.h * (1.0 - k) / 2.0, b.w * k, b.h * k);
        g.fill_rrect(b, b.h / 2.0, BLACK);
        g.stroke_rrect(b.inflate(-0.5), b.h / 2.0, 1.0, rgba(1.0, 1.0, 1.0, 0.07));
        let inner = b.inflate(-5.0);
        let a = ((s - 0.4) / 0.6).clamp(0.0, 1.0);
        match &self.other_art {
            Some((_, img)) => g.draw_image(img, inner, inner.h / 2.0, a),
            None => {
                let (cx, cy, k) = (inner.x + inner.w / 2.0, inner.y + inner.h / 2.0, inner.w / 24.0);
                let c = with_alpha(WHITE, 0.75 * a);
                g.fill_rrect(Rect::new(cx + 1.5 * k, cy - 7.0 * k, 2.0 * k, 11.0 * k), k, c);
                g.fill_rrect(Rect::new(cx - 4.0 * k, cy + 1.0 * k, 7.5 * k, 6.0 * k), 3.0 * k, c);
            }
        }
    }

    /// 便签页：标题和「+」，下面每行一条；悬停显示删除，点一下复制。
    fn draw_notes(&mut self, g: &Renderer, r: Rect, a: f32) {
        let pad = 24.0;
        let editing = self.editing == Some(Input::Note);
        g.text("便签", &g.fonts.title, Rect::new(r.x + pad, r.y + 16.0, 40.0, 24.0), with_alpha(WHITE, a));
        let total = self.notes.items.len();
        if total > 0 {
            g.text(&format!("{total} 条"), &g.fonts.small, Rect::new(r.x + pad + 40.0, r.y + 17.0, 80.0, 24.0), with_alpha(WHITE, 0.35 * a));
        }
        let add = Rect::new(r.x + r.w - pad - 26.0, r.y + 15.0, 26.0, 26.0);
        if !editing {
            let (cx, cy) = (add.x + 13.0, add.y + 13.0);
            self.squish_at(g, Btn::NewNote, cx, cy);
            let bg = 0.12 + 0.16 * self.glow(Btn::NewNote) + 0.1 * self.pressing(Btn::NewNote);
            g.fill_rrect(add, 13.0, with_alpha(WHITE, bg * a));
            g.line((cx - 5.5, cy), (cx + 5.5, cy), 1.8, with_alpha(WHITE, a));
            g.line((cx, cy - 5.5), (cx, cy + 5.5), 1.8, with_alpha(WHITE, a));
            self.unsquish(g);
            self.buttons.push((Btn::NewNote, add.inflate(5.0)));
        }

        let first = if editing {
            // 输入框（独立小窗口）就盖在这一行上，这里画个底
            g.fill_rrect(note_row(r, 0).inflate(-2.0), 8.0, with_alpha(rgba(0.17, 0.17, 0.18, 1.0), a));
            1
        } else {
            0
        };
        if total == 0 && !editing {
            g.text("点右上角的 + 记下一个想法", &g.fonts.small_center, Rect::new(r.x, r.y + 80.0, r.w, 22.0), with_alpha(WHITE, 0.35 * a));
            return;
        }
        let slots = NOTE_ROWS - first;
        let scroll = self.note_scroll.min(total.saturating_sub(slots));
        if total > slots {
            // 右边的滚动条
            let top = note_row(r, 0).y + 2.0;
            let track_h = NOTE_ROWS as f32 * NOTE_ROW - 6.0;
            let thumb_h = (track_h * slots as f32 / total as f32).max(16.0);
            let y = top + (track_h - thumb_h) * scroll as f32 / (total - slots) as f32;
            g.fill_rrect(Rect::new(r.x + r.w - 10.0, y, 3.0, thumb_h), 1.5, with_alpha(WHITE, 0.3 * a));
        }
        let items: Vec<(usize, String)> = self.notes.items.iter().cloned().enumerate().skip(scroll).take(slots).collect();
        for (k, (i, text)) in items.iter().enumerate() {
            let i = *i;
            let row = note_row(r, first + k);
            let hover = !editing && row.contains(self.cursor.0, self.cursor.1);
            // 底色淡入淡出；指着删除按钮时整行也算悬停；按下去再亮一点
            let glow = self.glow(Btn::CopyNote(i)).max(self.glow(Btn::DelNote(i)));
            let bg = 0.11 * glow + 0.07 * self.pressing(Btn::CopyNote(i));
            if bg > 0.0 {
                g.fill_rrect(row, 8.0, with_alpha(WHITE, bg * a));
            }
            let cy = row.y + row.h / 2.0;
            g.fill_rrect(Rect::new(row.x + 9.0, cy - 3.0, 6.0, 6.0), 3.0, with_alpha(YELLOW, a));
            g.text(text, &g.fonts.body, Rect::new(row.x + 24.0, row.y, row.w - 24.0 - 70.0, row.h), with_alpha(WHITE, 0.9 * a));
            if self.copied.is_some_and(|(j, _)| j == i) {
                g.text("已复制", &g.fonts.small_right, Rect::new(row.x + row.w - 70.0, row.y, 62.0, row.h), with_alpha(GREEN, a));
            } else if hover {
                g.text("复制", &g.fonts.small_right, Rect::new(row.x + row.w - 84.0, row.y, 50.0, row.h), with_alpha(WHITE, 0.4 * a));
                let close = Rect::new(row.x + row.w - 26.0, cy - 9.0, 18.0, 18.0);
                let (cx, ccy) = (close.x + 9.0, close.y + 9.0);
                let del = Btn::DelNote(i);
                self.squish_at(g, del, cx, ccy);
                // 指着删除时变红，提示这一下会删掉
                let k = self.glow(del);
                let gray = [0.35, 0.35, 0.38];
                let bg = rgba(gray[0] + (RED[0] - gray[0]) * k, gray[1] + (RED[1] - gray[1]) * k, gray[2] + (RED[2] - gray[2]) * k, 1.0);
                g.fill_rrect(close, 9.0, with_alpha(bg, a));
                let c = with_alpha(WHITE, a);
                g.line((cx - 3.5, ccy - 3.5), (cx + 3.5, ccy + 3.5), 1.6, c);
                g.line((cx - 3.5, ccy + 3.5), (cx + 3.5, ccy - 3.5), 1.6, c);
                self.unsquish(g);
                self.buttons.push((del, close.inflate(4.0)));
            }
            if !editing {
                self.buttons.push((Btn::CopyNote(i), row));
            }
        }
    }
}

/// 仪表盘右上角天气的位置（也是输入城市时输入框的位置）。
fn city_field(r: Rect) -> Rect {
    Rect::new(r.x + r.w - 24.0 - 190.0, r.y + 18.0, 190.0, 28.0)
}

/// 便签页第 i 行的位置。
fn note_row(r: Rect, i: usize) -> Rect {
    Rect::new(r.x + 16.0, r.y + 50.0 + i as f32 * NOTE_ROW, r.w - 32.0, NOTE_ROW - 2.0)
}

/// 简笔天气图标，(cx, cy) 是中心，大约 22×18。
fn draw_weather_icon(g: &Renderer, cx: f32, cy: f32, icon: Icon, day: bool, a: f32) {
    let sun = with_alpha(YELLOW, a);
    let cloud_c = with_alpha(rgba(0.85, 0.87, 0.9, 1.0), a);
    let small_sun = |x: f32, y: f32, k: f32| {
        if day {
            g.fill_rrect(Rect::new(x - 3.5 * k, y - 3.5 * k, 7.0 * k, 7.0 * k), 3.5 * k, sun);
            for i in 0..8 {
                let (s, c) = (i as f32 * std::f32::consts::FRAC_PI_4).sin_cos();
                g.line((x + c * 5.5 * k, y + s * 5.5 * k), (x + c * 7.5 * k, y + s * 7.5 * k), 1.4, sun);
            }
        } else {
            // 月牙：亮圆上叠一个黑圆
            let moon = with_alpha(rgba(0.95, 0.92, 0.75, 1.0), a);
            g.fill_rrect(Rect::new(x - 6.0 * k, y - 6.0 * k, 12.0 * k, 12.0 * k), 6.0 * k, moon);
            g.fill_rrect(Rect::new(x - 2.5 * k, y - 8.0 * k, 12.0 * k, 12.0 * k), 6.0 * k, BLACK);
        }
    };
    let cloud = |x: f32, y: f32| {
        g.fill_rrect(Rect::new(x - 10.0, y - 1.0, 20.0, 8.0), 4.0, cloud_c);
        g.fill_rrect(Rect::new(x - 7.0, y - 5.0, 9.0, 9.0), 4.5, cloud_c);
        g.fill_rrect(Rect::new(x - 2.0, y - 8.0, 11.0, 11.0), 5.5, cloud_c);
    };
    match icon {
        Icon::Clear => small_sun(cx, cy, 1.0),
        Icon::PartlyCloudy => {
            small_sun(cx - 4.0, cy - 3.0, 0.8);
            cloud(cx + 2.0, cy + 2.0);
        }
        Icon::Cloudy => cloud(cx, cy),
        Icon::Fog => {
            for i in 0..3 {
                let y = cy - 5.0 + i as f32 * 5.0;
                g.line((cx - 9.0 + i as f32 * 2.0, y), (cx + 9.0 - i as f32 * 2.0, y), 2.0, cloud_c);
            }
        }
        Icon::Rain | Icon::Snow | Icon::Thunder => {
            cloud(cx, cy - 3.0);
            let drop_c = with_alpha(if icon == Icon::Snow { WHITE } else { BLUE }, a);
            for i in 0..3 {
                let x = cx - 6.0 + i as f32 * 6.0;
                match icon {
                    Icon::Snow => g.fill_rrect(Rect::new(x - 1.5, cy + 7.0, 3.0, 3.0), 1.5, drop_c),
                    Icon::Rain => g.line((x + 1.0, cy + 6.0), (x - 1.0, cy + 10.0), 1.6, drop_c),
                    _ => {}
                }
            }
            if icon == Icon::Thunder {
                g.fill_poly(&[(cx + 1.0, cy + 4.0), (cx - 3.0, cy + 9.0), (cx, cy + 9.0), (cx - 1.5, cy + 13.0), (cx + 4.0, cy + 7.5), (cx + 1.0, cy + 7.5), (cx + 3.0, cy + 4.0)], sun);
            }
        }
    }
}

fn with_alpha(c: Color, a: f32) -> Color {
    [c[0], c[1], c[2], c[3] * a]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(expanded: bool, toast: Option<Toast>, media_active: bool, media_visible: bool) -> State {
        State { expanded, toast, media_active, media_visible, ..Default::default() }
    }

    #[test]
    fn expanded_wins() {
        assert_eq!(choose_layout(st(true, Some(Toast::Charging), true, true)), Layout::ExpandedMedia { shelf: false, lyric: 0 });
        assert_eq!(choose_layout(st(true, None, false, false)), Layout::ExpandedDash { shelf: false });
        let with_files = State { files: true, lyrics: 2, ..st(true, None, true, true) };
        assert_eq!(choose_layout(with_files), Layout::ExpandedMedia { shelf: true, lyric: 2 });
    }

    #[test]
    fn dash_page_when_media_playing() {
        let s = State { page: Page::Dash, ..st(true, None, true, true) };
        assert_eq!(choose_layout(s), Layout::ExpandedDash { shelf: false });
        // 没有音乐时音乐页落到仪表盘
        assert_eq!(choose_layout(State { page: Page::Media, ..st(true, None, false, false) }), Layout::ExpandedDash { shelf: false });
        let n = State { page: Page::Notes, notes: 3, files: true, ..st(true, None, true, true) };
        assert_eq!(choose_layout(n), Layout::ExpandedNotes { shelf: true, rows: 3 });
    }

    #[test]
    fn split_only_beside_compact_pills() {
        assert!(Layout::Media { lyric: true }.can_split());
        assert!(Layout::Idle { files: false, slim: false }.can_split());
        assert!(!Layout::Idle { files: false, slim: true }.can_split());
        assert!(!Layout::ExpandedMedia { shelf: false, lyric: 0 }.can_split());
        // 岛缩到 SPLIT_MAX_W 后，小圆泡也放得下
        let (_, h, _) = Layout::Media { lyric: true }.dims();
        assert!(SPLIT_MAX_W + BUBBLE_GAP + h + 20.0 <= WIN_W);
    }

    #[test]
    fn dropping_beats_everything() {
        let s = State { drop_hover: true, files: true, ..st(true, Some(Toast::Charging), true, true) };
        assert_eq!(choose_layout(s), Layout::Drop);
    }

    #[test]
    fn toasts_and_media() {
        assert_eq!(choose_layout(st(false, Some(Toast::Charging), false, false)), Layout::Charging);
        assert_eq!(choose_layout(st(false, Some(Toast::Level), true, true)), Layout::Level);
        assert_eq!(choose_layout(st(false, Some(Toast::TimerDone), false, false)), Layout::TimerDone);
        assert_eq!(choose_layout(st(false, Some(Toast::Track), true, true)), Layout::TrackToast);
        // 音乐已经隐藏时，切歌提示不弹
        assert_eq!(choose_layout(st(false, Some(Toast::Track), true, false)), Layout::Idle { files: false, slim: false });
        assert_eq!(choose_layout(st(false, None, true, true)), Layout::Media { lyric: false });
        assert_eq!(choose_layout(State { lyrics: 1, ..st(false, None, true, true) }), Layout::Media { lyric: true });
        assert_eq!(choose_layout(State { files: true, ..st(false, None, false, false) }), Layout::Idle { files: true, slim: false });
        assert_eq!(choose_layout(st(false, Some(Toast::LowBattery), true, true)), Layout::LowBattery);
        assert_eq!(choose_layout(st(false, Some(Toast::Device), true, true)), Layout::Device);
        assert_eq!(choose_layout(st(false, Some(Toast::Privacy), false, false)), Layout::Privacy);
    }

    #[test]
    fn timer_shows_when_no_music() {
        assert_eq!(choose_layout(State { timer: true, ..st(false, None, false, false) }), Layout::Timer);
        // 有音乐时音乐优先（倒计时画在音乐紧凑态的右边）
        assert_eq!(choose_layout(State { timer: true, ..st(false, None, true, true) }), Layout::Media { lyric: false });
    }

    #[test]
    fn slim_only_when_nothing_to_show() {
        let slim = State { slim: true, ..st(false, None, false, false) };
        assert_eq!(choose_layout(slim), Layout::Idle { files: false, slim: true });
        // 有暂存文件、有应用在用麦克风时要露出药丸
        assert_eq!(choose_layout(State { files: true, ..slim }), Layout::Idle { files: true, slim: false });
        assert_eq!(choose_layout(State { privacy: true, ..slim }), Layout::Idle { files: false, slim: false });
        assert_eq!(Layout::Idle { files: false, slim: true }.top(), SLIM_TOP);
    }

    #[test]
    fn every_layout_fits_window() {
        let all = [
            Layout::Idle { files: false, slim: false },
            Layout::Idle { files: false, slim: true },
            Layout::Idle { files: true, slim: false },
            Layout::Media { lyric: false },
            Layout::Media { lyric: true },
            Layout::TrackToast,
            Layout::Charging,
            Layout::Level,
            Layout::Timer,
            Layout::TimerDone,
            Layout::LowBattery,
            Layout::Device,
            Layout::Privacy,
            Layout::Drop,
            Layout::ExpandedMedia { shelf: false, lyric: 0 },
            Layout::ExpandedMedia { shelf: true, lyric: 2 },
            Layout::ExpandedDash { shelf: false },
            Layout::ExpandedDash { shelf: true },
            Layout::ExpandedNotes { shelf: false, rows: 0 },
            Layout::ExpandedNotes { shelf: true, rows: NOTE_ROWS as u8 },
        ];
        for l in all {
            let (w, h, r) = l.dims();
            assert!(w + 30.0 <= WIN_W && h + TOP + 20.0 <= WIN_H, "{l:?} 放不下");
            assert!(r <= h / 2.0);
        }
    }

    #[test]
    fn shelf_row_fits_max_items() {
        let (w, ..) = Layout::ExpandedDash { shelf: true }.dims();
        let used = 2.0 * 20.0 + shelf::MAX_ITEMS as f32 * (TILE_W + TILE_GAP);
        assert!(used <= w, "{used} > {w}");
    }

    #[test]
    fn seek_zone_is_generous() {
        let z = seek_zone(Rect::new(100.0, 50.0, 200.0, 5.0));
        assert!(z.contains(100.0, 42.0) && z.contains(300.0, 62.0));
        assert!(!z.contains(100.0, 30.0));
    }
}
