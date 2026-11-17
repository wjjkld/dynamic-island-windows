//! 天气：Windows 定位拿到大概位置，查当前天气和今天的最高 / 最低温。
//!
//! 先问 wttr.in（国内访问快），不通再问 Open-Meteo；两个都免费、不用密钥。
//! 只发送保留一位小数的经纬度（约 10 公里精度），不发送其它信息。每 30 分钟更新一次。
//! 系统定位没开（设置 → 隐私和安全性 → 位置 → 允许桌面应用访问你的位置）时不查，仪表盘上给出提示。

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::Data::Json::JsonObject;
use windows::Devices::Geolocation::{GeolocationAccessStatus, Geolocator, PositionAccuracy};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::core::{HSTRING, h};

use crate::lyrics;

#[derive(Clone, Debug, PartialEq)]
pub struct Now {
    /// 当前气温（℃）
    pub temp: f32,
    pub hi: f32,
    pub lo: f32,
    pub desc: &'static str,
    pub icon: Icon,
    pub is_day: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum State {
    /// 还没查到（刚启动、网络不通、关掉了）
    #[default]
    Unknown,
    /// 系统定位关着或不让桌面应用用
    NoLocation,
    Ready(Now),
}

type Shared = Arc<Mutex<(u64, State)>>;

pub struct Weather {
    shared: Shared,
    seen: u64,
    tx: Sender<bool>,
}

impl Weather {
    pub fn start(on: bool, wake: impl Fn() + Send + 'static) -> Weather {
        let shared: Shared = Arc::default();
        let (tx, rx) = channel();
        let s = shared.clone();
        let _ = std::thread::Builder::new().name("weather".into()).spawn(move || run(on, &s, &rx, &wake));
        Weather { shared, seen: 0, tx }
    }

    /// 打开 / 关闭天气；关闭后不再联网，状态回到 Unknown。
    pub fn set_enabled(&self, on: bool) {
        let _ = self.tx.send(on);
    }

