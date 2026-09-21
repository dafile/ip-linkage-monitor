use std::path::Path;
use winreg::enums::HKEY_LOCAL_MACHINE;
use winreg::RegKey;

/// 默认注册表键：掌上看家采集端（Inno Setup 安装信息）
pub const DEFAULT_REG_SUBKEY: &str =
    r"SOFTWARE\Wow6432Node\Microsoft\Windows\CurrentVersion\Uninstall\{B659A0AE-7339-41DF-A7BA-81EBEBF91321}_is1";

/// 排除明显不是主程序的 exe
const EXE_BLACKLIST: [&str; 7] = ["unins", "setup", "update", "loader", "crash", "report", "helper"];
/// 多个候选时的偏好关键词（按优先级排序）
const EXE_PREFER: [&str; 6] = ["streamer", "athome", "camera", "monitor", "viewer", "avs"];

/// 从注册表读取监控程序主程序路径：
/// 1. DisplayIcon（若指向存在的 exe）
/// 2. InstallLocation / Inno Setup: App Path 目录下的主程序 exe
pub fn read_program_path() -> Result<String, String> {
    let hk = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hk
        .open_subkey(DEFAULT_REG_SUBKEY)
        .map_err(|e| format!("无法打开注册表键（监控程序可能未安装）：{e}"))?;

    if let Ok(di) = key.get_value::<String, _>("DisplayIcon") {
        let p = parse_display_icon(&di);
        if is_exe(&p) && Path::new(&p).is_file() {
            return Ok(p);
        }
    }

    let loc: String = key
        .get_value("InstallLocation")
        .ok()
        .filter(|s: &String| !s.trim().is_empty())
        .or_else(|| key.get_value("Inno Setup: App Path").ok())
        .unwrap_or_default();
    let loc = loc.trim().trim_matches('"').to_string();
    if !loc.is_empty() && Path::new(&loc).is_dir() {
        if let Some(pick) = pick_main_exe(&loc) {
            return Ok(pick);
        }
    }
    Err("注册表中未找到有效的监控程序路径，请手动选择".to_string())
}

/// DisplayIcon 形如 `C:\...\app.exe,0` 或 `"C:\Program Files\...\app.exe",0`
fn parse_display_icon(s: &str) -> String {
    let s = s.trim();
    if let Some(start) = s.find('"') {
        if let Some(off) = s[start + 1..].find('"') {
            return s[start + 1..start + 1 + off].to_string();
        }
    }
    s.split(',').next().unwrap_or("").trim().trim_matches('"').to_string()
}

fn is_exe(p: &str) -> bool {
    Path::new(p)
        .extension()
        .map(|e| e.eq_ignore_ascii_case("exe"))
        .unwrap_or(false)
}

/// 在安装目录中挑选主程序 exe：唯一则直接返回；
/// 多个则按偏好关键词打分（其次比较文件大小）选最优。
fn pick_main_exe(dir: &str) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    let rd = std::fs::read_dir(dir).ok()?;
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_file() || !is_exe(&p.to_string_lossy()) {
            continue;
        }
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if EXE_BLACKLIST.iter().any(|b| name.contains(b)) {
            continue;
        }
        candidates.push(p.to_string_lossy().into_owned());
    }
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        return Some(candidates.remove(0));
    }
    let mut best: Option<(i32, u64, String)> = None;
    for c in &candidates {
        let name = Path::new(c)
            .file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let score = EXE_PREFER
            .iter()
            .position(|k| name.contains(k))
            .map(|i| 100 - i as i32)
            .unwrap_or(0);
        let size = std::fs::metadata(c).map(|m| m.len()).unwrap_or(0);
        let better = best
            .as_ref()
            .map(|b: &(i32, u64, String)| (score, size) > (b.0, b.1))
            .unwrap_or(true);
        if better {
            best = Some((score, size, c.clone()));
        }
    }
    best.map(|b| b.2)
}
