//! 歌词：按 歌名+歌手+时长 在线查找同步歌词（LRC），本地缓存。
//!
//! 来源：网易云音乐（中文歌覆盖好）和 LRCLIB（开源，英文歌覆盖好）。
//! 只发送歌名和歌手去查询；结果缓存在 %APPDATA%\WinDynamicIsland\lyrics\，同一首歌只查一次。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Data::Json::{JsonArray, JsonObject};
use windows::Foundation::Uri;
use windows::Web::Http::HttpClient;
use windows::core::{HSTRING, Result, h};
use windows_future::AsyncStatus;

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub t: f64,
    pub text: String,
    /// 翻译（网易云有的话）。
    pub trans: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lyrics {
    pub lines: Vec<Line>,
}

impl Lyrics {
    /// 当前时间对应的行号。
    pub fn index_at(&self, t: f64) -> Option<usize> {
        let i = self.lines.partition_point(|l| l.t <= t);
        i.checked_sub(1)
    }
}

/// 解析 LRC 文本：支持一行多个时间标签、[offset:]、mm:ss / mm:ss.xx / mm:ss.xxx。
pub fn parse_lrc(text: &str) -> Vec<(f64, String)> {
    let text = text.replace("\\n", "\n");
    let mut offset = 0.0;
    let mut out = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            let tag = &stripped[..end];
            rest = stripped[end + 1..].trim_start();
            if let Some(v) = tag.strip_prefix("offset:") {
                offset = v.trim().parse::<f64>().unwrap_or(0.0) / 1000.0;
            } else if let Some(t) = parse_stamp(tag) {
                stamps.push(t);
            }
        }
        for t in stamps {
            out.push(((t - offset).max(0.0), rest.trim().to_string()));
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

fn parse_stamp(tag: &str) -> Option<f64> {
    let (m, s) = tag.split_once(':')?;
    let m: f64 = m.trim().parse().ok()?;
    let s: f64 = s.trim().replace(':', ".").parse().ok()?;
    Some(m * 60.0 + s)
}

/// "作词 : 周杰伦" 这类开头的署名行不当歌词显示。
fn is_credit(t: f64, text: &str) -> bool {
    if t > 60.0 {
        return false;
    }
    let Some(pos) = text.find([':', '：']) else { return false };
    text[..pos].trim().chars().count() <= 8
}

/// 合并原文和翻译，去掉署名行。
pub fn build(lrc: &str, tlrc: Option<&str>) -> Lyrics {
    let trans = tlrc.map(parse_lrc).unwrap_or_default();
    let lines = parse_lrc(lrc)
        .into_iter()
        .filter(|(t, s)| !is_credit(*t, s))
        .map(|(t, text)| {
            let tr = trans.iter().find(|(tt, s)| (tt - t).abs() < 0.08 && !s.is_empty()).map(|(_, s)| s.clone());
            Line { t, text, trans: tr }
        })
        .collect();
    Lyrics { lines }
}

// ---------- 在线查询 ----------

pub struct Query<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    /// 秒；0 表示未知。
    pub duration: f64,
}

/// 先查缓存，再按语言决定先问哪个源。查不到返回 None（也会缓存"查不到"）。
pub fn find(q: &Query) -> Option<Lyrics> {
    let cache = cache_path(q);
    if let Some(p) = &cache
        && let Ok(text) = std::fs::read_to_string(p)
    {
        return parse_cached(&text);
    }
    let cjk = q.title.chars().chain(q.artist.chars()).any(is_cjk);
    let (lrc, tlrc) = if cjk {
        netease(q).or_else(|| lrclib(q).map(|l| (l, None)))
    } else {
        lrclib(q).map(|l| (l, None)).or_else(|| netease(q))
    }
    .unwrap_or_default();
    let lyrics = (!lrc.is_empty()).then(|| build(&lrc, tlrc.as_deref())).filter(|l| !l.lines.is_empty());
    if let Some(p) = cache {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let body = match &lyrics {
            Some(_) => format!("{lrc}\n#TRANS\n{}", tlrc.unwrap_or_default()),
            None => "#NONE".to_string(),
        };
        let _ = std::fs::write(p, body);
    }
    lyrics
}

fn parse_cached(text: &str) -> Option<Lyrics> {
    if text.starts_with("#NONE") {
        return None;
    }
    let (lrc, tl) = text.split_once("\n#TRANS\n").unwrap_or((text, ""));
    Some(build(lrc, (!tl.trim().is_empty()).then_some(tl))).filter(|l| !l.lines.is_empty())
}

