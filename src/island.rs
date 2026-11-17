//! 灵动岛本体：状态机、弹簧形变、各状态的内容绘制、命中测试。
//! 所有坐标都是逻辑像素（DIP），窗口内坐标系。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows_numerics::{Matrix3x2, Vector2};

use crate::media::{ART, Cmd, Media, MediaInfo};
use crate::render::{Color, Image, Rect, Renderer, rgba};
use crate::shelf::{self, ICON, Shelf};
use crate::spring::Spring;
use crate::system::{self, Battery};

/// 窗口逻辑尺寸：容纳最大展开态 + 回弹余量。
pub const WIN_W: f32 = 460.0;
pub const WIN_H: f32 = 270.0;
const TOP: f32 = 8.0;
/// 展开态底部暂存区那一行的高度。
const SHELF_ROW: f32 = 92.0;
const TILE_W: f32 = 68.0;
const TILE_GAP: f32 = 4.0;
/// 按住文件拖多远算"拖出去"。
const DRAG_SLOP: f32 = 5.0;

const HOVER_DELAY: Duration = Duration::from_millis(140);
const LEAVE_DELAY: Duration = Duration::from_millis(320);
const PAUSED_LINGER: Duration = Duration::from_secs(30);
const TOAST_TIME: Duration = Duration::from_millis(2800);

const BLACK: Color = rgba(0.0, 0.0, 0.0, 1.0);
const WHITE: Color = rgba(1.0, 1.0, 1.0, 1.0);
const GREEN: Color = rgba(0.29, 0.86, 0.39, 1.0);
const BLUE: Color = rgba(0.30, 0.62, 1.0, 1.0);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    /// files：暂存区里有文件，药丸上显示个数。
    Idle { files: bool },
    Media,
    TrackToast,
    Charging,
    /// 正在往岛上拖文件。
    Drop,
    /// shelf：底部多一行暂存的文件。
    ExpandedMedia { shelf: bool },
    ExpandedDash { shelf: bool },
}

impl Layout {
    /// (宽, 高, 圆角)
    pub fn dims(self) -> (f32, f32, f32) {
        let row = |s: bool| if s { SHELF_ROW } else { 0.0 };
        match self {
            Layout::Idle { files: false } => (120.0, 30.0, 15.0),
            Layout::Idle { files: true } => (150.0, 30.0, 15.0),
            Layout::Media => (250.0, 34.0, 17.0),
            Layout::TrackToast => (360.0, 34.0, 17.0),
            Layout::Charging => (300.0, 34.0, 17.0),
            Layout::Drop => (400.0, 120.0, 40.0),
            Layout::ExpandedMedia { shelf } => (400.0, 150.0 + row(shelf), 40.0),
            Layout::ExpandedDash { shelf: false } => (340.0, 108.0, 36.0),
            Layout::ExpandedDash { shelf: true } => (400.0, 108.0 + SHELF_ROW, 36.0),
        }
    }

    /// 展开态上半部分（不含暂存区那一行）的高度。
    fn base_height(self) -> f32 {
        match self {
            Layout::ExpandedMedia { .. } => 150.0,
            Layout::ExpandedDash { .. } => 108.0,
            _ => self.dims().1,
        }
    }

