//! 蓝牙邻近检测，多通道策略：
//!
//! 通道① 系统维持连接（推荐，已配对 BLE 设备）：
//!   通过 WinRT `GattSession.MaintainConnection` + 订阅电池服务通知，
//!   让 Windows 持续维持与手机的 BLE 连接——手机在蓝牙范围内系统就自动
//!   连上，离开范围自动断开；直读 `ConnectionStatus` 即"在不在附近"。
//!   无需手机可发现、无需连 WiFi、手机端零操作（配对一次即可，耗电极低）。
//!
//! 通道② 经典蓝牙连接状态：已配对设备若有任何经典档案在连接（音频、
//!   网络共享等），同样视为在附近（winapi 缓存查询，瞬时返回）。
//!
//! 通道③ 经典查询扫描（兜底，未配对设备）：
//!   仅「可被发现」状态的设备能被听到；结果中的「已配对/记住」缓存条目
//!   与在场无关（手机关机也会出现），以 stLastSeen ≥ 本轮扫描开始时刻
//!   或 当前已连接 判定真实在场。

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
use windows::core::GUID;
use windows::core::HSTRING;
use windows::Devices::Bluetooth::GenericAttributeProfile::GattClientCharacteristicConfigurationDescriptorValue;
use windows::Devices::Bluetooth::GenericAttributeProfile::GattCommunicationStatus;
use windows::Devices::Bluetooth::GenericAttributeProfile::GattSession;
use windows::Devices::Bluetooth::{BluetoothConnectionStatus, BluetoothLEDevice};
use windows::Devices::Enumeration::DeviceInformation;

/// EPOCH_FILETIME ticks 与 Unix 毫秒的差值（1601-01-01 → 1970-01-01）
const FILETIME_TO_UNIX_MS: u64 = 11_644_473_600_000;
/// stLastSeen 判定余量：栈写入时间戳可能略早于我们的开始时刻
const SEEN_GRACE_MS: u64 = 2_000;
/// 首次建立系统连接时等待连接恢复的时长（手机在范围内通常 1~3 秒连上）
const LINK_WAIT_MS: u64 = 6_000;

/// Standard Bluetooth 电池服务（Android/iOS 手机配对后普遍支持）
const BATTERY_SERVICE_GUID: GUID = GUID::from_u128(0x0000_180f_0000_1000_8000_0080_5f9b_34fb);
const BATTERY_LEVEL_GUID: GUID = GUID::from_u128(0x0000_2a19_0000_1000_8000_0080_5f9b_34fb);

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

/// 判断设备是否匹配用户指定的名称或 MAC（名称为包含匹配，MAC 为精确匹配）
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
    !name.is_empty() && name.to_lowercase().contains(&m)
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

/// 12 位 hex → AA:BB:CC:DD:EE:FF（显示用）
pub fn mac_colon(addr: &str) -> String {
    let b = addr.as_bytes();
    let mut out = String::new();
    for (i, c) in b.chunks(2).enumerate() {
        if i > 0 {
            out.push(':');
        }
        out.push_str(&String::from_utf8_lossy(c));
    }
    out
}

/* ===================== 通道③：winapi 蓝牙查询（扫描 + 缓存） ===================== */

/// 打开第一个本机蓝牙适配器；无适配器返回 Err
unsafe fn open_radio() -> Result<HANDLE, String> {
    let mut rp: BLUETOOTH_FIND_RADIO_PARAMS = std::mem::zeroed();
    rp.dwSize = std::mem::size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as DWORD;
    let mut h_radio: HANDLE = std::ptr::null_mut();
    let find = BluetoothFindFirstRadio(&mut rp, &mut h_radio);
    if find.is_null() {
        return Err("未检测到可用的蓝牙适配器".into());
    }
    BluetoothFindRadioClose(find);
    Ok(h_radio)
}