fn cache_path(q: &Query) -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    // FNV-1a，够用来做文件名
    let mut h: u64 = 0xcbf29ce484222325;
    for b in format!("{}\u{1}{}", q.artist.to_lowercase(), q.title.to_lowercase()).bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    Some(PathBuf::from(base).join("WinDynamicIsland").join("lyrics").join(format!("{h:016x}.lrc")))
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xac00..=0xd7af)
}

pub fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn get(url: &str, referer: Option<&str>) -> Result<String> {
    let client = HttpClient::new()?;
    let headers = client.DefaultRequestHeaders()?;
    let _ = headers.UserAgent()?.TryParseAdd(h!("Mozilla/5.0 (Windows NT 10.0; Win64; x64) WinDynamicIsland/0.1"));
    if let Some(r) = referer {
        headers.SetReferer(&Uri::CreateUri(&HSTRING::from(r))?)?;
    }
    let op = client.GetStringAsync(&Uri::CreateUri(&HSTRING::from(url))?)?;
    // WinRT 的 HttpClient 没有超时设置，自己等 8 秒
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match op.Status()? {
            AsyncStatus::Started if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            AsyncStatus::Completed => return Ok(op.GetResults()?.to_string_lossy()),
            _ => {
                let _ = op.Cancel();
                return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL));
            }
        }
    }
}

/// 给候选结果打分：歌手对得上、时长接近的优先。
fn score(cand_title: &str, cand_artists: &str, cand_dur: f64, q: &Query) -> f64 {
    let norm = |s: &str| s.to_lowercase().replace(' ', "");
    let (ct, ca, qt, qa) = (norm(cand_title), norm(cand_artists), norm(q.title), norm(q.artist));
    let mut s = 0.0;
    if ct == qt {
        s += 3.0;
    } else if ct.contains(&qt) || qt.contains(&ct) {
        s += 1.0;
    }
    if !qa.is_empty() && qa.split(['/', ',', '、', '&']).any(|a| !a.is_empty() && ca.contains(a)) {
        s += 4.0;
    }
    if q.duration > 0.0 && cand_dur > 0.0 {
        let d = (cand_dur - q.duration).abs();
        s += if d < 3.0 { 3.0 } else if d < 10.0 { 1.0 } else { -2.0 };
    }
    // 翻唱、伴奏版往后排
    if ["翻唱", "伴奏", "女声版", "男声版", "cover", "instrumental", "dj"].iter().any(|k| ct.contains(k) && !qt.contains(k)) {
        s -= 3.0;
    }
    s
}

fn netease(q: &Query) -> Option<(String, Option<String>)> {
    const REF: &str = "https://music.163.com/";
    let url = format!("https://music.163.com/api/cloudsearch/pc?s={}&type=1&limit=10", encode(&format!("{} {}", q.title, q.artist)));
    let json = JsonObject::Parse(&HSTRING::from(get(&url, Some(REF)).ok()?)).ok()?;
    let songs: JsonArray = json.GetNamedObject(h!("result")).ok()?.GetNamedArray(h!("songs")).ok()?;
    let mut best: Option<(f64, f64)> = None;
    for i in 0..songs.Size().ok()? {
        let Ok(s) = songs.GetObjectAt(i) else { continue };
        let id = s.GetNamedNumber(h!("id")).unwrap_or(0.0);
        let name = s.GetNamedString(h!("name")).map(|h| h.to_string_lossy()).unwrap_or_default();
        let dur = s.GetNamedNumber(h!("dt")).unwrap_or(0.0) / 1000.0;
        let mut artists = String::new();
        if let Ok(ar) = s.GetNamedArray(h!("ar")) {
            for j in 0..ar.Size().unwrap_or(0) {
                if let Ok(a) = ar.GetObjectAt(j) {
                    artists.push_str(&a.GetNamedString(h!("name")).map(|h| h.to_string_lossy()).unwrap_or_default());
                    artists.push('/');
                }
            }
        }
        let sc = score(&name, &artists, dur, q);
        if best.is_none_or(|b| sc > b.0) {
            best = Some((sc, id));
        }
    }
    let (sc, id) = best?;
    if sc < 3.0 {
        return None;
    }
    let url = format!("https://music.163.com/api/song/lyric?id={id}&lv=1&tv=-1");
    let json = JsonObject::Parse(&HSTRING::from(get(&url, Some(REF)).ok()?)).ok()?;
    let lrc = json.GetNamedObject(h!("lrc")).ok()?.GetNamedString(h!("lyric")).ok()?.to_string_lossy();
    let tl = json
        .GetNamedObject(h!("tlyric"))
        .ok()
        .and_then(|t| t.GetNamedString(h!("lyric")).ok())
        .map(|h| h.to_string_lossy())
        .filter(|s| !s.trim().is_empty());
    // 纯音乐、没有时间轴的不要
    (lrc.contains('[') && !lrc.contains("纯音乐，请欣赏")).then_some((lrc, tl))
}

