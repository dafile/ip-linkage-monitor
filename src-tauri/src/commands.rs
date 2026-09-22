use crate::config::{Config, LinkageRule, RuleAction, ACT_RUN};
use crate::logger::{self, LogEntry, LogFilter, CAT_CONFIG, CAT_USER, INFO, WARN};
use crate::monitor;
use crate::{bluetooth, netcheck, process, registry};
use crate::state::AppState;
use crate::tray::TrayState;
use chrono::Local;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;
use tauri::{AppHandle, Manager, State};

fn valid_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.trim().split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.chars().all(|c| c.is_ascii_digit())
                && p.parse::<u8>().is_ok()
        })
}

fn fmt_sec(s: u32) -> String {
    if s < 60 {
        format!("{s} 秒")
    } else if s % 60 == 0 {
        format!("{} 分钟", s / 60)
    } else {
        format!("{} 分 {} 秒", s / 60, s % 60)
    }
}

fn rule_title(note: &str, action: &RuleAction) -> String {
    if note.trim().is_empty() {
        action.desc()
    } else {
        note.trim().to_string()
    }
}

/// 校验并规范化配置（set_config 与 import_config 共用）
fn validate_and_normalize(cfg: &mut Config) -> Result<(), String> {
    if !cfg.monitor_ip.trim().is_empty() && !valid_ipv4(&cfg.monitor_ip) {
        return Err(format!("IP 地址格式不正确：{}", cfg.monitor_ip));
    }
    cfg.monitor_ip = cfg.monitor_ip.trim().to_string();
    cfg.program_path = cfg.program_path.trim().trim_matches('"').to_string();
    cfg.poll_interval_sec = cfg.poll_interval_sec.clamp(1, 60);
    cfg.ping_timeout_ms = cfg.ping_timeout_ms.clamp(200, 10000);
    if cfg.monitor_mode != "bluetooth" {
        cfg.monitor_mode = "ip".to_string();
    }
    cfg.bt_device = cfg.bt_device.trim().to_string();
    cfg.bt_scan_timeout_mult = cfg.bt_scan_timeout_mult.clamp(1, 48);
    let mut seen_ids: HashSet<u32> = HashSet::new();
    for r in &mut cfg.rules {
        if !matches!(r.trigger.as_str(), "online" | "offline") {
            return Err(format!("规则「{}」的触发时机无效：{}", rule_title(&r.note, &r.action), r.trigger));
        }
        if !r.action.valid_kind() {
            return Err(format!("规则「{}」的动作类型无效：{}", rule_title(&r.note, &r.action), r.action.kind));
        }
        if r.action.kind == ACT_RUN {
            r.action.path = r.action.path.trim().trim_matches('"').to_string();
        }
        r.delay_sec = r.delay_sec.min(86400);
        r.note = r.note.trim().to_string();
        if !seen_ids.insert(r.id) {
            return Err(format!("规则 ID 重复：{}", r.id));
        }
    }
    Ok(())
}

fn sync_tray_linkage(app: &AppHandle, enabled: bool) {
    if let Some(ts) = app.try_state::<TrayState>() {
        if let Some(item) = ts.link_item.lock().unwrap().as_ref() {
            let _ = item.set_checked(enabled);
        }
    }
}

#[tauri::command]
pub fn get_config(state: State<'_, Arc<AppState>>) -> Config {
    state.config_snapshot()
}

