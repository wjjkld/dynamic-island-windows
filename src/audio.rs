//! 系统音量：CoreAudio 回调通知音量/静音变化，默认输出设备切换（插耳机等）时重新挂接。
//! 回调跑在系统的工作线程上，只写原子变量并唤醒 UI 线程。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl};
use windows::Win32::Media::Audio::{
    AUDIO_VOLUME_NOTIFICATION_DATA, DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToBSTR;
use windows::Win32::System::Com::{CLSCTX_ALL, CLSCTX_INPROC_SERVER, CoCreateInstance, STGM_READ};
use windows::core::{PCWSTR, Result};
use windows_core::implement;

use crate::volkeys::{VolKey, stepped};

type Wake = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Shared {
    volume_bits: AtomicU32,
    muted: AtomicBool,
    version: AtomicU64,
    reattach: AtomicBool,
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
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> Result<()> {
        Ok(())
    }
    fn OnDeviceAdded(&self, _: &PCWSTR) -> Result<()> {
        Ok(())
    }
    fn OnDeviceRemoved(&self, _: &PCWSTR) -> Result<()> {
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _: &PCWSTR) -> Result<()> {
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
    /// 默认输出设备换了，等主循环取走去显示。
    switched: Option<(String, f32, bool)>,
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
            let mut a = Audio { enumerator, endpoint: None, volume_cb, device_cb, shared, seen: 0, name: String::new(), switched: None };
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
            let name = dev
                .OpenPropertyStore(STGM_READ)
                .and_then(|p| p.GetValue(&PKEY_Device_FriendlyName))
                .and_then(|v| PropVariantToBSTR(&v))
                .map(|b| b.to_string())
                .unwrap_or_default();
            // 同一个设备重复通知（不同角色）不提示
            if name != self.name {
                let volume = ep.GetMasterVolumeLevelScalar().unwrap_or(0.0);
                let muted = ep.GetMute().map(|m| m.as_bool()).unwrap_or(false);
                self.switched = Some((name.clone(), volume, muted));
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

    /// 默认输出设备换了时返回 (设备名, 音量, 是否静音)。
    pub fn take_switch(&mut self) -> Option<(String, f32, bool)> {
        self.switched.take()
    }

    /// 有新的音量变化时返回 (音量 0..1, 是否静音)。
    pub fn poll(&mut self) -> Option<(f32, bool)> {
        if self.shared.reattach.swap(false, Ordering::Acquire) {
            self.attach();
        }
        let v = self.shared.version.load(Ordering::Acquire);
        if v == self.seen {
            return None;
        }
        self.seen = v;
        Some((f32::from_bits(self.shared.volume_bits.load(Ordering::Relaxed)), self.shared.muted.load(Ordering::Relaxed)))
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