/// 按 search 参数枚举设备。scan_start 为 Some 时执行真实无线查询判定，
/// 为 None 时只读缓存（in_range = 已连接）。
unsafe fn find_devices(sp: &BLUETOOTH_DEVICE_SEARCH_PARAMS, scan_start: Option<u64>) -> Vec<BtDevice> {
    let mut di: BLUETOOTH_DEVICE_INFO = std::mem::zeroed();
    di.dwSize = std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as DWORD;
    let mut out: Vec<BtDevice> = Vec::new();
    let h_find = BluetoothFindFirstDevice(sp, &mut di);
    if h_find.is_null() {
        CloseHandle(sp.hRadio);
        return out; // ERROR_NO_MORE_ITEMS 等：查询完成但没有设备
    }
    loop {
        let connected = di.fConnected != 0;
        let heard_now = scan_start
            .and_then(|t0| systime_to_unix_ms(&di.stLastSeen).map(|t| t >= t0))
            .unwrap_or(false);
        out.push(BtDevice {
            name: from_wide(&di.szName),
            address: format!("{:012x}", di.Address),
            connected,
            in_range: connected || heard_now,
            paired: di.fAuthenticated != 0 || di.fRemembered != 0,
        });
        if BluetoothFindNextDevice(h_find, &mut di) == 0 {
            break;
        }
    }
    BluetoothFindDeviceClose(h_find);
    CloseHandle(sp.hRadio);
    out
}

/// 经典蓝牙查询（inquiry）：真实无线扫描。耗时约 timeout_mult × 1.28 秒。
/// 返回发现的全部设备（含配对缓存条目，用 in_range 区分真实在场）。
pub fn inquiry(timeout_mult: u32) -> Result<Vec<BtDevice>, String> {
    unsafe {
        let h_radio = open_radio()?;
        let scan_start = Some(now_unix_ms().saturating_sub(SEEN_GRACE_MS));
        let mut sp: BLUETOOTH_DEVICE_SEARCH_PARAMS = std::mem::zeroed();
        sp.dwSize = std::mem::size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as DWORD;
        sp.fReturnAuthenticated = 1;
        sp.fReturnRemembered = 1;
        sp.fReturnUnknown = 1;
        sp.fReturnConnected = 1;
        sp.fIssueInquiry = 1;
        sp.cTimeoutMultiplier = timeout_mult.clamp(1, 48) as u8;
        sp.hRadio = h_radio;
        Ok(find_devices(&sp, scan_start))
    }
}

/// 经典蓝牙缓存查询（不发起无线扫描，瞬时返回）：已配对/记住/已连接设备。
/// 用于补全 BLE 配对条目的名称、检查经典连接状态。
pub fn classic_cache() -> Vec<BtDevice> {
    unsafe {
        let Ok(h_radio) = open_radio() else { return Vec::new() };
        let mut sp: BLUETOOTH_DEVICE_SEARCH_PARAMS = std::mem::zeroed();
        sp.dwSize = std::mem::size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as DWORD;
        sp.fReturnAuthenticated = 1;
        sp.fReturnRemembered = 1;
        sp.fReturnUnknown = 0;
        sp.fReturnConnected = 1;
        sp.fIssueInquiry = 0;
        sp.cTimeoutMultiplier = 1;
        sp.hRadio = h_radio;
        find_devices(&sp, None)
    }
}

/// 目标设备是否有经典蓝牙档案处于连接状态（音频/网络共享等）
pub fn classic_connected(matcher: &str) -> bool {
    classic_cache()
        .iter()
        .any(|d| d.connected && device_matches(&d.name, &d.address, matcher))
}

/* ===================== 通道①：系统维持连接（已配对 BLE 设备） ===================== */

/// 已建立的系统维持连接（session/订阅对象必须存活，维持才持续生效）
pub struct BtLink {
    device: AgileReference<BluetoothLEDevice>,
    _session: AgileReference<GattSession>,
    /// 电池服务通知订阅（存在时强制系统维持 GATT 连接）
    _notify: Option<AgileReference<windows::Devices::Bluetooth::GenericAttributeProfile::GattCharacteristic>>,
    pub name: String,
    pub address: String,
}

