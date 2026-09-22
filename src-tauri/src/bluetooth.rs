//! 蓝牙邻近检测，两级策略：
//!
//! ① 系统连接直读（推荐，最可靠）：设备与本机已配对时，通过 WinRT
//!    `GattSession.MaintainConnection` 让 Windows 持续维持一条 BLE 连接——
//!    手机只要在蓝牙范围内系统就自动连上，离开范围自动断开。
//!    直读 `ConnectionStatus` 即"在不在范围"：无需手机可发现、无需连 WiFi、
//!    手机端零操作（配对一次即可）。会话对象常驻内存，跨检测轮次复用。
//!
//! ② 经典查询扫描（兜底）：对未配对设备执行 inquiry。只有处于「可被发现」
//!    状态的设备才能被听到；结果中的「已配对/记住」缓存条目与在场无关
//!    （手机关机也会出现），以 stLastSeen ≥ 本轮扫描开始时刻 或 当前已连接
//!    判定真实在场。

use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
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
use windows::core::AgileReference;
use windows::core::HSTRING;
use windows::Devices::Bluetooth::GenericAttributeProfile::GattSession;
use windows::Devices::Bluetooth::{BluetoothConnectionStatus, BluetoothLEDevice};
use windows::Devices::Enumeration::DeviceInformation;

/// EPOCH_FILETIME ticks 与 Unix 毫秒的差值（1601-01-01 → 1970-01-01）
const FILETIME_TO_UNIX_MS: u64 = 11_644_473_600_000;
/// stLastSeen 判定余量：栈写入时间戳可能略早于我们的开始时刻
const SEEN_GRACE_MS: u64 = 2_000;
/// 首次建立系统连接时等待连接恢复的时长（手机在范围内通常 1~3 秒连上）
const LINK_WAIT_MS: u64 = 4_000;

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
    /// 是否为本机已配对设备
    pub paired: bool,
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

/// 从 BLE DeviceInformation.Id 解析远端 MAC。
/// 形如 `BluetoothLE#BluetoothLE00:1a:7d:da:71:13-0c:f3:ee:4c:f6:73`，取 '-' 之后一段。
pub fn parse_le_id(id: &str) -> Option<String> {
    let after = id.split('#').nth(1)?;
    let remote = after.split('-').nth(1)?;
    let hex: String = remote.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() == 12 {
        Some(hex.to_lowercase())
    } else {
        None
    }
}

/* ===================== ① 系统连接直读（已配对设备） ===================== */

/// 已建立的系统维持连接（session 对象必须存活，MaintainConnection 才持续生效）
pub struct BtLink {
    device: AgileReference<BluetoothLEDevice>,
    _session: AgileReference<GattSession>,
    pub name: String,
    pub address: String,
}

static LINK: OnceLock<Mutex<Option<(String, BtLink)>>> = OnceLock::new();

fn fe(v: impl std::fmt::Display) -> String {
    format!("Windows 蓝牙 API 调用失败：{v}")
}

/// 枚举本机已配对的 BLE 设备（名称、MAC、原始 Id）
pub fn paired_ble_devices() -> Result<Vec<(String, String, String)>, String> {
    let selector = BluetoothLEDevice::GetDeviceSelectorFromPairingState(true).map_err(fe)?;
    let coll = DeviceInformation::FindAllAsyncAqsFilter(&selector)
        .map_err(fe)?
        .get()
        .map_err(fe)?;
    let n = coll.Size().map_err(fe)?;
    let mut out = Vec::new();
    for i in 0..n {
        let info = coll.GetAt(i).map_err(fe)?;
        let name = info.Name().map_err(fe)?.to_string();
        let id = info.Id().map_err(fe)?.to_string();
        let addr = parse_le_id(&id).unwrap_or_default();
        out.push((name, addr, id));
    }
    Ok(out)
}

/// 为匹配的已配对设备建立系统维持连接；未找到返回 None
pub fn prepare_link(matcher: &str) -> Result<Option<BtLink>, String> {
    for (name, addr, id) in paired_ble_devices()? {
        if device_matches(&name, &addr, matcher) {
            let dev = BluetoothLEDevice::FromIdAsync(&HSTRING::from(id))
                .map_err(fe)?
                .get()
                .map_err(fe)?;
            let devid = dev.BluetoothDeviceId().map_err(fe)?;
            let session = GattSession::FromDeviceIdAsync(&devid)
                .map_err(fe)?
                .get()
                .map_err(fe)?;
            session.SetMaintainConnection(true).map_err(fe)?;
            return Ok(Some(BtLink {
                device: AgileReference::new(&dev).map_err(fe)?,
                _session: AgileReference::new(&session).map_err(fe)?,
                name,
                address: addr,
            }));
        }
    }
    Ok(None)
}

