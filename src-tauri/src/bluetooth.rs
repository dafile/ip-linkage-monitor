//! 蓝牙邻近检测：通过 Windows 经典蓝牙查询（inquiry）发现附近设备。
//!
//! 可发现的对象：
//! 1. 处于「可被发现」状态的设备（如手机停在蓝牙设置页）；
//! 2. 与本机已配对/已连接的设备（fReturnConnected / fReturnRemembered）。
//!
//! 注意：手机平时（息屏、不在配对页）大多不可被经典蓝牙查询发现，
//! 这是系统层面的限制；与电脑保持蓝牙连接的设备检测最可靠。

use serde::Serialize;
use winapi::shared::minwindef::DWORD;
use winapi::um::bluetoothapis::{
    BluetoothFindDeviceClose, BluetoothFindFirstDevice, BluetoothFindFirstRadio,
    BluetoothFindNextDevice, BluetoothFindRadioClose, BLUETOOTH_DEVICE_INFO,
    BLUETOOTH_DEVICE_SEARCH_PARAMS, BLUETOOTH_FIND_RADIO_PARAMS,
};
use winapi::um::handleapi::CloseHandle;
use winapi::um::winnt::HANDLE;

/// 扫描结果中的单个蓝牙设备
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BtDevice {
    pub name: String,
    /// 12 位十六进制小写 MAC（无分隔符）
    pub address: String,
    pub connected: bool,
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 把用户输入规整为 12 位十六进制 MAC；不像 MAC 的输入返回 None
pub fn normalize_mac(s: &str) -> Option<String> {
    let cleaned: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if cleaned.len() == 12 && s.contains([':', '-', ' ']) || (cleaned.len() == 12 && !s.contains(' ') && !s.contains('.')) {
        Some(cleaned.to_lowercase())
    } else {
        None
    }
}

/// 判断扫描到的设备是否匹配用户指定的名称或 MAC（名称为包含匹配，MAC 为精确匹配）
pub fn device_matches(name: &str, address: &str, matcher: &str) -> bool {
    let m = matcher.trim().to_lowercase();
    if m.is_empty() {
        return false;
    }
    if let Some(mac) = normalize_mac(&m) {
        if address == mac {
            return true;
        }
    }
    name.to_lowercase().contains(&m)
}

/// 执行一轮蓝牙设备查询。耗时约 timeout_mult × 1.28 秒。
/// 返回发现的全部设备；本机无蓝牙适配器时返回 Err。
pub fn inquiry(timeout_mult: u32) -> Result<Vec<BtDevice>, String> {
    unsafe {
        let mut rp: BLUETOOTH_FIND_RADIO_PARAMS = std::mem::zeroed();
        rp.dwSize = std::mem::size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as DWORD;
        let mut h_radio: HANDLE = std::ptr::null_mut();
        let radio_find = BluetoothFindFirstRadio(&mut rp, &mut h_radio);
        if radio_find.is_null() {
            return Err("未检测到可用的蓝牙适配器".into());
        }
        BluetoothFindRadioClose(radio_find);

        let mut sp: BLUETOOTH_DEVICE_SEARCH_PARAMS = std::mem::zeroed();
        sp.dwSize = std::mem::size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as DWORD;
        sp.fReturnAuthenticated = 1;
        sp.fReturnRemembered = 1;
        sp.fReturnUnknown = 1;
        sp.fReturnConnected = 1;
        sp.fIssueInquiry = 1; // 执行真实无线查询，而非读缓存
        sp.cTimeoutMultiplier = timeout_mult.clamp(1, 48) as u8;
        sp.hRadio = h_radio;

        let mut di: BLUETOOTH_DEVICE_INFO = std::mem::zeroed();
        di.dwSize = std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as DWORD;

        let mut out: Vec<BtDevice> = Vec::new();
        let h_find = BluetoothFindFirstDevice(&mut sp, &mut di);
        if h_find.is_null() {
            CloseHandle(h_radio);
            let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            // ERROR_NO_MORE_ITEMS(259)：查询正常完成但没有发现任何设备
            if code == 259 {
                return Ok(out);
            }
            return Err(format!("蓝牙设备查询失败（错误码 {code}）"));
        }
        loop {
            out.push(BtDevice {
                name: from_wide(&di.szName),
                address: format!("{:012x}", di.Address),
                connected: di.fConnected != 0,
            });
            if BluetoothFindNextDevice(h_find, &mut di) == 0 {
                break;
            }
        }
        BluetoothFindDeviceClose(h_find);
        CloseHandle(h_radio);
        Ok(out)
    }
}

/// 判断目标设备是否在附近（名称或 MAC 匹配）
pub fn presence(matcher: &str, timeout_mult: u32) -> Result<bool, String> {
    let m = matcher.trim();
    if m.is_empty() {
        return Err("未指定要监控的蓝牙设备（名称或 MAC）".into());
    }
    Ok(inquiry(timeout_mult)?
        .iter()
        .any(|d| device_matches(&d.name, &d.address, m)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_normalize() {
        assert_eq!(
            normalize_mac("A4:C1:38:11:22:33").as_deref(),
            Some("a4c138112233")
        );
        assert_eq!(
            normalize_mac("a4-c1-38-11-22-33").as_deref(),
            Some("a4c138112233")
        );
        assert_eq!(normalize_mac("a4c138112233").as_deref(), Some("a4c138112233"));
        assert_eq!(normalize_mac("Xiaomi 14"), None);
        assert_eq!(normalize_mac("172.30.109.238"), None);
    }

    #[test]
    fn matcher_by_name_and_mac() {
        assert!(device_matches("Xiaomi 14", "a4c138112233", "xiaomi"));
        assert!(device_matches("小米手机", "a4c138112233", "A4:C1:38:11:22:33"));
        assert!(device_matches("", "a4c138112233", "a4-c1-38-11-22-33"));
        assert!(device_matches("Galaxy Buds", "001122334455", "galaxy"));
        assert!(!device_matches("Other Phone", "001122334455", "a4:c1:38:11:22:33"));
        assert!(!device_matches("Xiaomi 14", "a4c138112233", ""));
        assert!(!device_matches("Xiaomi 14", "a4c138112233", "iphone"));
    }

    /// 扫描冒烟测试：无适配器时应优雅返回 Err 而非崩溃（有适配器则返回设备列表）
    #[test]
    fn inquiry_smoke() {
        let r = inquiry(1);
        match r {
            Ok(devs) => println!("蓝牙扫描成功，发现 {} 个设备", devs.len()),
            Err(e) => println!("蓝牙扫描不可用（本机无适配器属预期）：{e}"),
        }
    }
}
