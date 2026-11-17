//! 测试工具：切换两次静音（最终不变），用来触发音量变化通知。
//! 用法：cargo run --example vol_poke

use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};

fn main() -> windows::core::Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let ep: IAudioEndpointVolume = e.GetDefaultAudioEndpoint(eRender, eConsole)?.Activate(CLSCTX_ALL, None)?;
        let v = ep.GetMasterVolumeLevelScalar()?;
        let m = ep.GetMute()?.as_bool();
        ep.SetMute(!m, std::ptr::null())?;
        std::thread::sleep(std::time::Duration::from_millis(200));
        ep.SetMute(m, std::ptr::null())?;
        println!("音量 {:.0}%，静音 {m}（已恢复）", v * 100.0);
    }
    Ok(())
}