/// 读取系统连接状态：手机在蓝牙范围内 = Connected
pub fn link_connected(link: &BtLink) -> Result<bool, String> {
    let dev = link.device.resolve().map_err(fe)?;
    Ok(dev.ConnectionStatus().map_err(fe)? == BluetoothConnectionStatus::Connected)
}

/// 目标是否存在已配对设备（用于提示检测模式）
pub fn paired_match_exists(matcher: &str) -> Result<bool, String> {
    for (name, addr, _id) in paired_ble_devices()? {
        if device_matches(&name, &addr, matcher) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 汇总扫描：已配对设备（含连接状态）+ inquiry 真实在场设备（去重）
pub fn scan_all(timeout_mult: u32) -> Result<Vec<BtDevice>, String> {
    let mut out: Vec<BtDevice> = Vec::new();
    if let Ok(pairs) = paired_ble_devices() {
        for (name, addr, id) in pairs {
            let connected = BluetoothLEDevice::FromIdAsync(&HSTRING::from(id))
                .map(|op| op.get().ok())
                .ok()
                .flatten()
                .and_then(|d| d.ConnectionStatus().ok())
                .map(|s| s == BluetoothConnectionStatus::Connected)
                .unwrap_or(false);
            out.push(BtDevice {
                name,
                address: addr,
                connected,
                in_range: true,
                paired: true,
            });
        }
    }
    if let Ok(devs) = inquiry(timeout_mult) {
        for d in devs {
            if d.in_range && !out.iter().any(|x| x.address == d.address) {
                out.push(BtDevice { paired: false, ..d });
            }
        }
    }
    Ok(out)
}

/// 判断目标设备是否真实在附近。
/// 优先走系统连接直读（已配对），未配对时退回经典 inquiry 扫描。
pub fn presence(matcher: &str, timeout_mult: u32) -> Result<bool, String> {
    let key = matcher.trim().to_string();
    if key.is_empty() {
        return Err("未指定要监控的蓝牙设备（名称或 MAC）".into());
    }
    let lock = LINK.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().unwrap();
    let need_new = match guard.as_ref() {
        Some((k, _)) => *k != key,
        None => true,
    };
    if need_new {
        *guard = None; // 释放旧目标连接（对象丢弃即断开维持）
        if let Some(link) = prepare_link(matcher)? {
            // 给系统一点时间建立/恢复连接：手机在范围内会自动连上
            let deadline = std::time::Instant::now() + Duration::from_millis(LINK_WAIT_MS);
            while std::time::Instant::now() < deadline {
                if link_connected(&link).unwrap_or(false) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(400));
            }
            *guard = Some((key, link));
        }
    }
    if let Some((_, link)) = guard.as_ref() {
        return link_connected(link);
    }
    drop(guard);
    // 未配对 → 兜底：经典 inquiry（仅「可被发现」设备可见）
    Ok(inquiry(timeout_mult)?
        .iter()
        .any(|d| d.in_range && device_matches(&d.name, &d.address, matcher)))
}

/// 当前系统维持连接绑定的目标（名称, MAC）；未绑定时返回 None
pub fn link_target() -> Option<(String, String)> {
    LINK.get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .as_ref()
        .map(|(_, l)| (l.name.clone(), l.address.clone()))
}

/* ===================== ② 经典查询扫描（兜底） ===================== */

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
                paired: di.fAuthenticated != 0,
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
        assert_eq!(normalize_mac("A4:C1:38:11:22:33").as_deref(), Some("a4c138112233"));
        assert_eq!(normalize_mac("a4-c1-38-11-22-33").as_deref(), Some("a4c138112233"));
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
    fn le_id_parse() {
        let id = "BluetoothLE#BluetoothLE00:1a:7d:da:71:13-0c:f3:ee:4c:f6:73";
        assert_eq!(parse_le_id(id).as_deref(), Some("0cf3ee4cf673"));
        assert_eq!(parse_le_id(" nonsense "), None);
        assert_eq!(
            parse_le_id("BluetoothLE#BluetoothLEaa:bb-11:22:33:44:55:66").as_deref(),
            Some("112233445566")
        );
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

    /// 汇总扫描冒烟：已配对枚举 + inquiry 合并
    #[test]
    fn scan_all_smoke() {
        match scan_all(1) {
            Ok(devs) => {
                let paired = devs.iter().filter(|d| d.paired).count();
                println!("汇总扫描：{} 个设备（已配对 {} 个）", devs.len(), paired);
            }
            Err(e) => println!("汇总扫描不可用（无适配器属预期）：{e}"),
        }
    }
}
