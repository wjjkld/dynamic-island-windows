//! 麦克风 / 摄像头占用检测。
//!
//! 以前 Windows 把使用记录写在注册表 CapabilityAccessManager\ConsentStore 里，新版 Windows 11
//! 改存到普通程序读不了的数据库，注册表不再更新，所以改用两个公开的实时接口：
//! - 摄像头：Media Foundation 的传感器活动监视器，哪个进程开始 / 停止取流都会回调；
//! - 麦克风：枚举各录音设备上的音频会话，处于活动状态的就是正在用麦克风的进程（每秒查一次）。

use std::collections::HashMap;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, S_OK};
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
    MMDeviceEnumerator, eCapture,
};
use windows::Win32::Media::MediaFoundation::{
    IMFSensorActivitiesReport, IMFSensorActivitiesReportCallback, IMFSensorActivitiesReportCallback_Impl,
    IMFSensorActivityMonitor, MF_API_VERSION, MF_SDK_VERSION, MFCreateSensorActivityMonitor, MFStartup,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{Interface, PWSTR, Ref, Result};
use windows_core::implement;

/// 正在使用的应用名（去重、排序）。
#[derive(Clone, Default, PartialEq, Debug)]
pub struct InUse {
    pub mic: Vec<String>,
    pub cam: Vec<String>,
}

impl InUse {
    pub fn any(&self) -> bool {
        !self.mic.is_empty() || !self.cam.is_empty()
    }
}

type Shared = Arc<Mutex<(u64, InUse)>>;

pub struct Privacy {
    shared: Shared,
    seen: u64,
}

impl Privacy {
    pub fn start(wake: impl Fn() + Send + 'static) -> Privacy {
        let shared: Shared = Arc::default();
        let s = shared.clone();
        let _ = std::thread::Builder::new().name("privacy".into()).spawn(move || {
            if let Err(e) = run(&s, &wake) {
                eprintln!("[privacy] 停止: {e:?}");
            }
        });
        Privacy { shared, seen: 0 }
    }

    /// 有变化时返回最新状态。
    pub fn poll(&mut self) -> Option<InUse> {
        let g = self.shared.lock().ok()?;
        if g.0 == self.seen {
            return None;
        }
        self.seen = g.0;
        Some(g.1.clone())
    }
}

/// 摄像头符号链接 → 正在取流的进程。
type Cams = Arc<Mutex<HashMap<String, Vec<u32>>>>;

#[implement(IMFSensorActivitiesReportCallback)]
struct CamCb {
    cams: Cams,
    tx: Sender<()>,
}

impl IMFSensorActivitiesReportCallback_Impl for CamCb_Impl {
    fn OnActivitiesReport(&self, report: Ref<IMFSensorActivitiesReport>) -> Result<()> {
        let report = report.ok()?;
        let mut cams = self.cams.lock().unwrap();
        unsafe {
            for i in 0..report.GetCount()? {
                let dev = report.GetActivityReport(i)?;
                let mut buf = [0u16; 512];
                let mut n = 0u32;
                dev.GetSymbolicLink(&mut buf, &mut n)?;
                let link = String::from_utf16_lossy(&buf[..(n as usize).min(buf.len())]);
                let mut pids = Vec::new();
                for j in 0..dev.GetProcessCount()? {
                    let p = dev.GetProcessActivity(j)?;
                    if p.GetStreamingState()?.as_bool() {
                        pids.push(p.GetProcessId()?);
                    }
                }
                cams.insert(link, pids);
            }
        }
        let _ = self.tx.send(());
        Ok(())
    }
}

fn run(shared: &Shared, wake: &dyn Fn()) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        MFStartup((MF_SDK_VERSION << 16) | MF_API_VERSION, 0)?;
    }
    let cams: Cams = Arc::default();
    let (tx, rx) = channel();
    let cb: IMFSensorActivitiesReportCallback = CamCb { cams: cams.clone(), tx }.into();
    // 没有摄像头或接口不可用时只查麦克风
    let monitor: Option<IMFSensorActivityMonitor> = unsafe {
        MFCreateSensorActivityMonitor(&cb).ok().filter(|m| m.Start().is_ok())
    };
    if monitor.is_none() {
        eprintln!("[privacy] 摄像头活动监视器不可用，只检测麦克风");
    }
    let devices: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let me = unsafe { GetCurrentProcessId() };

    let mut last = InUse::default();
    let mut managers = Vec::new();
    let mut tick = 0u64;
    loop {
        // 录音设备不常变，每 10 秒重新取一次会话管理器，其余时间复用
        tick += 1;
        if tick % 10 == 1 {
            managers = session_managers(&devices);
        }
        let cam_pids: Vec<u32> = cams.lock().unwrap().values().flatten().copied().collect();
        let mut now = InUse { mic: mic_names(&managers, me), cam: cam_pids.into_iter().filter_map(process_name).collect() };
        for list in [&mut now.mic, &mut now.cam] {
            list.sort();
            list.dedup();
        }
        if now != last {
            last = now.clone();
            if let Ok(mut g) = shared.lock() {
                g.0 += 1;
                g.1 = now;
            }
            wake();
        }
        // 摄像头有事件立即醒；麦克风每秒查一次
        let _ = rx.recv_timeout(Duration::from_secs(1));
        let _ = &monitor;
    }
}

/// 每个启用的录音设备的会话管理器。
fn session_managers(devices: &IMMDeviceEnumerator) -> Vec<IAudioSessionManager2> {
    let mut out = Vec::new();
    unsafe {
        let Ok(list) = devices.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else { return out };
        for i in 0..list.GetCount().unwrap_or(0) {
            if let Ok(mgr) = list.Item(i).and_then(|d| d.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None)) {
                out.push(mgr);
            }
        }
    }
    out
}

/// 正在录音的进程名。
fn mic_names(managers: &[IAudioSessionManager2], me: u32) -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        for mgr in managers {
            let Ok(sessions) = mgr.GetSessionEnumerator() else { continue };
            for j in 0..sessions.GetCount().unwrap_or(0) {
                let Ok(s) = sessions.GetSession(j).and_then(|s| s.cast::<IAudioSessionControl2>()) else { continue };
                if s.GetState() != Ok(AudioSessionStateActive) || s.IsSystemSoundsSession() == S_OK {
                    continue;
                }
                if let Ok(pid) = s.GetProcessId()
                    && pid != 0
                    && pid != me
                    && let Some(name) = process_name(pid)
                {
                    out.push(name);
                }
            }
        }
    }
    out
}

fn process_name(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(exe_stem(&String::from_utf16_lossy(&buf[..len as usize])))
    }
}

/// C:\Program Files\Zoom\bin\Zoom.exe → Zoom
fn exe_stem(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(exe_stem(r"C:\Program Files\Zoom\bin\Zoom.exe"), "Zoom");
        assert_eq!(exe_stem(r"C:\Program Files\WindowsApps\Microsoft.WindowsCamera_2026\WindowsCamera.exe"), "WindowsCamera");
        assert_eq!(exe_stem("plain"), "plain");
    }

    #[test]
    fn any_in_use() {
        assert!(!InUse::default().any());
        assert!(InUse { mic: vec!["a".into()], cam: vec![] }.any());
    }

    /// 读本机当前状态，默认跳过：cargo test -- --ignored live_mic --nocapture
    #[test]
    #[ignore]
    fn live_mic() {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap() };
        let devices: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap() };
        println!("正在用麦克风：{:?}", mic_names(&session_managers(&devices), 0));
    }
}