    fn has_shelf(self) -> bool {
        matches!(self, Layout::ExpandedMedia { shelf: true } | Layout::ExpandedDash { shelf: true })
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
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Toast {
    Track,
    Charging,
}

/// 选择当前应显示的形态。纯函数，方便测试。
pub fn choose_layout(s: State) -> Layout {
    if s.drop_hover {
        return Layout::Drop;
    }
    if s.expanded {
        return if s.media_active { Layout::ExpandedMedia { shelf: s.files } } else { Layout::ExpandedDash { shelf: s.files } };
    }
    match s.toast {
        Some(Toast::Charging) => Layout::Charging,
        Some(Toast::Track) if s.media_visible => Layout::TrackToast,
        _ if s.media_visible => Layout::Media,
        _ => Layout::Idle { files: s.files },
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Btn {
    Prev,
    Toggle,
    Next,
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
    Other,
}

pub struct Island {
    w: Spring,
    h: Spring,
    r: Spring,
    press: Spring,
    target: Layout,
    shown: Layout,
    content_alpha: f32,

    hover_since: Option<Instant>,
    leave_since: Option<Instant>,
    expanded: bool,
    pressed: bool,

    pub media: Media,
    info: MediaInfo,
    art: Option<(u64, Image)>,
    last_track: u64,
    last_playing: Option<Instant>,

    battery: Option<Battery>,
    toast: Option<(Toast, Instant)>,

    pub shelf: Shelf,
    icons: HashMap<u64, Image>,
    tiles: Vec<Tile>,
    drop_hover: bool,
    cursor: (f32, f32),
    hovered_tile: Option<u64>,
    press_on: Option<(PressOn, f32, f32)>,
    /// 要拖出去的文件 id，由主循环在借用之外发起拖放。
    pending_drag: Option<u64>,

    buttons: Vec<(Btn, Rect)>,
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
    pub fn new(media: Media, shelf: Shelf) -> Self {
        let start = Layout::Idle { files: !shelf.items.is_empty() };
        let (w, h, r) = start.dims();
        let now = Instant::now();
        Island {
            w: Spring::new(w, 0.42, 0.78),
            h: Spring::new(h, 0.42, 0.82),
            r: Spring::new(r, 0.42, 0.9),
            press: Spring::new(1.0, 0.25, 0.6),
            target: start,
            shown: start,
            content_alpha: 1.0,
            hover_since: None,
            leave_since: None,
            expanded: false,
            pressed: false,
            media,
            info: MediaInfo::default(),
            art: None,
            last_track: 0,
            last_playing: None,
            battery: system::battery(),
            toast: None,
            shelf,
            icons: HashMap::new(),
            tiles: Vec::new(),
            drop_hover: false,
            cursor: (-1.0, -1.0),
            hovered_tile: None,
            press_on: None,
            pending_drag: None,
            buttons: Vec::new(),
            anim_t: 0.0,
            last_tick: now,
            last_render: now,
            dirty: true,
        }
    }

    /// 岛当前（动画中）的外框。
    pub fn rect(&self) -> Rect {
        let (w, h) = (self.w.value, self.h.value);
        Rect::new((WIN_W - w) / 2.0, TOP, w, h)
    }

    /// 鼠标是否在岛上（用于决定是否穿透点击）。
    pub fn hit(&self, x: f32, y: f32) -> bool {
        self.rect().inflate(3.0).contains(x, y)
    }

    pub fn poll_system(&mut self) {
        let now_batt = system::battery();
        if let (Some(old), Some(new)) = (self.battery, now_batt) {
            if !old.charging && new.charging {
                self.toast = Some((Toast::Charging, Instant::now()));
            }
            if old != new {
                self.dirty = true;
            }
        }
        self.battery = now_batt;
    }

    pub fn on_mouse(&mut self, inside: bool, cursor: (f32, f32), now: Instant) {
        self.cursor = cursor;
        let hovered = if inside { self.tile_at(cursor.0, cursor.1).map(|t| t.0) } else { None };
        if hovered != self.hovered_tile {
            self.hovered_tile = hovered;
            self.dirty = true;
        }
        if inside {
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
            }
            if self.expanded {
                let since = *self.leave_since.get_or_insert(now);
                if now - since >= LEAVE_DELAY {
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

    pub fn mouse_down(&mut self, x: f32, y: f32) {
        self.pressed = true;
        let on = match self.tile_at(x, y) {
            Some((id, true)) => PressOn::Close(id),
            Some((id, false)) => PressOn::Tile(id),
            None => PressOn::Other,
        };
        // 按在文件上时岛不缩，免得文件跟着抖
        if on == PressOn::Other {
            self.press.target = 0.96;
        }
        self.press_on = Some((on, x, y));
    }

    pub fn mouse_move(&mut self, x: f32, y: f32) {
        if let Some((PressOn::Tile(id), px, py)) = self.press_on
            && (x - px).hypot(y - py) >= DRAG_SLOP {
                self.press_on = None;
                self.pressed = false;
                self.press.target = 1.0;
                self.pending_drag = Some(id);
            }
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
        self.pressed = false;
        self.press.target = 1.0;
        let press = self.press_on.take();
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
        if !matches!(self.shown, Layout::ExpandedMedia { .. }) {
            return;
        }
        if let Some((b, _)) = self.buttons.iter().find(|(_, r)| r.contains(x, y)) {
            self.media.send(match b {
                Btn::Prev => Cmd::Prev,
                Btn::Toggle => Cmd::Toggle,
                Btn::Next => Cmd::Next,
            });
            // 乐观更新，让按钮立刻有反馈
            if *b == Btn::Toggle {
                self.info.playing = !self.info.playing;
            }
            self.dirty = true;
        }
    }

    pub fn tick(&mut self, now: Instant) {
        let dt = (now - self.last_tick).as_secs_f32();
        self.last_tick = now;

        if let Some(info) = self.media.snapshot_if_newer(self.info.version) {
            if info.track_id != self.last_track {
                if self.last_track != 0 && info.playing && info.active {
                    self.toast = Some((Toast::Track, now));
                }
                self.last_track = info.track_id;
            }
            self.info = info;
            self.dirty = true;
        }
        if self.info.playing {
            self.last_playing = Some(now);
        }
        if let Some((_, t)) = self.toast
            && now - t >= TOAST_TIME {
                self.toast = None;
            }

        let media_visible = self.info.active && self.last_playing.is_some_and(|t| now - t < PAUSED_LINGER);
        let target = choose_layout(State {
            expanded: self.expanded,
            drop_hover: self.drop_hover,
            toast: self.toast.map(|t| t.0),
            media_active: self.info.active,
            media_visible,
            files: !self.shelf.items.is_empty(),
        });
        if target != self.target {
            self.target = target;
            let (w, h, r) = target.dims();
            self.w.target = w;
            self.h.target = h;
            self.r.target = r;
        }

        // 内容先淡出，换成新形态的内容后，随尺寸接近目标再淡入
        if self.shown != self.target {
            self.content_alpha -= dt / 0.07;
            if self.content_alpha <= 0.0 {
                self.content_alpha = 0.0;
                self.shown = self.target;
            }
        } else if self.content_alpha < 1.0 {
            self.content_alpha = (self.content_alpha + dt / 0.22).min(1.0);
        }

        for s in [&mut self.w, &mut self.h, &mut self.r, &mut self.press] {
            s.step(dt);
        }
        self.anim_t += dt;
    }

    fn animating(&self) -> bool {
        !(self.w.settled() && self.h.settled() && self.r.settled() && self.press.settled())
            || self.shown != self.target
            || self.content_alpha < 1.0
    }

    /// 判断要不要现在出一帧，以及空闲时多久后再来。
    pub fn pace(&mut self, now: Instant) -> (bool, Pace) {
        if self.animating() {
            return (true, Pace::Animating);
        }
        let periodic = match self.shown {
            Layout::Media | Layout::TrackToast | Layout::ExpandedMedia { .. } if self.info.playing => Some(33),
            Layout::ExpandedDash { .. } => Some(1000),
            _ => None,
        };
        let since = (now - self.last_render).as_millis() as u32;
        let due = self.dirty || periodic.is_some_and(|p| since >= p);
        let wait = periodic.map(|p| p.saturating_sub(if due { 0 } else { since })).unwrap_or(40).clamp(1, 40);
        (due, Pace::Wait(wait))
    }

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
        // 暂存文件的图标：新的建、删掉的丢
        self.icons.retain(|id, _| self.shelf.items.iter().any(|i| i.id == *id));
        for item in &self.shelf.items {
            if let (false, Some(px)) = (self.icons.contains_key(&item.id), &item.icon)
                && let Ok(img) = g.image(px, ICON) {
                    self.icons.insert(item.id, img);
                }
        }

        g.begin();
        let r = self.rect();
        let center = Vector2 { X: r.x + r.w / 2.0, Y: r.y + r.h / 2.0 };
        let s = self.press.value;
        g.set_transform(Matrix3x2::scale_around(s, s, center));

        let radius = self.r.value.min(r.h / 2.0).max(0.0);
        g.fill_rrect(r, radius, BLACK);
        g.stroke_rrect(r.inflate(-0.5), radius, 1.0, rgba(1.0, 1.0, 1.0, 0.07));

        // 尺寸还差得远时不画内容，避免内容溢出到形变中的岛外
        let (tw, th, _) = self.shown.dims();
        let gap = (r.w - tw).abs() + (r.h - th).abs();
        let a = self.content_alpha * (1.0 - gap / 90.0).clamp(0.0, 1.0);
        self.buttons.clear();
        self.tiles.clear();
        if a > 0.01 {
            // 形变途中内容不能溢出岛外；上下各让出圆角的一部分
            let inset = (radius * 0.3).min(8.0);
            g.push_clip(Rect::new(r.x, r.y + inset, r.w, r.h - 2.0 * inset));
            match self.shown {
                Layout::Idle { files } => {
                    if files {
                        self.draw_idle_files(g, r, a);
                    }
                }
                Layout::Media => self.draw_compact(g, r, a, None),
                Layout::TrackToast => {
                    let title = self.info.title.clone();
                    self.draw_compact(g, r, a, Some(&title));
                }
                Layout::Charging => self.draw_charging(g, r, a),
                Layout::Drop => self.draw_drop(g, r, a),
                Layout::ExpandedMedia { .. } => self.draw_expanded_media(g, r, a),
                Layout::ExpandedDash { .. } => self.draw_dash(g, r, a),
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
            let hovered = self.hovered_tile == Some(id);
            if hovered {
                g.fill_rrect(tile, 12.0, with_alpha(WHITE, 0.09 * a));
            }
            let icon = Rect::new(x + (TILE_W - 44.0) / 2.0, y + 6.0, 44.0, 44.0);
            match self.icons.get(&id) {
                Some(img) => g.draw_image(img, icon, 6.0, a),
                None => self.draw_doc_glyph(g, icon.x + 12.0, icon.y + 8.0, 2.0, with_alpha(WHITE, 0.6 * a)),
            }
            let name = self.shelf.get(id).map(|i| i.name.as_str()).unwrap_or("");
            g.text(name, &g.fonts.tiny_center, Rect::new(x + 2.0, y + 54.0, TILE_W - 4.0, 16.0), with_alpha(WHITE, 0.8 * a));

            let close = Rect::new(x + TILE_W - 20.0, y + 1.0, 18.0, 18.0);
            if hovered {
                let (cx, cy) = (close.x + 9.0, close.y + 9.0);
                g.fill_rrect(close, 9.0, with_alpha(rgba(0.35, 0.35, 0.38, 1.0), a));
                let c = with_alpha(WHITE, a);
                g.line((cx - 3.5, cy - 3.5), (cx + 3.5, cy + 3.5), 1.6, c);
                g.line((cx - 3.5, cy + 3.5), (cx + 3.5, cy - 3.5), 1.6, c);
            }
            self.tiles.push(Tile { id, rect: tile, close });
            x += TILE_W + TILE_GAP;
        }
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

    fn draw_bars(&self, g: &Renderer, right: f32, cy: f32, max_h: f32, a: f32) {
        let [cr, cg, cb] = self.info.accent;
        let c = rgba(cr, cg, cb, a);
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

    fn draw_compact(&self, g: &Renderer, r: Rect, a: f32, title: Option<&str>) {
        let art = Rect::new(r.x + 6.0, r.y + (r.h - 22.0) / 2.0, 22.0, 22.0);
        self.draw_art(g, art, 6.0, a);
        self.draw_bars(g, r.x + r.w - 14.0, r.y + r.h / 2.0, 14.0, a);
        if let Some(t) = title {
            g.text(t, &g.fonts.small, Rect::new(r.x + 38.0, r.y, r.w - 38.0 - 44.0, r.h), with_alpha(WHITE, a));
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

    fn draw_expanded_media(&mut self, g: &Renderer, r: Rect, a: f32) {
        let pad = 20.0;
        let art = Rect::new(r.x + pad, r.y + pad, 62.0, 62.0);
        self.draw_art(g, art, 14.0, a);

        let tx = art.x + art.w + 14.0;
        let tw = r.x + r.w - pad - 34.0 - tx;
        g.text(&self.info.title, &g.fonts.title, Rect::new(tx, art.y + 8.0, tw, 22.0), with_alpha(WHITE, a));
        g.text(&self.info.artist, &g.fonts.body, Rect::new(tx, art.y + 32.0, tw, 20.0), with_alpha(WHITE, 0.55 * a));
        self.draw_bars(g, r.x + r.w - pad, art.y + 19.0, 16.0, a);

        let mut row_y = art.y + art.h + 14.0;
        if self.info.duration > 1.0 {
            let pos = self.info.position_now();
            let bar = Rect::new(r.x + pad + 40.0, row_y + 6.0, r.w - 2.0 * pad - 80.0, 5.0);
            g.fill_rrect(bar, 2.5, with_alpha(WHITE, 0.18 * a));
            let p = (pos / self.info.duration).clamp(0.0, 1.0) as f32;
            g.fill_rrect(Rect::new(bar.x, bar.y, (bar.w * p).max(5.0), bar.h), 2.5, with_alpha(WHITE, 0.85 * a));
            let lc = with_alpha(WHITE, 0.5 * a);
            g.text(&system::fmt_secs(pos), &g.fonts.small, Rect::new(r.x + pad, row_y, 36.0, 17.0), lc);
            g.text(&format!("-{}", system::fmt_secs(self.info.duration - pos)), &g.fonts.small_right, Rect::new(r.x + r.w - pad - 40.0, row_y, 40.0, 17.0), lc);
            row_y += 20.0;
        }

        let cy = row_y + 13.0;
        let cx = r.x + r.w / 2.0;
        let c = with_alpha(WHITE, a);
        for (btn, bx) in [(Btn::Prev, cx - 64.0), (Btn::Toggle, cx), (Btn::Next, cx + 64.0)] {
            let pressed_dim = if self.pressed { 0.9 } else { 1.0 };
            let c = with_alpha(c, pressed_dim);
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
                Btn::Prev => {
                    g.fill_poly(&[(bx + 11.0, cy - 7.0), (bx, cy), (bx + 11.0, cy + 7.0)], c);
                    g.fill_poly(&[(bx, cy - 7.0), (bx - 11.0, cy), (bx, cy + 7.0)], c);
                }
            }
            self.buttons.push((btn, Rect::new(bx - 24.0, cy - 18.0, 48.0, 36.0)));
        }
    }

    fn draw_dash(&self, g: &Renderer, r: Rect, a: f32) {
        let clock = system::clock();
        let pad = 24.0;
        g.text(&clock.time, &g.fonts.clock, Rect::new(r.x + pad, r.y + 16.0, 160.0, 46.0), with_alpha(WHITE, a));
        g.text(&clock.date, &g.fonts.body, Rect::new(r.x + pad + 2.0, r.y + 62.0, 200.0, 22.0), with_alpha(WHITE, 0.55 * a));
        if let Some(b) = self.battery {
            let bx = r.x + r.w - pad - 26.0;
            let cy = r.y + 38.0;
            self.draw_battery(g, bx, cy, b, a);
            let c = if b.charging { GREEN } else { WHITE };
            g.text(&format!("{}%", b.percent), &g.fonts.small_right, Rect::new(bx - 64.0, cy - 9.0, 58.0, 18.0), with_alpha(c, a));
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
        assert_eq!(choose_layout(st(true, Some(Toast::Charging), true, true)), Layout::ExpandedMedia { shelf: false });
        assert_eq!(choose_layout(st(true, None, false, false)), Layout::ExpandedDash { shelf: false });
        let with_files = State { files: true, ..st(true, None, true, true) };
        assert_eq!(choose_layout(with_files), Layout::ExpandedMedia { shelf: true });
    }

    #[test]
    fn dropping_beats_everything() {
        let s = State { drop_hover: true, files: true, ..st(true, Some(Toast::Charging), true, true) };
        assert_eq!(choose_layout(s), Layout::Drop);
    }

    #[test]
    fn toasts_and_media() {
        assert_eq!(choose_layout(st(false, Some(Toast::Charging), false, false)), Layout::Charging);
        assert_eq!(choose_layout(st(false, Some(Toast::Track), true, true)), Layout::TrackToast);
        // 音乐已经隐藏时，切歌提示不弹
        assert_eq!(choose_layout(st(false, Some(Toast::Track), true, false)), Layout::Idle { files: false });
        assert_eq!(choose_layout(st(false, None, true, true)), Layout::Media);
        assert_eq!(choose_layout(State { files: true, ..st(false, None, false, false) }), Layout::Idle { files: true });
    }

    #[test]
    fn every_layout_fits_window() {
        let all = [
            Layout::Idle { files: false },
            Layout::Idle { files: true },
            Layout::Media,
            Layout::TrackToast,
            Layout::Charging,
            Layout::Drop,
            Layout::ExpandedMedia { shelf: false },
            Layout::ExpandedMedia { shelf: true },
            Layout::ExpandedDash { shelf: false },
            Layout::ExpandedDash { shelf: true },
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
}
