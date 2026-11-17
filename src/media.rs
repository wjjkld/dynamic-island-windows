//! 媒体：通过系统媒体传输控件（SMTC）读取正在播放的内容并控制播放。
//! 跑在独立的 MTA 线程上，WinRT 异步调用直接 join 阻塞等待，不影响 UI 线程。

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::DataReader;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows::core::{Result, RuntimeType};

use crate::lyrics::{self, Lyrics};

/// 封面解码后的边长（像素，BGRA 预乘）。
pub const ART: u32 = 128;

#[derive(Clone, Default)]
pub struct MediaInfo {
    pub active: bool,
    pub title: String,
    pub artist: String,
    pub playing: bool,
    /// 采样时刻的播放位置（秒）和对应的本地时刻，用来外推进度。
    pub position: f64,
    pub sampled_at: Option<Instant>,
    pub duration: f64,
    /// 时间轴起点（秒）。跳转时要加回去。
    pub start: f64,
    /// 播放器是否支持跳转进度。
    pub can_seek: bool,
    /// 当前歌曲的歌词（查到才有）。
    pub lyrics: Option<Arc<Lyrics>>,
    /// 每换一首歌 +1。
    pub track_id: u64,
    pub art: Option<Arc<Vec<u8>>>,
    /// 从封面取的强调色（RGB 0..1）。
    pub accent: [f32; 3],
    /// 播放器名（Spotify、网易云音乐…）。
    pub app: String,
    /// 另一个正在播放的播放器：显示成岛右边的小圆泡，点一下互换。
    pub other: Option<Other>,
    /// 每次有变化 +1，UI 线程据此判断要不要重新拷贝。
    pub version: u64,
}

#[derive(Clone, Default, PartialEq)]
pub struct Other {
    pub app: String,
    pub title: String,
    pub art: Option<Arc<Vec<u8>>>,
    /// 换了歌（封面）+1。
    pub art_id: u64,
}

impl MediaInfo {
    pub fn position_now(&self) -> f64 {
        let mut p = self.position;
        if self.playing
            && let Some(t) = self.sampled_at {
                p += t.elapsed().as_secs_f64();
            }
        if self.duration > 0.0 { p.min(self.duration) } else { p }
    }
}

pub enum Cmd {
    Toggle,
    Next,
    Prev,
    /// 跳到第几秒（相对歌曲开头）。
    Seek(f64),
    /// 打开/关闭歌词查询。
    Lyrics(bool),
    /// 和另一个正在播放的播放器互换主位。
    Swap,
    /// 内部用：系统事件通知需要刷新。
    Refresh,
    /// 内部用：系统的「当前会话」换了。
    CurrentChanged,
}

pub struct Media {
    pub info: Arc<Mutex<MediaInfo>>,
    tx: Sender<Cmd>,
}

impl Media {
    pub fn start(wake: impl Fn() + Send + Sync + 'static, lyrics_on: bool) -> Media {
        let info = Arc::new(Mutex::new(MediaInfo { accent: DEFAULT_ACCENT, ..Default::default() }));
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let (tx, rx) = mpsc::channel();
        let (ltx, lrx) = mpsc::channel::<LyricReq>();
        let shared = info.clone();
        let event_tx = tx.clone();
        let w = wake.clone();
        std::thread::Builder::new()
            .name("media".into())
            .spawn(move || {
                let ctx = Ctx { info: &shared, rx: &rx, tx: &event_tx, lyrics: &ltx, wake: &*w, lyrics_on };
                if let Err(e) = run(ctx) {
                    eprintln!("[media] 线程退出: {e:?}");
                }
            })
            .expect("spawn media thread");
        let shared = info.clone();
        std::thread::Builder::new()
            .name("lyrics".into())
            .spawn(move || lyrics_worker(&shared, &lrx, &*wake))
            .expect("spawn lyrics thread");
        Media { info, tx }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    pub fn snapshot_if_newer(&self, version: u64) -> Option<MediaInfo> {
        let g = self.info.lock().ok()?;
        (g.version != version).then(|| g.clone())
    }
}

const DEFAULT_ACCENT: [f32; 3] = [0.92, 0.92, 0.95];

struct LyricReq {
    track_id: u64,
    title: String,
    artist: String,
    duration: f64,
}

/// 歌词在单独线程查（网络可能慢），只处理最新的一首。
fn lyrics_worker(info: &Mutex<MediaInfo>, rx: &Receiver<LyricReq>, wake: &(dyn Fn() + Send + Sync)) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    while let Ok(mut req) = rx.recv() {
        while let Ok(r) = rx.try_recv() {
            req = r;
        }
        let found = lyrics::find(&lyrics::Query { title: &req.title, artist: &req.artist, duration: req.duration });
        let mut g = info.lock().unwrap();
        if g.track_id == req.track_id {
            g.lyrics = found.map(Arc::new);
            g.version += 1;
            drop(g);
            wake();
        }
    }
}

