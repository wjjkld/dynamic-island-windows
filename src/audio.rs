//! 系统音量：CoreAudio 回调通知音量/静音变化，默认输出设备切换（插耳机等）时重新挂接；
//! 新的输出设备连上（蓝牙耳机等，不一定成为默认设备）时也通知。
//! 回调跑在系统的工作线程上，只记下数据并唤醒 UI 线程。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl};
use windows::Win32::Media::Audio::{
    AUDIO_VOLUME_NOTIFICATION_DATA, DEVICE_STATE, DEVICE_STATE_ACTIVE, EDataFlow, ERole, IMMDevice, IMMDeviceEnumerator,
    IMMEndpoint, IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToBSTR;
use windows::Win32::System::Com::{CLSCTX_ALL, CLSCTX_INPROC_SERVER, CoCreateInstance, STGM_READ};
use windows::core::{HSTRING, Interface, PCWSTR, Result};
use windows_core::implement;

use crate::headset;
use crate::volkeys::{VolKey, stepped};

type Wake = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Shared {
    volume_bits: AtomicU32,
    muted: AtomicBool,
    version: AtomicU64,
    reattach: AtomicBool,
    /// 刚变成可用的设备 id，主线程再判断是不是输出设备。
    arrived: Mutex<Vec<String>>,
}

/// 输出设备的变化。
pub enum DeviceEvent {
    /// 成了默认输出设备：(名字, 音量, 是否静音, 蓝牙耳机电量)
    Default(String, f32, bool, Option<u8>),
    /// 连上了，但没有成为默认设备：(名字, 蓝牙耳机电量)
    Connected(String, Option<u8>),
}

fn trace(msg: impl FnOnce() -> String) {
    if cfg!(debug_assertions) && std::env::var_os("ISLAND_TRACE").is_some() {
        eprintln!("[audio] {}", msg());
    }
}

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeCb {
    shared: Arc<Shared>,
    wake: Wake,
}

impl IAudioEndpointVolumeCallback_Impl for VolumeCb_Impl {
    fn OnNotify(&self, data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> Result<()> {
        if let Some(d) = unsafe { data.as_ref() } {
            self.shared.volume_bits.store(d.fMasterVolume.to_bits(), Ordering::Relaxed);
            self.shared.muted.store(d.bMuted.as_bool(), Ordering::Relaxed);
            self.shared.version.fetch_add(1, Ordering::Release);
            (self.wake)();
        }
        Ok(())
    }
}

#[implement(IMMNotificationClient)]
struct DeviceCb {
    shared: Arc<Shared>,
    wake: Wake,
}

impl IMMNotificationClient_Impl for DeviceCb_Impl {
    fn OnDeviceStateChanged(&self, id: &PCWSTR, state: DEVICE_STATE) -> Result<()> {
        let id = unsafe { id.to_string() }.unwrap_or_default();
        trace(|| format!("state {id} -> {}", state.0));
        if state == DEVICE_STATE_ACTIVE && !id.is_empty() {
            self.shared.arrived.lock().unwrap().push(id);
            (self.wake)();
        }
        Ok(())
    }
    fn OnDeviceAdded(&self, _: &PCWSTR) -> Result<()> {
        Ok(())
    }
    fn OnDeviceRemoved(&self, _: &PCWSTR) -> Result<()> {
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _: &PCWSTR) -> Result<()> {
        trace(|| format!("default changed flow={} role={}", flow.0, role.0));
        if flow == eRender && role == eConsole {
            self.shared.reattach.store(true, Ordering::Release);
            (self.wake)();
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> Result<()> {
        Ok(())
    }
}

pub struct Audio {
    enumerator: IMMDeviceEnumerator,
    endpoint: Option<IAudioEndpointVolume>,
    volume_cb: IAudioEndpointVolumeCallback,
    device_cb: IMMNotificationClient,
    shared: Arc<Shared>,
    seen: u64,
    /// 当前输出设备的名字。
    name: String,
    /// 输出设备的变化，等主循环取走去显示。
    switched: Option<DeviceEvent>,
    /// 上次提示设备变化的时间：蓝牙耳机一连上会同时出现「立体声」「免提」两个设备，只提示一次。
    last_event: Option<Instant>,
}

impl Audio {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Result<Audio> {
        let wake: Wake = Arc::new(wake);
        let shared = Arc::new(Shared::default());
        unsafe {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)?;
            let device_cb: IMMNotificationClient = DeviceCb { shared: shared.clone(), wake: wake.clone() }.into();
            enumerator.RegisterEndpointNotificationCallback(&device_cb)?;
            let volume_cb: IAudioEndpointVolumeCallback = VolumeCb { shared: shared.clone(), wake }.into();
            let mut a = Audio { enumerator, endpoint: None, volume_cb, device_cb, shared, seen: 0, name: String::new(), switched: None, last_event: None };
            a.attach();
            a.switched = None;
            Ok(a)
        }
    }

    fn attach(&mut self) {
        unsafe {
            if let Some(old) = self.endpoint.take() {
                let _ = old.UnregisterControlChangeNotify(&self.volume_cb);
            }
            let Ok(dev) = self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole) else { return };
            let Ok(ep) = dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) else { return };
            let name = device_name(&dev);
            // 同一个设备重复通知（不同角色）不提示
            if name != self.name {
                let volume = ep.GetMasterVolumeLevelScalar().unwrap_or(0.0);
                let muted = ep.GetMute().map(|m| m.as_bool()).unwrap_or(false);
                trace(|| format!("default is now {name}"));
                let battery = headset::battery_of(&dev);
                trace(|| format!("battery {battery:?}"));
                self.switched = Some(DeviceEvent::Default(name.clone(), volume, muted, battery));
                self.last_event = Some(Instant::now());
                self.name = name;
            }
            if ep.RegisterControlChangeNotify(&self.volume_cb).is_ok() {
                self.endpoint = Some(ep);
            }
        }
    }

    /// 代替系统处理一次音量键。成功返回 true；随后会照常收到音量变化通知。
    pub fn press(&self, key: VolKey) -> bool {
        let Some(ep) = &self.endpoint else { return false };
        unsafe {
            match key {
                VolKey::Mute => {
                    let Ok(m) = ep.GetMute() else { return false };
                    ep.SetMute(!m.as_bool(), std::ptr::null()).is_ok()
                }
                VolKey::Up | VolKey::Down => {
                    let Ok(v) = ep.GetMasterVolumeLevelScalar() else { return false };
                    // 和系统一样：按音量键会解除静音
                    let _ = ep.SetMute(false, std::ptr::null());
                    ep.SetMasterVolumeLevelScalar(stepped(v, key == VolKey::Up), std::ptr::null()).is_ok()
                }
            }
        }
    }

    /// 输出设备有变化时返回。
    pub fn take_switch(&mut self) -> Option<DeviceEvent> {
        self.switched.take()
    }

    /// 处理新连上的设备：是输出设备、又不是当前默认设备（那种由默认切换提示）时提示「已连接」。
    fn check_arrived(&mut self) {
        let ids = std::mem::take(&mut *self.shared.arrived.lock().unwrap());
        for id in ids {
            let dev: Option<IMMDevice> = unsafe { self.enumerator.GetDevice(&HSTRING::from(id.as_str())).ok() };
            let Some(dev) = dev else { continue };
            let is_output = dev.cast::<IMMEndpoint>().and_then(|e| unsafe { e.GetDataFlow() }) == Ok(eRender);
            let name = unsafe { device_name(&dev) };
            trace(|| format!("arrived {name} output={is_output}"));
            let recent = self.last_event.is_some_and(|t| t.elapsed() < Duration::from_secs(3));
            if is_output && name != self.name && !recent && self.switched.is_none() {
                self.switched = Some(DeviceEvent::Connected(name, headset::battery_of(&dev)));
                self.last_event = Some(Instant::now());
            }
        }
    }

    /// 有新的音量变化时返回 (音量 0..1, 是否静音)。
    pub fn poll(&mut self) -> Option<(f32, bool)> {
        if self.shared.reattach.swap(false, Ordering::Acquire) {
            self.attach();
        }
        self.check_arrived();
        let v = self.shared.version.load(Ordering::Acquire);
        if v == self.seen {
            return None;
        }
        self.seen = v;
        Some((f32::from_bits(self.shared.volume_bits.load(Ordering::Relaxed)), self.shared.muted.load(Ordering::Relaxed)))
    }
}

unsafe fn device_name(dev: &IMMDevice) -> String {
    unsafe {
        dev.OpenPropertyStore(STGM_READ)
            .and_then(|p| p.GetValue(&PKEY_Device_FriendlyName))
            .and_then(|v| PropVariantToBSTR(&v))
            .map(|b| b.to_string())
            .unwrap_or_default()
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        unsafe {
            if let Some(ep) = self.endpoint.take() {
                let _ = ep.UnregisterControlChangeNotify(&self.volume_cb);
            }
            let _ = self.enumerator.UnregisterEndpointNotificationCallback(&self.device_cb);
        }
    }
}
