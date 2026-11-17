//! 测试工具：打开默认麦克风 N 秒（默认 4 秒）再关掉，数据直接丢弃，不录音。
//! 用来触发系统的“麦克风使用中”记录，检查岛上的橙色指示点。
//! 用法：cargo run --example mic_probe [秒数]

use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eConsole,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree};

fn main() -> windows::core::Result<()> {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(4);
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let client: IAudioClient = e.GetDefaultAudioEndpoint(eCapture, eConsole)?.Activate(CLSCTX_ALL, None)?;
        let fmt = client.GetMixFormat()?;
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 10_000_000, 0, fmt, None)?;
        CoTaskMemFree(Some(fmt as _));
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        println!("麦克风已打开 {secs} 秒");
        let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(50));
            // 把缓冲区里的数据读出来直接扔掉
            while capture.GetNextPacketSize()? > 0 {
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                capture.ReleaseBuffer(frames)?;
            }
        }
        client.Stop()?;
        println!("麦克风已关闭");
    }
    Ok(())
}
