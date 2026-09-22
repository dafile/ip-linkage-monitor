//! 蓝牙邻近检测：通过 Windows 经典蓝牙查询（inquiry）发现附近设备。
//!
//! 在场判定（关键）：搜索结果中会混入本机「已配对 / 记住」的缓存设备，
//! 与设备当前是否在附近无关（手机关机也会出现）。蓝牙栈只有在 inquiry
//! 真实听到设备时才会刷新其 stLastSeen 时间戳，因此以
//! 「stLastSeen ≥ 本次扫描开始时刻」或「当前已连接」判定真实在场。
//!
//! 最可靠的监控对象：与本机保持蓝牙连接的设备；其次是处于「可被发现」
//! 状态的设备（如手机停在蓝牙设置页）。多数手机平时息屏不可被经典蓝牙
//! 查询发现，这是系统层面的限制。

use serde::Serialize;
use winapi::shared::minwindef::DWORD;
use winapi::shared::minwindef::FILETIME;
use winapi::um::bluetoothapis::{
    BluetoothFindDeviceClose, BluetoothFindFirstDevice, BluetoothFindFirstRadio,
    BluetoothFindNextDevice, BluetoothFindRadioClose, BLUETOOTH_DEVICE_INFO,
    BLUETOOTH_DEVICE_SEARCH_PARAMS, BLUETOOTH_FIND_RADIO_PARAMS,
};
use winapi::um::handleapi::CloseHandle;
use winapi::um::minwinbase::SYSTEMTIME;
use winapi::um::sysinfoapi::GetSystemTimeAsFileTime;
use winapi::um::timezoneapi::SystemTimeToFileTime;
use winapi::um::winnt::HANDLE;

/// EPOCH_FILETIME ticks 与 Unix 毫秒的差值（1601-01-01 → 1970-01-01）
const FILETIME_TO_UNIX_MS: u64 = 11_644_473_600_000;
/// stLastSeen 判定余量：栈写入时间戳可能略早于我们的开始时刻
const SEEN_GRACE_MS: u64 = 2_000;

/// 扫描结果中的单个蓝牙设备
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BtDevice {
    pub name: String,
    /// 12 位十六进制小写 MAC（无分隔符）
    pub address: String,
    pub connected: bool,
    /// 真实在场：本轮 inquiry 听到，或当前已连接（缓存设备为 false）
    pub in_range: bool,
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn filetime_to_unix_ms(ft: &FILETIME) -> u64 {
    let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    ticks / 10_000 - FILETIME_TO_UNIX_MS
}

fn now_unix_ms() -> u64 {
    unsafe {
        let mut ft: FILETIME = std::mem::zeroed();
        GetSystemTimeAsFileTime(&mut ft);
        filetime_to_unix_ms(&ft)
    }
}

fn systime_to_unix_ms(st: &SYSTEMTIME) -> Option<u64> {
    unsafe {
        let mut ft: FILETIME = std::mem::zeroed();
        if SystemTimeToFileTime(st, &mut ft) == 0 {
            return None;
        }
        Some(filetime_to_unix_ms(&ft))
    }
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
/// 返回发现的全部设备（含缓存，用 in_range 区分真实在场）；无适配器时返回 Err。
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

        let scan_start_ms = now_unix_ms().saturating_sub(SEEN_GRACE_MS);

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
            let connected = di.fConnected != 0;
            let heard_now = systime_to_unix_ms(&di.stLastSeen)
                .map(|t| t >= scan_start_ms)
                .unwrap_or(false);
            out.push(BtDevice {
                name: from_wide(&di.szName),
                address: format!("{:012x}", di.Address),
                connected,
                in_range: connected || heard_now,
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

/// 判断目标设备是否真实在附近（在场 + 名称或 MAC 匹配）
pub fn presence(matcher: &str, timeout_mult: u32) -> Result<bool, String> {
    let m = matcher.trim();
    if m.is_empty() {
        return Err("未指定要监控的蓝牙设备（名称或 MAC）".into());
    }
    Ok(inquiry(timeout_mult)?
        .iter()
        .any(|d| d.in_range && device_matches(&d.name, &d.address, m)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_systemtime(y: u16, mo: u16, d: u16, h: u16, mi: u16, s: u16) -> SYSTEMTIME {
        let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
        st.wYear = y; st.wMonth = mo; st.wDay = d;
        st.wHour = h; st.wMinute = mi; st.wSecond = s;
        st
    }

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

    #[test]
    fn systime_conversion() {
        // 2026-01-01 00:00:00 UTC = 1767225600 秒
        assert_eq!(
            systime_to_unix_ms(&make_systemtime(2026, 1, 1, 0, 0, 0)).unwrap(),
            1_767_225_600_000
        );
        // 2024-02-29 12:34:56 UTC（闰年）= 1704067200 + 59*86400 + 45296 秒
        assert_eq!(
            systime_to_unix_ms(&make_systemtime(2024, 2, 29, 12, 34, 56)).unwrap(),
            1_709_210_096_000
        );
        // 当前时间转换单调合理
        let now = now_unix_ms();
        assert!(now > 1_700_000_000_000);
    }

    /// 扫描冒烟测试：无适配器时应优雅返回 Err 而非崩溃（有适配器则返回设备列表）
    #[test]
    fn inquiry_smoke() {
        let r = inquiry(1);
        match r {
            Ok(devs) => {
                let in_range = devs.iter().filter(|d| d.in_range).count();
                println!("蓝牙扫描成功：共 {} 个条目，其中在场 {} 个", devs.len(), in_range);
            }
            Err(e) => println!("蓝牙扫描不可用（本机无适配器属预期）：{e}"),
        }
    }
}