fn lrclib(q: &Query) -> Option<String> {
    let url = format!("https://lrclib.net/api/search?track_name={}&artist_name={}", encode(q.title), encode(q.artist));
    let arr = JsonArray::Parse(&HSTRING::from(get(&url, None).ok()?)).ok()?;
    let mut best: Option<(f64, String)> = None;
    for i in 0..arr.Size().ok()? {
        let Ok(s) = arr.GetObjectAt(i) else { continue };
        let Ok(synced) = s.GetNamedString(h!("syncedLyrics")) else { continue };
        let name = s.GetNamedString(h!("trackName")).map(|h| h.to_string_lossy()).unwrap_or_default();
        let artist = s.GetNamedString(h!("artistName")).map(|h| h.to_string_lossy()).unwrap_or_default();
        let dur = s.GetNamedNumber(h!("duration")).unwrap_or(0.0);
        let sc = score(&name, &artist, dur, q);
        if best.as_ref().is_none_or(|b| sc > b.0) {
            best = Some((sc, synced.to_string_lossy()));
        }
    }
    best.filter(|b| b.0 >= 3.0).map(|b| b.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_lrc_forms() {
        let l = parse_lrc("[ti:x]\n[00:01.50]a\n[00:03]b\n[01:02.123][00:02.00]c\n[offset:500]\n");
        assert_eq!(l.iter().map(|x| x.1.as_str()).collect::<Vec<_>>(), ["a", "c", "b", "c"]);
        assert!((l[0].0 - 1.5).abs() < 1e-9);
        assert!((l[3].0 - 62.123).abs() < 1e-9);
    }

    #[test]
    fn handles_escaped_newlines() {
        let l = parse_lrc("[00:06.000] 贝斯 : 陈任佑\\n[00:07.000] 鼓 : 陈柏州");
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn drops_credits_and_merges_translation() {
        let ly = build("[00:00.00]作词 : 某某\n[00:28.00]路灯一盏一盏亮起来\n[00:31.00]Hello", Some("[00:31.00]你好"));
        assert_eq!(ly.lines.len(), 2);
        assert_eq!(ly.lines[0].text, "路灯一盏一盏亮起来");
        assert_eq!(ly.lines[1].trans.as_deref(), Some("你好"));
    }

    #[test]
    fn index_lookup() {
        let ly = build("[00:10.00]a\n[00:20.00]b", None);
        assert_eq!(ly.index_at(5.0), None);
        assert_eq!(ly.index_at(10.0), Some(0));
        assert_eq!(ly.index_at(25.0), Some(1));
    }

    #[test]
    fn scoring_prefers_original_artist_and_duration() {
        let q = Query { title: "晴天", artist: "周杰伦", duration: 269.0 };
        let original = score("晴天", "周杰伦/", 269.5, &q);
        let cover = score("晴天 (原唱 周杰伦)", "RyaVocal/", 270.7, &q);
        let female = score("晴天 (女声版)", "GYBeat/", 268.7, &q);
        assert!(original > cover && original > female);
    }

    #[test]
    fn url_encoding() {
        assert_eq!(encode("a b&晴"), "a%20b%26%E6%99%B4");
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// 联网测试，默认跳过：cargo test -- --ignored live
    #[test]
    #[ignore]
    fn live_lookup() {
        for (title, artist, d) in [("晴天", "周杰伦", 269.0), ("Yellow", "Coldplay", 266.0)] {
            let l = find(&Query { title, artist, duration: d });
            let n = l.as_ref().map(|l| l.lines.len()).unwrap_or(0);
            println!("{title} - {artist}: {n} 行; 第 10 行 = {:?}", l.as_ref().and_then(|l| l.lines.get(10)).map(|x| (&x.text, &x.trans)));
            assert!(n > 10);
        }
    }
}