struct Ctx<'a> {
    info: &'a Mutex<MediaInfo>,
    rx: &'a Receiver<Cmd>,
    tx: &'a Sender<Cmd>,
    lyrics: &'a Sender<LyricReq>,
    wake: &'a (dyn Fn() + Send + Sync),
    lyrics_on: bool,
}

fn request_lyrics(c: &Ctx, g: &MediaInfo) {
    if c.lyrics_on && g.active && !g.title.is_empty() {
        let _ = c.lyrics.send(LyricReq { track_id: g.track_id, title: g.title.clone(), artist: g.artist.clone(), duration: g.duration });
    }
}

/// 生成一个只负责"通知媒体线程去刷新"的 WinRT 事件回调。
fn notifier<S: RuntimeType + 'static, A: RuntimeType + 'static>(tx: &Sender<Cmd>) -> TypedEventHandler<S, A> {
    notify_with(tx, || Cmd::Refresh)
}

fn notify_with<S: RuntimeType + 'static, A: RuntimeType + 'static>(
    tx: &Sender<Cmd>,
    cmd: fn() -> Cmd,
) -> TypedEventHandler<S, A> {
    let tx = tx.clone();
    TypedEventHandler::new(move |_, _| {
        let _ = tx.send(cmd());
        Ok(())
    })
}

fn is_playing(s: &Session) -> bool {
    s.GetPlaybackInfo().and_then(|p| p.PlaybackStatus()).is_ok_and(|st| st == Status::Playing)
}

fn app_id(s: &Session) -> String {
    s.SourceAppUserModelId().map(|h| h.to_string_lossy()).unwrap_or_default()
}

/// AppUserModelId → 给人看的播放器名。
pub fn app_name(id: &str) -> String {
    let l = id.to_lowercase();
    const KNOWN: [(&str, &str); 18] = [
        ("spotify", "Spotify"),
        ("cloudmusic", "网易云音乐"),
        ("qqmusic", "QQ音乐"),
        ("kugou", "酷狗音乐"),
        ("kwmusic", "酷我音乐"),
        ("applemusic", "Apple Music"),
        ("zunemusic", "媒体播放器"),
        ("zunevideo", "电影和电视"),
        ("msedge", "Edge"),
        ("chrome", "Chrome"),
        ("firefox", "Firefox"),
        ("308046b0af4a39cb", "Firefox"),
        ("bilibili", "哔哩哔哩"),
        ("potplayer", "PotPlayer"),
        ("vlc", "VLC"),
        ("foobar2000", "foobar2000"),
        ("musicbee", "MusicBee"),
        ("aimp", "AIMP"),
    ];
    if let Some((_, name)) = KNOWN.iter().find(|(k, _)| l.contains(k)) {
        return name.to_string();
    }
    // 其它：取最后一段，去掉 .exe 和商店包名后缀
    let last = id.rsplit(['\\', '/', '!']).next().unwrap_or(id);
    let last = last.strip_suffix(".exe").or_else(|| last.strip_suffix(".EXE")).unwrap_or(last);
    let last = last.split('_').next().unwrap_or(last);
    last.rsplit('.').next().unwrap_or(last).to_string()
}

/// 当前会话上挂的事件，换会话时整体替换（Drop 时自动解绑）。
struct Subscription {
    session: Session,
    props: i64,
    playback: i64,
}

impl Subscription {
    fn new(session: Session, tx: &Sender<Cmd>) -> Result<Self> {
        let props = session.MediaPropertiesChanged(&notifier(tx))?;
        let playback = session.PlaybackInfoChanged(&notifier(tx))?;
        Ok(Subscription { session, props, playback })
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.session.RemoveMediaPropertiesChanged(self.props);
        let _ = self.session.RemovePlaybackInfoChanged(self.playback);
    }
}

