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
    /// 每换一首歌 +1。
    pub track_id: u64,
    pub art: Option<Arc<Vec<u8>>>,
    /// 从封面取的强调色（RGB 0..1）。
    pub accent: [f32; 3],
    /// 每次有变化 +1，UI 线程据此判断要不要重新拷贝。
    pub version: u64,
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
    /// 内部用：系统事件通知需要刷新。
    Refresh,
}

pub struct Media {
    pub info: Arc<Mutex<MediaInfo>>,
    tx: Sender<Cmd>,
}

impl Media {
    pub fn start(wake: impl Fn() + Send + 'static) -> Media {
        let info = Arc::new(Mutex::new(MediaInfo { accent: DEFAULT_ACCENT, ..Default::default() }));
        let (tx, rx) = mpsc::channel();
        let shared = info.clone();
        let event_tx = tx.clone();
        std::thread::Builder::new()
            .name("media".into())
            .spawn(move || {
                if let Err(e) = run(&shared, &rx, &event_tx, &wake) {
                    eprintln!("[media] 线程退出: {e:?}");
                }
            })
            .expect("spawn media thread");
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

/// 生成一个只负责"通知媒体线程去刷新"的 WinRT 事件回调。
fn notifier<S: RuntimeType + 'static, A: RuntimeType + 'static>(tx: &Sender<Cmd>) -> TypedEventHandler<S, A> {
    let tx = tx.clone();
    TypedEventHandler::new(move |_, _| {
        let _ = tx.send(Cmd::Refresh);
        Ok(())
    })
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

fn run(info: &Mutex<MediaInfo>, rx: &Receiver<Cmd>, tx: &Sender<Cmd>, wake: &dyn Fn()) -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let mgr = Manager::RequestAsync()?.join()?;
    let wic: IWICImagingFactory = unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)? };
    let mut last_key = String::new();

    // 事件驱动：会话切换、曲目/播放状态变化时系统通知我们；
    // 另留一个低频兜底轮询，用来校正进度（拖动进度条不一定触发事件）。
    let session_token = mgr.CurrentSessionChanged(&notifier(tx))?;
    let mut sub: Option<Subscription> = None;
    let mut resubscribe = true;

    loop {
        let playing = info.lock().map(|g| g.playing).unwrap_or(false);
        let fallback = Duration::from_secs(if playing { 3 } else { 10 });
        match rx.recv_timeout(fallback) {
            Ok(Cmd::Refresh) => {
                // 一次变化常常连发好几个事件，攒一下再查
                std::thread::sleep(Duration::from_millis(60));
                while rx.try_recv().is_ok() {}
                resubscribe = true;
            }
            Ok(cmd) => {
                if let Ok(s) = mgr.GetCurrentSession() {
                    let r = match cmd {
                        Cmd::Toggle => s.TryTogglePlayPauseAsync(),
                        Cmd::Next => s.TrySkipNextAsync(),
                        Cmd::Prev => s.TrySkipPreviousAsync(),
                        Cmd::Refresh => unreachable!(),
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
                let _ = mgr.RemoveCurrentSessionChanged(session_token);
                return Ok(());
            }
        }

        let current = mgr.GetCurrentSession().ok();
        if resubscribe {
            resubscribe = false;
            let same = matches!((&sub, &current), (Some(s), Some(c)) if s.session == *c);
            if !same {
                sub = current.clone().and_then(|c| Subscription::new(c, tx).ok());
            }
        }

        let changed = match current {
            Some(session) => poll(&session, &wic, info, &mut last_key).unwrap_or(false),
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
            wake();
        }
    }
}

fn poll(session: &Session, wic: &IWICImagingFactory, info: &Mutex<MediaInfo>, last_key: &mut String) -> Result<bool> {
    let props = session.TryGetMediaPropertiesAsync()?.join()?;
    let title = props.Title()?.to_string_lossy();
    let artist = props.Artist()?.to_string_lossy();
    let playing = session.GetPlaybackInfo()?.PlaybackStatus()? == Status::Playing;

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
    let before = (g.active, g.playing, g.title.clone(), g.artist.clone(), (g.duration as i64));
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
    }
    let after = (g.active, g.playing, g.title.clone(), g.artist.clone(), (g.duration as i64));
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