/// 保存基础配置（IP/路径/间隔/托盘等整体字段；规则以逐条命令为准）
#[tauri::command]
pub fn set_config(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    config: Config,
) -> Result<Config, String> {
    let mut new_cfg = config;
    validate_and_normalize(&mut new_cfg)?;

    let mut changes: Vec<String> = Vec::new();
    let linkage_turned_off;
    let linkage_changed_to;
    {
        let mut old = state.config.lock().unwrap();
        if old.program_path != new_cfg.program_path {
            changes.push(format!("监控程序路径 → {}", new_cfg.program_path));
        }
        if old.monitor_ip != new_cfg.monitor_ip {
            changes.push(format!(
                "监控 IP {} → {}",
                if old.monitor_ip.is_empty() { "（空）" } else { &old.monitor_ip },
                if new_cfg.monitor_ip.is_empty() { "（空）" } else { &new_cfg.monitor_ip }
            ));
        }
        if old.monitor_mode != new_cfg.monitor_mode {
            changes.push(format!(
                "监控方式 → {}",
                if new_cfg.monitor_mode == "bluetooth" { "蓝牙邻近监控" } else { "IP Ping 监控" }
            ));
        }
        if old.bt_device != new_cfg.bt_device {
            changes.push(format!(
                "监控蓝牙设备 {} → {}",
                if old.bt_device.is_empty() { "（空）" } else { &old.bt_device },
                if new_cfg.bt_device.is_empty() { "（空）" } else { &new_cfg.bt_device }
            ));
        }
        if old.linkage_enabled != new_cfg.linkage_enabled {
            changes.push(format!(
                "联动开关 {} → {}",
                if old.linkage_enabled { "开" } else { "关" },
                if new_cfg.linkage_enabled { "开" } else { "关" }
            ));
        }
        if old.close_to_tray != new_cfg.close_to_tray {
            changes.push(format!(
                "关闭窗口行为 → {}",
                if new_cfg.close_to_tray { "最小化到托盘" } else { "退出程序" }
            ));
        }
        if old.launch_hidden != new_cfg.launch_hidden {
            changes.push(format!(
                "启动方式 → {}",
                if new_cfg.launch_hidden {
                    "后台运行（隐藏窗口）"
                } else {
                    "正常显示窗口"
                }
            ));
        }
        if old.poll_interval_sec != new_cfg.poll_interval_sec {
            changes.push(format!(
                "检测间隔 {} → {}",
                fmt_sec(old.poll_interval_sec),
                fmt_sec(new_cfg.poll_interval_sec)
            ));
        }
        if old.ping_timeout_ms != new_cfg.ping_timeout_ms {
            changes.push(format!(
                "Ping 超时 {}ms → {}ms",
                old.ping_timeout_ms, new_cfg.ping_timeout_ms
            ));
        }
        if old.rules != new_cfg.rules {
            let total = new_cfg.rules.len();
            let enabled = new_cfg.rules.iter().filter(|r| r.enabled).count();
            changes.push(format!(
                "联动规则整体更新（共 {total} 条，启用 {enabled} 条）"
            ));
        }
        linkage_turned_off = old.linkage_enabled && !new_cfg.linkage_enabled;
        linkage_changed_to = if old.linkage_enabled != new_cfg.linkage_enabled {
            Some(new_cfg.linkage_enabled)
        } else {
            None
        };
        *old = new_cfg.clone();
    }
    state.save_config()?;

    if linkage_turned_off {
        let cancelled = monitor::cancel_pending(&state, None);
        for p in &cancelled {
            logger::log(
                INFO,
                CAT_CONFIG,
                &format!("联动已关闭，取消待执行任务「{}」（{}）", rule_title(&p.note, &p.action), p.action.desc()),
            );
        }
    }
    if let Some(v) = linkage_changed_to {
        sync_tray_linkage(&app, v);
    }
    for c in &changes {
        logger::log(INFO, CAT_CONFIG, c);
    }
    if !changes.is_empty()
        && !new_cfg.program_path.is_empty()
        && !std::path::Path::new(&new_cfg.program_path).is_file()
    {
        logger::log(
            WARN,
            CAT_CONFIG,
            &format!("注意：配置的程序文件不存在 {}", new_cfg.program_path),
        );
    }
    Ok(new_cfg)
}

/* ===================== 联动规则：逐条原子操作 ===================== */

fn next_rule_id(rules: &[LinkageRule]) -> u32 {
    rules.iter().map(|r| r.id).max().unwrap_or(0) + 1
}

/// 规范化单条规则字段（新增/修改共用）
fn normalize_rule(rule: &mut LinkageRule) -> Result<(), String> {
    if !matches!(rule.trigger.as_str(), "online" | "offline") {
        return Err(format!("触发时机无效：{}", rule.trigger));
    }
    if !rule.action.valid_kind() {
        return Err(format!("动作类型无效：{}", rule.action.kind));
    }
    if rule.action.kind == ACT_RUN {
        rule.action.path = rule.action.path.trim().trim_matches('"').to_string();
    }
    rule.delay_sec = rule.delay_sec.min(86400);
    rule.note = rule.note.trim().to_string();
    Ok(())
}