fn run(mut c: Ctx) -> Result<()> {
    let (info, rx, tx) = (c.info, c.rx, c.tx);
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let mgr = Manager::RequestAsync()?.join()?;
    let wic: IWICImagingFactory = unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)? };
    let mut last_key = String::new();

    // 事件驱动：会话增减、当前会话切换、曲目/播放状态变化时系统通知我们；
    // 另留一个低频兜底轮询，用来校正进度（拖动进度条不一定触发事件）。
    let current_token = mgr.CurrentSessionChanged(&notify_with(tx, || Cmd::CurrentChanged))?;
    let sessions_token = mgr.SessionsChanged(&notifier(tx))?;
    // 每个会话都挂上事件，别的播放器开始 / 停止播放时也能知道
    let mut subs: Vec<Subscription> = Vec::new();
    // 用户点小圆泡选中的会话；没有就跟随系统的当前会话
    let mut selected: Option<Session> = None;
    let mut primary: Option<Session> = None;
    let mut other: Option<Session> = None;
    let mut other_key = String::new();

    loop {
        let playing = info.lock().map(|g| g.playing).unwrap_or(false);
        let fallback = Duration::from_secs(if playing { 3 } else { 10 });
        let mut current_changed = false;
        match rx.recv_timeout(fallback) {
            Ok(first @ (Cmd::Refresh | Cmd::CurrentChanged)) => {
                #[cfg(debug_assertions)]
                if std::env::var_os("ISLAND_TRACE").is_some() {
                    eprintln!("[media] refresh {:?}", std::time::SystemTime::now());
                }
                // 一次变化常常连发好几个事件，攒一下再查；夹在中间的用户操作放回队列
                std::thread::sleep(Duration::from_millis(60));
                current_changed = matches!(first, Cmd::CurrentChanged);
                let mut later = Vec::new();
                while let Ok(m) = rx.try_recv() {
                    match m {
                        Cmd::Refresh => {}
                        Cmd::CurrentChanged => current_changed = true,
                        m => later.push(m),
                    }
                }
                for m in later {
                    let _ = tx.send(m);
                }
            }
            Ok(Cmd::Swap) => {
                if other.is_some() {
                    selected = other.take();
                }
            }
            Ok(Cmd::Lyrics(on)) => {
                c.lyrics_on = on;
                let mut g = info.lock().unwrap();
                if on {
                    request_lyrics(&c, &g);
                } else if g.lyrics.take().is_some() {
                    g.version += 1;
                    drop(g);
                    (c.wake)();
                }
                continue;
            }
            Ok(cmd) => {
                if let Some(s) = &primary {
                    let start = info.lock().map(|g| g.start).unwrap_or(0.0);
                    let r = match cmd {
                        Cmd::Toggle => s.TryTogglePlayPauseAsync(),
                        Cmd::Next => s.TrySkipNextAsync(),
                        Cmd::Prev => s.TrySkipPreviousAsync(),
                        Cmd::Seek(sec) => s.TryChangePlaybackPositionAsync(((start + sec.max(0.0)) * 1e7) as i64),
                        Cmd::Refresh | Cmd::CurrentChanged | Cmd::Lyrics(_) | Cmd::Swap => unreachable!(),
                    };
                    if let Ok(op) = r {
                        let _ = op.join();
                    }
                }
                // 给播放器一点时间更新状态
                std::thread::sleep(Duration::from_millis(150));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let _ = mgr.RemoveCurrentSessionChanged(current_token);
                let _ = mgr.RemoveSessionsChanged(sessions_token);
                return Ok(());
            }
        }

        let sessions: Vec<Session> = match mgr.GetSessions() {
            Ok(v) => (0..v.Size().unwrap_or(0)).filter_map(|i| v.GetAt(i).ok()).collect(),
            Err(_) => Vec::new(),
        };
        let current = mgr.GetCurrentSession().ok();
        let same = subs.len() == sessions.len() && subs.iter().zip(&sessions).all(|(a, b)| a.session == *b);
        if !same {
            subs = sessions.iter().filter_map(|s| Subscription::new(s.clone(), tx).ok()).collect();
        }
        // 选中的会话没了就不再选；系统的当前会话换成了另一个正在播放的（新开始播的），
        // 让它占主位，和苹果「最新的活动在主位」一致
        if selected.as_ref().is_some_and(|s| !sessions.contains(s)) {
            selected = None;
        }
        if current_changed
            && let Some(cur) = &current
            && selected.as_ref().is_some_and(|s| s != cur)
            && is_playing(cur)
        {
            selected = None;
        }
        primary = selected.clone().or(current).or_else(|| sessions.first().cloned());
        other = sessions.iter().find(|s| Some(*s) != primary.as_ref() && is_playing(s)).cloned();

        let mut changed = update_other(other.as_ref(), &wic, info, &mut other_key);
        changed |= match &primary {
            Some(session) => poll(session, &wic, &c, &mut last_key).unwrap_or(false),
            None => {
                let mut g = info.lock().unwrap();
                if g.active {
                    *g = MediaInfo { track_id: g.track_id, version: g.version + 1, accent: DEFAULT_ACCENT, ..Default::default() };
                    last_key.clear();
                    true
                } else {
                    false
                }
            }
        };
        if changed {
            (c.wake)();
        }
    }
}

