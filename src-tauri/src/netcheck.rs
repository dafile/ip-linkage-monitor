use std::os::windows::process::CommandExt;
use std::process::Command;

/// 隐藏 ping 命令的控制台窗口
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 通过系统 ping 命令检测 IP 在线状态。
/// 仅当命令成功且输出中包含 TTL= 时判定在线，
/// 避免「目标主机不可达」等回复被误判为在线（该回复不含 TTL）。
pub fn ping_once(ip: &str, timeout_ms: u32) -> bool {
    let ip = ip.trim();
    if ip.is_empty() {
        return false;
    }
    let out = Command::new("ping")
        .args(["-n", "1", "-w", &timeout_ms.to_string(), ip])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) => {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout).to_lowercase().contains("ttl=")
        }
        Err(_) => false,
    }
}