#[tauri::command]
pub fn add_rule(state: State<'_, Arc<AppState>>, mut rule: LinkageRule) -> Result<Config, String> {
    normalize_rule(&mut rule)?;
    let log_msg = {
        let mut cfg = state.config.lock().unwrap();
        rule.id = next_rule_id(&cfg.rules);
        let msg = format!(
            "新增联动规则 #{}「{}」（{}后{}，{}）",
            rule.id,
            rule_title(&rule.note, &rule.action),
            if rule.trigger == "online" { "上线" } else { "离线" },
            fmt_sec(rule.delay_sec),
            rule.action.desc()
        );
        cfg.rules.push(rule);
        msg
    };
    state.save_config()?;
    logger::log(INFO, CAT_CONFIG, &log_msg);
    Ok(state.config_snapshot())
}

#[tauri::command]
pub fn update_rule(
    state: State<'_, Arc<AppState>>,
    mut rule: LinkageRule,
) -> Result<Config, String> {
    normalize_rule(&mut rule)?;
    let log_msg = {
        let mut cfg = state.config.lock().unwrap();
        match cfg.rules.iter_mut().find(|r| r.id == rule.id) {
            Some(slot) => {
                let msg = format!(
                    "修改联动规则 #{}「{}」（{}后{}，{}）",
                    rule.id,
                    rule_title(&rule.note, &rule.action),
                    if rule.trigger == "online" { "上线" } else { "离线" },
                    fmt_sec(rule.delay_sec),
                    rule.action.desc()
                );
                *slot = rule;
                msg
            }
            None => return Err(format!("规则 {} 不存在（可能已被删除）", rule.id)),
        }
    };
    state.save_config()?;
    logger::log(INFO, CAT_CONFIG, &log_msg);
    Ok(state.config_snapshot())
}

#[tauri::command]
pub fn delete_rule(state: State<'_, Arc<AppState>>, id: u32) -> Result<Config, String> {
    let removed = {
        let mut cfg = state.config.lock().unwrap();
        let pos = cfg
            .rules
            .iter()
            .position(|r| r.id == id)
            .ok_or(format!("规则 {id} 不存在"))?;
        let r = cfg.rules.remove(pos);
        (rule_title(&r.note, &r.action), r.action.desc())
    };
    let cancelled = monitor::cancel_pending(&state, Some(id));
    state.save_config()?;
    logger::log(
        INFO,
        CAT_CONFIG,
        &format!("删除联动规则 #{id}「{}」（{}）", removed.0, removed.1),
    );
    for p in &cancelled {
        logger::log(
            INFO,
            CAT_CONFIG,
            &format!("已取消其待执行任务「{}」（{}）", rule_title(&p.note, &p.action), p.action.desc()),
        );
    }
    Ok(state.config_snapshot())
}

#[tauri::command]
pub fn toggle_rule(
    state: State<'_, Arc<AppState>>,
    id: u32,
    enabled: bool,
) -> Result<Config, String> {
    let title = {
        let mut cfg = state.config.lock().unwrap();
        match cfg.rules.iter_mut().find(|r| r.id == id) {
            Some(r) => {
                r.enabled = enabled;
                rule_title(&r.note, &r.action)
            }
            None => return Err(format!("规则 {id} 不存在")),
        }
    };
    if !enabled {
        let cancelled = monitor::cancel_pending(&state, Some(id));
        for p in &cancelled {
            logger::log(
                INFO,
                CAT_CONFIG,
                &format!("规则已停用，取消待执行任务「{}」（{}）", rule_title(&p.note, &p.action), p.action.desc()),
            );
        }
    }
    state.save_config()?;
    logger::log(
        INFO,
        CAT_CONFIG,
        &format!(
            "联动规则 #{id}「{title}」已{}",
            if enabled { "启用" } else { "停用" }
        ),
    );
    Ok(state.config_snapshot())
}

#[tauri::command]
pub fn reset_rules(state: State<'_, Arc<AppState>>) -> Result<Config, String> {
    let count;
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.rules = Config::default_rules();
        cfg.rules_seeded = true;
        count = cfg.rules.len();
    }
    // 默认规则方向与旧规则可能不同，取消全部待执行任务
    monitor::cancel_pending(&state, None);
    state.save_config()?;
    logger::log(INFO, CAT_CONFIG, &format!("已恢复默认联动规则（{count} 条）"));
    Ok(state.config_snapshot())
}

/* ===================== 其他命令 ===================== */

