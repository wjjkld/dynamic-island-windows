//! 设置：%APPDATA%\WinDynamicIsland\settings.ini，简单的 key=value。
//! 开机自启不存在这里，直接读写注册表 HKCU\...\Run，避免两边不一致。

use std::path::PathBuf;

use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::core::w;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// 联网查歌词并显示。
    pub lyrics: bool,
    /// 调音量、亮度时在岛上显示。
    pub level_osd: bool,
    /// 有全屏应用时隐藏。
    pub hide_in_fullscreen: bool,
    /// 空闲时收成贴顶细条，少挡浏览器标签页。
    pub slim_idle: bool,
    /// 仪表盘显示天气（会把大概位置发给 Open-Meteo）。
    pub weather: bool,
    /// 系统定位用不了时按这个城市查天气；空 = 没设。
    pub weather_city: String,
    /// 显示在哪个显示器（设备名，如 \\.\DISPLAY2）；空 = 主显示器。
    pub monitor: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { lyrics: true, level_osd: true, hide_in_fullscreen: true, slim_idle: false, weather: true, weather_city: String::new(), monitor: String::new() }
    }
}

impl Settings {
    pub fn load() -> Settings {
        let text = path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        Settings::parse(&text)
    }

    pub fn parse(text: &str) -> Settings {
        let mut s = Settings::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            let b = v == "1" || v.eq_ignore_ascii_case("true");
            match k {
                "lyrics" => s.lyrics = b,
                "level_osd" => s.level_osd = b,
                "hide_in_fullscreen" => s.hide_in_fullscreen = b,
                "slim_idle" => s.slim_idle = b,
                "weather" => s.weather = b,
                "weather_city" => s.weather_city = v.to_string(),
                "monitor" => s.monitor = v.to_string(),
                _ => {}
            }
        }
        s
    }

    pub fn serialize(&self) -> String {
        let b = |v: bool| if v { 1 } else { 0 };
        format!(
            "lyrics={}\nlevel_osd={}\nhide_in_fullscreen={}\nslim_idle={}\nweather={}\nweather_city={}\nmonitor={}\n",
            b(self.lyrics),
            b(self.level_osd),
            b(self.hide_in_fullscreen),
            b(self.slim_idle),
            b(self.weather),
            self.weather_city,
            self.monitor
        )
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, self.serialize());
    }
}

fn path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("WinDynamicIsland").join("settings.ini"))
}

const RUN_KEY: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_NAME: windows::core::PCWSTR = w!("WinDynamicIsland");

pub fn autostart_enabled() -> bool {
    let mut size = 0u32;
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_NAME, RRF_RT_REG_SZ, None, None, Some(&mut size)).is_ok() }
}

pub fn set_autostart(on: bool) {
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            let value: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain([0]).collect();
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_NAME,
                REG_SZ.0,
                Some(value.as_ptr() as _),
                (value.len() * 2) as u32,
            );
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_NAME);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let s = Settings {
            lyrics: false,
            level_osd: true,
            hide_in_fullscreen: false,
            slim_idle: true,
            weather: false,
            weather_city: "上海".into(),
            monitor: "\\\\.\\DISPLAY2".into(),
        };
        assert_eq!(Settings::parse(&s.serialize()), s);
    }

    #[test]
    fn missing_keys_use_defaults() {
        assert_eq!(Settings::parse("garbage\nlyrics=0\n"), Settings { lyrics: false, ..Default::default() });
    }
}
