//! 蓝牙耳机电量。
//!
//! 耳机通过免提协议上报的电量，Windows 存在蓝牙设备节点的一个属性上（设置里「蓝牙和设备」显示的就是它）。
//! 音频端点和这个节点同属一台物理设备，容器 id 相同，按容器 id 找过去读出来。

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GETIDLIST_FILTER_PRESENT, CM_Get_DevNode_PropertyW, CM_Get_Device_ID_ListW, CM_Get_Device_ID_List_SizeW,
    CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW, CR_SUCCESS,
};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_ContainerId;
use windows::Win32::Devices::Properties::{DEVPKEY_Device_ContainerId, DEVPROPTYPE};
use windows::Win32::Foundation::DEVPROPKEY;
use windows::Win32::Media::Audio::IMMDevice;
use windows::Win32::System::Com::STGM_READ;
use windows::Win32::System::Com::StructuredStorage::PropVariantToGUID;
use windows::core::{GUID, PCWSTR};

/// 蓝牙设备的电量百分比（DEVPROP_TYPE_BYTE）。
const DEVPKEY_BLUETOOTH_BATTERY: DEVPROPKEY =
    DEVPROPKEY { fmtid: GUID::from_u128(0x104ea319_6ee2_4701_bd47_8ddbf425bbe5), pid: 2 };

/// 输出设备所属蓝牙耳机的电量；不是蓝牙设备或耳机没上报时返回 None。
pub fn battery_of(dev: &IMMDevice) -> Option<u8> {
    let container = unsafe {
        dev.OpenPropertyStore(STGM_READ)
            .and_then(|p| p.GetValue(&PKEY_Device_ContainerId))
            .and_then(|v| PropVariantToGUID(&v))
            .ok()?
    };
    for id in device_ids() {
        // 只看蓝牙枚举出来的节点（BTHENUM / BTHHFENUM / BTHLE…）
        if !id.starts_with("BTH") {
            continue;
        }
        let wide: Vec<u16> = id.encode_utf16().chain([0]).collect();
        let mut inst = 0u32;
        if unsafe { CM_Locate_DevNodeW(&mut inst, PCWSTR(wide.as_ptr()), CM_LOCATE_DEVNODE_NORMAL) } != CR_SUCCESS {
            continue;
        }
        if prop::<GUID>(inst, &DEVPKEY_Device_ContainerId) != Some(container) {
            continue;
        }
        if let Some(b) = prop::<u8>(inst, &DEVPKEY_BLUETOOTH_BATTERY) {
            return Some(b.min(100));
        }
    }
    None
}

/// 当前在线的所有设备实例 id。
fn device_ids() -> Vec<String> {
    unsafe {
        let mut len = 0u32;
        if CM_Get_Device_ID_List_SizeW(&mut len, PCWSTR::null(), CM_GETIDLIST_FILTER_PRESENT) != CR_SUCCESS {
            return Vec::new();
        }
        let mut buf = vec![0u16; len as usize];
        if CM_Get_Device_ID_ListW(PCWSTR::null(), &mut buf, CM_GETIDLIST_FILTER_PRESENT) != CR_SUCCESS {
            return Vec::new();
        }
        split_multi_sz(&buf)
    }
}

/// "a\0b\0\0" → ["a", "b"]
fn split_multi_sz(buf: &[u16]) -> Vec<String> {
    buf.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
}

/// 读一个定长的设备节点属性。
fn prop<T: Copy + Default>(inst: u32, key: &DEVPROPKEY) -> Option<T> {
    let mut ty = DEVPROPTYPE::default();
    let mut v = T::default();
    let mut size = size_of::<T>() as u32;
    let r = unsafe { CM_Get_DevNode_PropertyW(inst, key, &mut ty, Some(&mut v as *mut T as *mut u8), &mut size, 0) };
    (r == CR_SUCCESS && size as usize == size_of::<T>()).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_sz() {
        let buf: Vec<u16> = "BTHENUM\\A\0USB\\B\0\0".encode_utf16().collect();
        assert_eq!(split_multi_sz(&buf), ["BTHENUM\\A", "USB\\B"]);
        assert!(split_multi_sz(&[0, 0]).is_empty());
    }

    /// 列出本机输出设备和读到的电量，默认跳过：cargo test -- --ignored live_battery --nocapture
    #[test]
    #[ignore]
    fn live_battery() {
        use windows::Win32::Media::Audio::{DEVICE_STATE_ACTIVE, IMMDeviceEnumerator, MMDeviceEnumerator, eRender};
        use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
            let list = e.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE).unwrap();
            for i in 0..list.GetCount().unwrap() {
                let d = list.Item(i).unwrap();
                println!("{:?}", battery_of(&d));
            }
        }
    }
}