/// 更新「另一个播放器」：换了播放器或歌才重新读封面。返回是否有变化。
fn update_other(s: Option<&Session>, wic: &IWICImagingFactory, info: &Mutex<MediaInfo>, key: &mut String) -> bool {
    let Some(s) = s else {
        key.clear();
        let mut g = info.lock().unwrap();
        if g.other.take().is_some() {
            g.version += 1;
            return true;
        }
        return false;
    };
    let props = s.TryGetMediaPropertiesAsync().and_then(|op| op.join()).ok();
    let title = props.as_ref().and_then(|p| p.Title().ok()).map(|t| t.to_string_lossy()).unwrap_or_default();
    let id = app_id(s);
    let k = format!("{id}\u{1}{title}");
    if k == *key {
        return false;
    }
    *key = k;
    let art = props.and_then(|p| load_art(&p, wic).ok()).map(Arc::new);
    let mut g = info.lock().unwrap();
    let art_id = g.other.as_ref().map_or(0, |o| o.art_id) + 1;
    g.other = Some(Other { app: app_name(&id), title, art, art_id });
    g.version += 1;
    true
}

fn poll(session: &Session, wic: &IWICImagingFactory, c: &Ctx, last_key: &mut String) -> Result<bool> {
    let info = c.info;
    let props = session.TryGetMediaPropertiesAsync()?.join()?;
    let title = props.Title()?.to_string_lossy();
    let artist = props.Artist()?.to_string_lossy();
    let pb = session.GetPlaybackInfo()?;
    let playing = pb.PlaybackStatus()? == Status::Playing;
    let can_seek = pb.Controls().and_then(|c| c.IsPlaybackPositionEnabled()).unwrap_or(false);

    let tl = session.GetTimelineProperties()?;
    let start = tl.StartTime()?.Duration as f64 / 1e7;
    let end = tl.EndTime()?.Duration as f64 / 1e7;
    let mut position = tl.Position()?.Duration as f64 / 1e7 - start;
    let duration = (end - start).max(0.0);
    if playing {
        // 播放器只在状态变化时更新时间轴，按上次更新时间外推到现在
        let updated = tl.LastUpdatedTime()?.UniversalTime;
        let now = unsafe { GetSystemTimeAsFileTime() };
        let now = ((now.dwHighDateTime as i64) << 32) | now.dwLowDateTime as i64;
        let age = (now - updated) as f64 / 1e7;
        if (0.0..6.0 * 3600.0).contains(&age) {
            position += age;
        }
    }

    let app = session.SourceAppUserModelId()?.to_string_lossy();
    let key = format!("{app}\u{1}{title}\u{1}{artist}");
    let new_track = key != *last_key;

    let mut art = None;
    let mut accent = None;
    if new_track && !title.is_empty()
        && let Ok(px) = load_art(&props, wic) {
            accent = Some(accent_of(&px));
            art = Some(Arc::new(px));
        }

    let mut g = info.lock().unwrap();
    g.app = app_name(&app);
    let before = (g.active, g.playing, g.title.clone(), g.artist.clone(), (g.duration as i64), g.can_seek);
    g.start = start;
    g.can_seek = can_seek;
    g.active = !title.is_empty();
    g.title = title;
    g.artist = artist;
    g.playing = playing;
    g.duration = duration;
    let drift = (g.position_now() - position).abs();
    g.position = position;
    g.sampled_at = Some(Instant::now());
    if new_track {
        *last_key = key;
        g.track_id += 1;
        g.art = art;
        g.accent = accent.unwrap_or(DEFAULT_ACCENT);
        g.lyrics = None;
        request_lyrics(c, &g);
    }
    let after = (g.active, g.playing, g.title.clone(), g.artist.clone(), (g.duration as i64), g.can_seek);
    let changed = new_track || before != after || drift > 1.5;
    if changed {
        g.version += 1;
    }
    Ok(changed)
}