static LINK: OnceLock<Mutex<Option<(String, BtLink)>>> = OnceLock::new();

fn fe(v: impl std::fmt::Display) -> String {
    format!("Windows 蓝牙 API 调用失败：{v}")
}

/// 枚举本机已配对的 BLE 设备（名称、MAC、原始 Id）。
/// BLE 条目名称为空时（Android 手机常见），用经典蓝牙缓存按 MAC 补全。
pub fn paired_ble_devices() -> Result<Vec<(String, String, String)>, String> {
    let selector = BluetoothLEDevice::GetDeviceSelectorFromPairingState(true).map_err(fe)?;
    let coll = DeviceInformation::FindAllAsyncAqsFilter(&selector)
        .map_err(fe)?
        .get()
        .map_err(fe)?;
    let n = coll.Size().map_err(fe)?;
    let classic = classic_cache();
    let mut out = Vec::new();
    for i in 0..n {
        let info = coll.GetAt(i).map_err(fe)?;
        let mut name = info.Name().map_err(fe)?.to_string();
        let id = info.Id().map_err(fe)?.to_string();
        let addr = parse_le_id(&id).unwrap_or_default();
        if name.is_empty() {
            if let Some(c) = classic.iter().find(|d| d.address == addr) {
                name = c.name.clone();
            }
        }
        out.push((name, addr, id));
    }
    Ok(out)
}

/// 尽力订阅电池服务通知（强制系统维持 GATT 连接）；失败不影响主流程
fn try_subscribe_battery(dev: &BluetoothLEDevice) -> Option<AgileReference<windows::Devices::Bluetooth::GenericAttributeProfile::GattCharacteristic>> {
    use windows::Devices::Bluetooth::GenericAttributeProfile::GattCharacteristic;
    let svcs = dev.GetGattServicesAsync().ok()?.get().ok()?;
    if svcs.Status().ok()? != GattCommunicationStatus::Success {
        return None;
    }
    let view = svcs.Services().ok()?;
    let n = view.Size().ok()?;
    for i in 0..n {
        let Ok(svc) = view.GetAt(i) else { continue };
        let Ok(svc_uuid) = svc.Uuid() else { continue };
        if svc_uuid != BATTERY_SERVICE_GUID {
            continue;
        }
        let Ok(chars) = svc
            .GetCharacteristicsForUuidAsync(BATTERY_LEVEL_GUID)
            .ok()?
            .get()
        else {
            continue;
        };
        if chars.Status().ok()? != GattCommunicationStatus::Success {
            continue;
        }
        let cv = chars.Characteristics().ok()?;
        if cv.Size().ok()? == 0 {
            continue;
        }
        let ch: GattCharacteristic = cv.GetAt(0).ok()?;
        let _ = ch
            .WriteClientCharacteristicConfigurationDescriptorAsync(
                GattClientCharacteristicConfigurationDescriptorValue::Notify,
            )
            .ok()?
            .get();
        return AgileReference::new(&ch).ok();
    }
    None
}