    /// 有变化时返回最新状态。
    pub fn poll(&mut self) -> Option<State> {
        let g = self.shared.lock().ok()?;
        if g.0 == self.seen {
            return None;
        }
        self.seen = g.0;
        Some(g.1.clone())
    }
}

fn run(mut on: bool, shared: &Shared, rx: &Receiver<bool>, wake: &dyn Fn()) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let publish = |st: State| {
        if let Ok(mut g) = shared.lock()
            && g.1 != st
        {
            g.0 += 1;
            g.1 = st;
            drop(g);
            wake();
        }
    };
    loop {
        let wait = if on {
            let st = fetch();
            // 网络失败时保留上次的结果，过一会儿再试
            let retry = st == State::Unknown;
            if !retry {
                publish(st);
            }
            Duration::from_secs(if retry { 120 } else { 30 * 60 })
        } else {
            publish(State::Unknown);
            Duration::from_secs(24 * 3600)
        };
        match rx.recv_timeout(wait) {
            Ok(v) => on = v,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn fetch() -> State {
    let Some((lat, lon)) = locate() else { return State::NoLocation };
    let wttr = format!("https://wttr.in/{lat:.1},{lon:.1}?format=j1");
    let meteo = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat:.1}&longitude={lon:.1}\
         &current=temperature_2m,weather_code,is_day&daily=temperature_2m_max,temperature_2m_min\
         &timezone=auto&forecast_days=1"
    );
    let now = lyrics::get(&wttr, None)
        .ok()
        .and_then(|t| parse_wttr(&t, local_minutes()))
        .or_else(|| lyrics::get(&meteo, None).ok().and_then(|t| parse_open_meteo(&t)));
    now.map(State::Ready).unwrap_or_default()
}

/// 大概位置；没有权限或定位失败返回 None。
fn locate() -> Option<(f64, f64)> {
    if Geolocator::RequestAccessAsync().ok()?.join().ok()? != GeolocationAccessStatus::Allowed {
        return None;
    }
    let g = Geolocator::new().ok()?;
    let _ = g.SetDesiredAccuracy(PositionAccuracy::Default);
    let pos = g.GetGeopositionAsync().ok()?.join().ok()?;
    let p = pos.Coordinate().ok()?.Point().ok()?.Position().ok()?;
    Some((round1(p.Latitude), round1(p.Longitude)))
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn local_minutes() -> u32 {
    let t = unsafe { GetLocalTime() };
    t.wHour as u32 * 60 + t.wMinute as u32
}

/// wttr.in 的 j1 格式。数字都是字符串。
fn parse_wttr(text: &str, now_min: u32) -> Option<Now> {
    let j = JsonObject::Parse(&HSTRING::from(text)).ok()?;
    let num = |o: &JsonObject, k: &HSTRING| o.GetNamedString(k).ok()?.to_string_lossy().trim().parse::<f32>().ok();
    let cur = j.GetNamedArray(h!("current_condition")).ok()?.GetObjectAt(0).ok()?;
    let day = j.GetNamedArray(h!("weather")).ok()?.GetObjectAt(0).ok()?;
    let (desc, icon) = wwo(num(&cur, h!("weatherCode"))? as u32);
    let astro = day.GetNamedArray(h!("astronomy")).ok().and_then(|a| a.GetObjectAt(0).ok());
    let clock = |k: &HSTRING| astro.as_ref().and_then(|a| a.GetNamedString(k).ok()).and_then(|s| minutes_12h(&s.to_string_lossy()));
    let is_day = match (clock(h!("sunrise")), clock(h!("sunset"))) {
        (Some(rise), Some(set)) => (rise..set).contains(&now_min),
        _ => (6 * 60..18 * 60).contains(&now_min),
    };
    Some(Now { temp: num(&cur, h!("temp_C"))?, hi: num(&day, h!("maxtempC"))?, lo: num(&day, h!("mintempC"))?, desc, icon, is_day })
}

/// "07:02 AM" → 422
fn minutes_12h(s: &str) -> Option<u32> {
    let (hm, ampm) = s.trim().split_once(' ')?;
    let (h, m) = hm.split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    let h = match ampm {
        "AM" => h % 12,
        "PM" => h % 12 + 12,
        _ => return None,
    };
    Some(h * 60 + m)
}

fn parse_open_meteo(text: &str) -> Option<Now> {
    let j = JsonObject::Parse(&HSTRING::from(text)).ok()?;
    let cur = j.GetNamedObject(h!("current")).ok()?;
    let daily = j.GetNamedObject(h!("daily")).ok()?;
    let first = |k: &HSTRING| daily.GetNamedArray(k).ok()?.GetNumberAt(0).ok();
    let (desc, icon) = wmo(cur.GetNamedNumber(h!("weather_code")).ok()? as u32);
    Some(Now {
        temp: cur.GetNamedNumber(h!("temperature_2m")).ok()? as f32,
        hi: first(h!("temperature_2m_max"))? as f32,
        lo: first(h!("temperature_2m_min"))? as f32,
        desc,
        icon,
        is_day: cur.GetNamedNumber(h!("is_day")).unwrap_or(1.0) != 0.0,
    })
}

/// 天气图标的种类。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Clear,
    PartlyCloudy,
    Cloudy,
    Fog,
    Rain,
    Snow,
    Thunder,
}

/// WMO 天气代码（Open-Meteo）→ (中文描述, 图标)。
fn wmo(code: u32) -> (&'static str, Icon) {
    match code {
        0 => ("晴", Icon::Clear),
        1 => ("晴间多云", Icon::PartlyCloudy),
        2 => ("多云", Icon::PartlyCloudy),
        3 => ("阴", Icon::Cloudy),
        45 | 48 => ("雾", Icon::Fog),
        51..=57 => ("毛毛雨", Icon::Rain),
        61 | 80 => ("小雨", Icon::Rain),
        63 | 81 => ("中雨", Icon::Rain),
        65 | 82 => ("大雨", Icon::Rain),
        66 | 67 => ("冻雨", Icon::Rain),
        71 | 85 => ("小雪", Icon::Snow),
        73 => ("中雪", Icon::Snow),
        75 | 86 => ("大雪", Icon::Snow),
        77 => ("雪粒", Icon::Snow),
        95 => ("雷阵雨", Icon::Thunder),
        96 | 99 => ("雷阵雨伴冰雹", Icon::Thunder),
        _ => ("多云", Icon::Cloudy),
    }
}

/// WWO 天气代码（wttr.in）→ (中文描述, 图标)。
fn wwo(code: u32) -> (&'static str, Icon) {
    match code {
        113 => ("晴", Icon::Clear),
        116 => ("多云", Icon::PartlyCloudy),
        119 => ("多云", Icon::Cloudy),
        122 => ("阴", Icon::Cloudy),
        143 => ("薄雾", Icon::Fog),
        248 | 260 => ("雾", Icon::Fog),
        176 | 263 | 266 | 293 | 296 => ("小雨", Icon::Rain),
        299 | 302 => ("中雨", Icon::Rain),
        305 | 308 => ("大雨", Icon::Rain),
        353 => ("阵雨", Icon::Rain),
        356 | 359 => ("强阵雨", Icon::Rain),
        185 | 281 | 284 | 311 | 314 => ("冻雨", Icon::Rain),
        179 | 323 | 326 => ("小雪", Icon::Snow),
        329 | 332 => ("中雪", Icon::Snow),
        335 | 338 => ("大雪", Icon::Snow),
        368 | 371 => ("阵雪", Icon::Snow),
        227 | 230 => ("暴风雪", Icon::Snow),
        182 | 317 | 320 | 362 | 365 => ("雨夹雪", Icon::Snow),
        350 | 374 | 377 => ("冰粒", Icon::Snow),
        200 | 386 | 389 => ("雷阵雨", Icon::Thunder),
        392 | 395 => ("雷阵雪", Icon::Thunder),
        _ => ("多云", Icon::Cloudy),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wttr() {
        let t = r#"{"current_condition":[{"temp_C":"5","weatherCode":"116"}],
                   "weather":[{"maxtempC":"6","mintempC":"1","astronomy":[{"sunrise":"07:02 AM","sunset":"04:56 PM"}]}]}"#;
        let n = parse_wttr(t, 14 * 60).unwrap();
        assert_eq!((n.desc, n.icon, n.is_day), ("多云", Icon::PartlyCloudy, true));
        assert_eq!((n.temp, n.hi, n.lo), (5.0, 6.0, 1.0));
        assert!(!parse_wttr(t, 17 * 60).unwrap().is_day);
        assert!(parse_wttr("{}", 0).is_none());
    }

    #[test]
    fn parses_open_meteo() {
        let t = r#"{"latitude":31.2,"current":{"time":"2026-11-18T14:00","temperature_2m":17.4,"weather_code":2,"is_day":0},
                   "daily":{"time":["2026-11-18"],"temperature_2m_max":[19.8],"temperature_2m_min":[11.2]}}"#;
        let n = parse_open_meteo(t).unwrap();
        assert_eq!((n.desc, n.is_day), ("多云", false));
        assert!((n.temp - 17.4).abs() < 0.01 && (n.hi - 19.8).abs() < 0.01 && (n.lo - 11.2).abs() < 0.01);
        assert!(parse_open_meteo("{}").is_none());
    }

    #[test]
    fn codes_and_clock() {
        assert_eq!(wmo(0), ("晴", Icon::Clear));
        assert_eq!(wmo(99).1, Icon::Thunder);
        assert_eq!(wwo(296).1, Icon::Rain);
        assert_eq!(wwo(338), ("大雪", Icon::Snow));
        assert_eq!(minutes_12h("07:02 AM"), Some(422));
        assert_eq!(minutes_12h("12:30 PM"), Some(750));
        assert_eq!(minutes_12h("12:10 AM"), Some(10));
        assert_eq!(minutes_12h("No moonset"), None);
        assert_eq!(round1(31.2345), 31.2);
    }

    /// 真实查询一次，默认跳过：cargo test -- --ignored live_weather --nocapture
    #[test]
    #[ignore]
    fn live_weather() {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap() };
        println!("{:?}", fetch());
        // 定位没开时用固定的公开坐标（北京）检查两个接口
        let t = lyrics::get("https://wttr.in/39.9,116.4?format=j1", None).unwrap();
        println!("wttr: {:?}", parse_wttr(&t, local_minutes()));
    }
}