fn load_art(props: &windows::Media::Control::GlobalSystemMediaTransportControlsSessionMediaProperties, wic: &IWICImagingFactory) -> Result<Vec<u8>> {
    let stream = props.Thumbnail()?.OpenReadAsync()?.join()?;
    let size = stream.Size()? as u32;
    let reader = DataReader::CreateDataReader(&stream)?;
    reader.LoadAsync(size)?.join()?;
    let mut buf = vec![0u8; size as usize];
    reader.ReadBytes(&mut buf)?;

    unsafe {
        let ws = wic.CreateStream()?;
        ws.InitializeFromMemory(&buf)?;
        let dec = wic.CreateDecoderFromStream(&ws, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
        let frame = dec.GetFrame(0)?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h)?;
        // 居中裁成正方形
        let side = w.min(h);
        let clip = wic.CreateBitmapClipper()?;
        clip.Initialize(
            &frame,
            &WICRect { X: ((w - side) / 2) as i32, Y: ((h - side) / 2) as i32, Width: side as i32, Height: side as i32 },
        )?;
        let scaler = wic.CreateBitmapScaler()?;
        scaler.Initialize(&clip, ART, ART, WICBitmapInterpolationModeHighQualityCubic)?;
        let conv = wic.CreateFormatConverter()?;
        conv.Initialize(&scaler, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
        let mut px = vec![0u8; (ART * ART * 4) as usize];
        conv.CopyPixels(std::ptr::null(), ART * 4, &mut px)?;
        Ok(px)
    }
}

/// 取封面的平均色，提亮、提饱和，作为频谱条的颜色。
pub fn accent_of(bgra: &[u8]) -> [f32; 3] {
    let (mut r, mut g, mut b, mut n) = (0f64, 0f64, 0f64, 0f64);
    for p in bgra.as_chunks::<4>().0 {
        let (pb, pg, pr) = (p[0] as f64, p[1] as f64, p[2] as f64);
        let max = pr.max(pg).max(pb);
        let min = pr.min(pg).min(pb);
        // 彩色像素权重更高，避免被大片黑白背景带灰
        let w = 0.15 + (max - min) / 255.0;
        r += pr * w;
        g += pg * w;
        b += pb * w;
        n += w;
    }
    if n == 0.0 {
        return DEFAULT_ACCENT;
    }
    let (r, g, b) = ((r / n / 255.0) as f32, (g / n / 255.0) as f32, (b / n / 255.0) as f32);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    if max - min < 0.08 || max < 0.05 {
        return DEFAULT_ACCENT;
    }
    // 拉到最亮通道 = 1，再和白色混 25%
    let k = 1.0 / max;
    let mix = |c: f32| (c * k) * 0.75 + 0.25;
    [mix(r), mix(g), mix(b)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_names() {
        assert_eq!(app_name("Spotify.exe"), "Spotify");
        assert_eq!(app_name("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"), "媒体播放器");
        assert_eq!(app_name("MSEdge"), "Edge");
        assert_eq!(app_name("cloudmusic.exe"), "网易云音乐");
        assert_eq!(app_name(r"C:\Tools\Foo\Player.exe"), "Player");
        assert_eq!(app_name("Contoso.Tunes_abc123!App"), "App");
    }

    #[test]
    fn gray_art_uses_default_accent() {
        let px = [128u8, 128, 128, 255].repeat(16);
        assert_eq!(accent_of(&px), DEFAULT_ACCENT);
    }

    #[test]
    fn red_art_gives_reddish_accent() {
        let px = [20u8, 20, 200, 255].repeat(16); // BGRA
        let a = accent_of(&px);
        assert!(a[0] > 0.95 && a[1] < 0.5 && a[2] < 0.5, "{a:?}");
    }

    #[test]
    fn position_extrapolates_only_when_playing() {
        let mut m = MediaInfo { position: 10.0, duration: 100.0, sampled_at: Some(Instant::now() - Duration::from_secs(5)), ..Default::default() };
        assert_eq!(m.position_now(), 10.0);
        m.playing = true;
        assert!((m.position_now() - 15.0).abs() < 0.5);
        m.position = 99.0;
        assert_eq!(m.position_now(), 100.0);
    }
}