/// 为匹配的已配对设备建立系统维持连接；未找到返回 None
pub fn prepare_link(matcher: &str) -> Result<Option<BtLink>, String> {
    for (name, addr, id) in paired_ble_devices()? {
        if device_matches(&name, &addr, matcher) {
            let dev = BluetoothLEDevice::FromIdAsync(&HSTRING::from(id))
                .map_err(fe)?
                .get()
                .map_err(fe)?;
            let notify = try_subscribe_battery(&dev);
            let devid = dev.BluetoothDeviceId().map_err(fe)?;
            let session = GattSession::FromDeviceIdAsync(&devid)
                .map_err(fe)?
                .get()
                .map_err(fe)?;
            session.SetMaintainConnection(true).map_err(fe)?;
            return Ok(Some(BtLink {
                device: AgileReference::new(&dev).map_err(fe)?,
                _session: AgileReference::new(&session).map_err(fe)?,
                _notify: notify,
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

/// 汇总扫描：经典缓存 + 已配对 BLE（含连接状态）+ inquiry 真实在场，按 MAC 去重合并
pub fn scan_all(timeout_mult: u32) -> Result<Vec<BtDevice>, String> {
    let mut out: Vec<BtDevice> = Vec::new();
    fn merge(out: &mut Vec<BtDevice>, d: BtDevice) {
        if d.address.is_empty() {
            return;
        }
        if let Some(x) = out.iter_mut().find(|x| x.address == d.address) {
            if x.name.is_empty() && !d.name.is_empty() {
                x.name = d.name.clone();
            }
            x.connected |= d.connected;
            x.paired |= d.paired;
            x.in_range |= d.in_range;
        } else {
            out.push(d);
        }
    }
    for d in classic_cache() {
        merge(&mut out, d);
    }
    if let Ok(pairs) = paired_ble_devices() {
        for (name, addr, id) in pairs {
            let connected = BluetoothLEDevice::FromIdAsync(&HSTRING::from(id))
                .map(|op| op.get().ok())
                .ok()
                .flatten()
                .and_then(|d| d.ConnectionStatus().ok())
                .map(|s| s == BluetoothConnectionStatus::Connected)
                .unwrap_or(false);
            merge(
                &mut out,
                BtDevice { name, address: addr, connected, in_range: true, paired: true },
            );
        }
    }
    if let Ok(devs) = inquiry(timeout_mult) {
        for d in devs {
            if d.in_range {
                merge(&mut out, d);
            }
        }
    }
    Ok(out)
}

/// 判断目标设备是否真实在附近。
/// 已配对：系统 BLE 连接 ∨ 经典档案连接；未配对：inquiry 真实听到。
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
            // 给系统时间建立/恢复连接：手机在范围内会自动连上
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
        if link_connected(link)? {
            return Ok(true);
        }
        // BLE 未连：经典档案在连接同样算在场
        drop(guard);
        return Ok(classic_connected(matcher));
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
        assert!(device_matches("", "a4ccb388f111", "a4ccb388f111"));
        assert!(device_matches("k6u", "a4ccb388f111", "k6u"));
        assert!(!device_matches("", "a4ccb388f111", "k6u")); // 空名称不参与名称匹配
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
    fn mac_colon_format() {
        assert_eq!(mac_colon("a4ccb388f111"), "a4:cc:b3:88:f1:11");
        assert_eq!(mac_colon(""), "");
    }

    #[test]
    fn systime_conversion() {
        // 2026-01-01 00:00:00 UTC = 1767225600 秒
        assert_eq!(
            systime_to_unix_ms(&make_systemtime(2026, 1, 1, 0, 0, 0)).unwrap(),
            1_767_225_600_000
        );
        // 2024-02-29 12:34:56 UTC（闰年）
        assert_eq!(
            systime_to_unix_ms(&make_systemtime(2024, 2, 29, 12, 34, 56)).unwrap(),
            1_709_210_096_000
        );
        let now = now_unix_ms();
        assert!(now > 1_700_000_000_000);
    }

    /// 冒烟：扫描/缓存/汇总（本机有适配器则返回数据，无则优雅降级）
    #[test]
    fn scan_smoke() {
        match inquiry(1) {
            Ok(devs) => println!("inquiry：{} 个条目，在场 {} 个", devs.len(), devs.iter().filter(|d| d.in_range).count()),
            Err(e) => println!("inquiry 不可用：{e}"),
        }
        println!("classic_cache：{} 个条目", classic_cache().len());
        match scan_all(1) {
            Ok(devs) => println!("scan_all：{} 个设备（已配对 {} 个）", devs.len(), devs.iter().filter(|d| d.paired).count()),
            Err(e) => println!("scan_all 不可用：{e}"),
        }
    }
}
