//! 测试工具：模拟按一次音量键（默认「音量减」，参数 up 为「音量加」），2.5 秒后把音量和静音恢复原样。
//! 用法：cargo run --example vol_key [up]

use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY, VK_VOLUME_DOWN, VK_VOLUME_UP,
};

fn key(vk: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, dwFlags: flags, ..Default::default() } },
    }
}

fn main() -> windows::core::Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let ep: IAudioEndpointVolume = e.GetDefaultAudioEndpoint(eRender, eConsole)?.Activate(CLSCTX_ALL, None)?;
        let (v, m) = (ep.GetMasterVolumeLevelScalar()?, ep.GetMute()?);
        let vk = if std::env::args().any(|a| a == "up") { VK_VOLUME_UP } else { VK_VOLUME_DOWN };
        SendInput(&[key(vk, KEYBD_EVENT_FLAGS(0)), key(vk, KEYEVENTF_KEYUP)], size_of::<INPUT>() as i32);
        std::thread::sleep(std::time::Duration::from_millis(2500));
        ep.SetMasterVolumeLevelScalar(v, std::ptr::null())?;
        ep.SetMute(m.as_bool(), std::ptr::null())?;
        println!("已恢复：音量 {:.0}%，静音 {}", v * 100.0, m.as_bool());
    }
    Ok(())
}