#[tauri::command]
pub fn get_status(state: State<'_, Arc<AppState>>) -> monitor::Status {
    monitor::build_status(&state)
}

/// 取消待执行的联动任务；rule_id 为空时取消全部
#[tauri::command]
pub fn cancel_pending(
    state: State<'_, Arc<AppState>>,
    rule_id: Option<u32>,
) -> Result<usize, String> {
    let cancelled = monitor::cancel_pending(&state, rule_id);
    for p in &cancelled {
        logger::log(
            INFO,
            CAT_USER,
            &format!(
                "手动取消联动任务「{}」（{}，原定 {}）",
                rule_title(&p.note, &p.action),
                p.action.desc(),
                p.due_ts
            ),
        );
    }
    Ok(cancelled.len())
}

#[tauri::command]
pub fn read_registry_path() -> Result<String, String> {
    registry::read_program_path()
}

/// 选择程序或脚本（exe/bat/cmd/ps1）
#[tauri::command]
pub fn browse_program_path() -> Option<String> {
    rfd::FileDialog::new()
        .set_title("选择程序或脚本")
        .add_filter("程序与脚本 (*.exe;*.bat;*.cmd;*.ps1)", &["exe", "bat", "cmd", "ps1"])
        .add_filter("所有文件 (*.*)", &["*"])
        .pick_file()
        .map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
pub async fn manual_start(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let st = state.inner().clone();
    logger::log(INFO, CAT_USER, "手动操作：启动监控程序");
    tauri::async_runtime::spawn_blocking(move || monitor::do_start(&st))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn manual_stop(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let st = state.inner().clone();
    logger::log(INFO, CAT_USER, "手动操作：彻底关闭监控程序");
    tauri::async_runtime::spawn_blocking(move || {
        monitor::do_stop(&st);
        Ok::<(), String>(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub fn show_program_window(state: State<'_, Arc<AppState>>) -> Result<usize, String> {
    let cfg = state.config_snapshot();
    if cfg.program_path.trim().is_empty() {
        return Err("监控程序路径未配置".into());
    }
    let n = process::show_windows(&cfg.program_path);
    if n == 0 {
        if !process::is_exe_running(&cfg.program_path) {
            logger::log(WARN, CAT_USER, "显示窗口失败：监控程序未在运行");
            Err("监控程序未在运行，请先启动".into())
        } else {
            logger::log(WARN, CAT_USER, "显示窗口：未找到可显示的主窗口");
            Err("未找到可显示的窗口（程序可能以托盘方式运行）".into())
        }
    } else {
        logger::log(INFO, CAT_USER, &format!("已显示监控程序窗口（{n} 个）"));
        Ok(n)
    }
}

#[tauri::command]
pub fn hide_program_window(state: State<'_, Arc<AppState>>) -> Result<usize, String> {
    let cfg = state.config_snapshot();
    if cfg.program_path.trim().is_empty() {
        return Err("监控程序路径未配置".into());
    }
    if !process::is_exe_running(&cfg.program_path) {
        return Err("监控程序未在运行".into());
    }
    let n = process::hide_windows(&cfg.program_path);
    if n == 0 {
        logger::log(INFO, CAT_USER, "监控程序当前没有可见窗口");
    } else {
        logger::log(INFO, CAT_USER, &format!("已隐藏监控程序窗口（{n} 个）"));
    }
    Ok(n)
}

#[derive(serde::Serialize)]
pub struct TestIpResult {
    pub online: bool,
    pub latency_ms: u64,
}

#[tauri::command]
pub async fn test_ip(ip: String, timeout_ms: u32) -> Result<TestIpResult, String> {
    if !valid_ipv4(&ip) {
        return Err(format!("IP 地址格式不正确：{ip}"));
    }
    let timeout = timeout_ms.clamp(200, 10000);
    let online = tauri::async_runtime::spawn_blocking(move || netcheck::ping_once(&ip, timeout))
        .await
        .map_err(|e| e.to_string())?;
    let latency_ms = 0; // 由前端自行计时展示
    Ok(TestIpResult { online, latency_ms })
}

/// 立即试运行一个动作（直接使用前端传来的动作定义，不依赖已保存的规则状态）
#[tauri::command]
pub async fn run_action(
    state: State<'_, Arc<AppState>>,
    action: RuleAction,
    note: String,
) -> Result<String, String> {
    let st = state.inner().clone();
    let title = if note.trim().is_empty() {
        action.desc()
    } else {
        note.trim().to_string()
    };
    logger::log(INFO, CAT_USER, &format!("试运行「{title}」：{}", action.desc()));
    tauri::async_runtime::spawn_blocking(move || monitor::execute_action(&st, &action))
        .await
        .map_err(|e| e.to_string())?
}

/* ===================== 检测测试（IP / 蓝牙） ===================== */

#[derive(serde::Serialize)]
pub struct MonitorTestResult {
    pub online: bool,
    pub latency_ms: u64,
    pub mode: String,
}

/// 控制台「立即检测」：按当前监控方式执行一次检测
#[tauri::command]
pub async fn test_monitor(state: State<'_, Arc<AppState>>) -> Result<MonitorTestResult, String> {
    let cfg = state.config_snapshot();
    if cfg.monitor_mode == "bluetooth" {
        if cfg.bt_device.trim().is_empty() {
            return Err("未配置蓝牙设备（名称或 MAC）".into());
        }
        let dev = cfg.bt_device.trim().to_string();
        let mult = cfg.bt_scan_timeout_mult.clamp(1, 48);
        logger::log(INFO, CAT_USER, &format!("手动立即检测：蓝牙扫描「{dev}」…"));
        let t0 = Instant::now();
        let online =
            tauri::async_runtime::spawn_blocking(move || bluetooth::presence(&dev, mult))
                .await
                .map_err(|e| e.to_string())??;
        let ms = t0.elapsed().as_millis() as u64;
        logger::log(
            INFO,
            CAT_USER,
            &format!("手动蓝牙检测结果：{}（{ms} ms）", if online { "在线" } else { "未发现" }),
        );
        Ok(MonitorTestResult { online, latency_ms: ms, mode: "bluetooth".into() })
    } else {
        if cfg.monitor_ip.trim().is_empty() {
            return Err("未配置监控 IP".into());
        }
        let ip = cfg.monitor_ip.trim().to_string();
        let timeout = cfg.ping_timeout_ms;
        logger::log(INFO, CAT_USER, &format!("手动立即检测：IP {ip}…"));
        let t0 = Instant::now();
        let online = tauri::async_runtime::spawn_blocking(move || netcheck::ping_once(&ip, timeout))
            .await
            .map_err(|e| e.to_string())?;
        let ms = t0.elapsed().as_millis() as u64;
        Ok(MonitorTestResult { online, latency_ms: ms, mode: "ip".into() })
    }
}

#[derive(serde::Serialize)]
pub struct BtTestResult {
    pub online: bool,
    pub device_count: usize,
    pub duration_ms: u64,
    pub matched: Option<String>,
}

/// 设置页「测试蓝牙检测」：扫描并列出是否找到目标
#[tauri::command]
pub async fn test_bluetooth(device: String, timeout_mult: u32) -> Result<BtTestResult, String> {
    let dev = device.trim().to_string();
    if dev.is_empty() {
        return Err("请先填写设备名称或 MAC".into());
    }
    let mult = timeout_mult.clamp(1, 48);
    logger::log(INFO, CAT_USER, &format!("测试蓝牙检测：「{dev}」…"));
    let t0 = Instant::now();
    let devs = tauri::async_runtime::spawn_blocking(move || bluetooth::inquiry(mult))
        .await
        .map_err(|e| e.to_string())??;
    let duration = t0.elapsed().as_millis() as u64;
    let in_range_count = devs.iter().filter(|d| d.in_range).count();
    let matched = devs
        .iter()
        .find(|d| d.in_range && bluetooth::device_matches(&d.name, &d.address, &dev));
    logger::log(
        INFO,
        CAT_USER,
        &format!(
            "蓝牙检测结果：{}（在场 {} 个 / 缓存 {} 个，{duration} ms）",
            if matched.is_some() { "找到目标" } else { "未找到目标" },
            in_range_count,
            devs.len() - in_range_count
        ),
    );
    Ok(BtTestResult {
        online: matched.is_some(),
        device_count: in_range_count,
        duration_ms: duration,
        matched: matched.map(|d| if d.name.is_empty() { d.address.clone() } else { d.name.clone() }),
    })
}

#[derive(serde::Serialize)]
pub struct BtScanList {
    pub devices: Vec<bluetooth::BtDevice>,
    pub duration_ms: u64,
}

/// 扫描附近蓝牙设备（供前端拾取器选择目标）
#[tauri::command]
pub async fn bt_scan_devices(timeout_mult: u32) -> Result<BtScanList, String> {
    let mult = timeout_mult.clamp(1, 48);
    let t0 = Instant::now();
    let devices = tauri::async_runtime::spawn_blocking(move || bluetooth::inquiry(mult))
        .await
        .map_err(|e| e.to_string())??;
    Ok(BtScanList { devices, duration_ms: t0.elapsed().as_millis() as u64 })
}

/* ===================== 配置文件导入/导出 ===================== */

#[tauri::command]
pub fn get_config_path(state: State<'_, Arc<AppState>>) -> String {
    state.config_path.display().to_string()
}

#[tauri::command]
pub fn export_config(state: State<'_, Arc<AppState>>) -> Result<Option<String>, String> {
    let name = format!("监控配置-{}.json", Local::now().format("%Y%m%d_%H%M%S"));
    let Some(target) = rfd::FileDialog::new()
        .set_title("导出配置")
        .set_file_name(&name)
        .add_filter("配置文件 (*.json)", &["json"])
        .save_file()
    else {
        return Err("已取消导出".into());
    };
    let cfg = state.config_snapshot();
    let json = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    std::fs::write(&target, json.as_bytes()).map_err(|e| format!("写入文件失败：{e}"))?;
    logger::log(INFO, CAT_USER, &format!("导出配置 → {}", target.display()));
    Ok(Some(target.to_string_lossy().into_owned()))
}

#[tauri::command]
pub fn import_config(app: AppHandle, state: State<'_, Arc<AppState>>) -> Result<Option<Config>, String> {
    let Some(path) = rfd::FileDialog::new()
        .set_title("导入配置")
        .add_filter("配置文件 (*.json)", &["json"])
        .add_filter("所有文件 (*.*)", &["*"])
        .pick_file()
    else {
        return Err("已取消导入".into());
    };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("读取文件失败：{e}"))?;
    let mut cfg: Config =
        serde_json::from_str(&text).map_err(|e| format!("配置文件格式不正确：{e}"))?;
    validate_and_normalize(&mut cfg)?;

    {
        let mut old = state.config.lock().unwrap();
        *old = cfg.clone();
    }
    // 导入的规则可能与排期中的任务不一致，全部取消
    let cancelled = monitor::cancel_pending(&state, None);
    state.save_config()?;
    sync_tray_linkage(&app, cfg.linkage_enabled);
    logger::log(
        INFO,
        CAT_USER,
        &format!(
            "导入配置 ← {}（{} 条联动规则，已取消 {} 个待执行任务）",
            path.display(),
            cfg.rules.len(),
            cancelled.len()
        ),
    );
    Ok(Some(cfg))
}

/* ===================== 日志 ===================== */

#[tauri::command]
pub fn get_logs(state: State<'_, Arc<AppState>>, filter: LogFilter) -> Vec<LogEntry> {
    logger::query(&state.logs_dir, &filter)
}

#[derive(serde::Serialize)]
pub struct ExportResult {
    pub count: usize,
    pub path: String,
}

#[tauri::command]
pub fn export_logs(
    state: State<'_, Arc<AppState>>,
    filter: LogFilter,
) -> Result<ExportResult, String> {
    let default_name = format!("日志导出-{}.csv", Local::now().format("%Y%m%d_%H%M%S"));
    let Some(target) = rfd::FileDialog::new()
        .set_title("导出日志")
        .set_file_name(&default_name)
        .add_filter("CSV 文件 (*.csv)", &["csv"])
        .save_file()
    else {
        return Err("已取消导出".into());
    };
    let count = logger::export_csv(&state.logs_dir, &filter, &target)?;
    logger::log(
        INFO,
        CAT_USER,
        &format!("导出日志：{count} 条 → {}", target.display()),
    );
    Ok(ExportResult {
        count,
        path: target.to_string_lossy().into_owned(),
    })
}

#[tauri::command]
pub fn clear_logs(state: State<'_, Arc<AppState>>) -> Result<usize, String> {
    let n = logger::clear(&state.logs_dir)?;
    logger::log(WARN, CAT_USER, &format!("清空全部日志文件（{n} 个）"));
    Ok(n)
}
