//! 测试工具：看 Windows 定位每一步的结果（权限、定位），不联网、不打印坐标。
//! 用法：cargo run --example geo_probe

use windows::Devices::Geolocation::{Geolocator, PositionAccuracy};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let access = Geolocator::RequestAccessAsync().and_then(|op| op.join());
    println!("权限：{access:?}");
    let g = match Geolocator::new() {
        Ok(g) => g,
        Err(e) => return println!("创建定位器失败：{e:?}"),
    };
    println!("定位状态：{:?}", g.LocationStatus());
    let _ = g.SetDesiredAccuracy(PositionAccuracy::Default);
    let t = std::time::Instant::now();
    match g.GetGeopositionAsync().and_then(|op| op.join()) {
        Ok(p) => {
            let src = p.Coordinate().and_then(|c| c.PositionSource());
            let acc = p.Coordinate().and_then(|c| c.Accuracy());
            println!("定位成功（{:.1}s），来源 {src:?}，精度 {acc:?} 米", t.elapsed().as_secs_f32());
        }
        Err(e) => println!("定位失败（{:.1}s）：{e:?}", t.elapsed().as_secs_f32()),
    }
}
