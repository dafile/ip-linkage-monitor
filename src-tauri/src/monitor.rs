use crate::config::{
    Config, LinkageRule, RuleAction, ACT_HIDE, ACT_RUN, ACT_SHOW, ACT_START, TRIGGER_OFFLINE,
    TRIGGER_ONLINE,
};
use crate::logger::{self, CAT_MONITOR, ERROR, INFO, WARN};
use crate::netcheck;
use crate::process;
use crate::state::AppState;
use chrono::Local;
use serde::Serialize;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::AppHandle;

/// 监控线程心跳超时：超过该时长未跳动视为线程异常
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);
/// 防复活验证时长与复活强杀轮次上限
const STAY_DEAD_SECS: u64 = 15;
const MAX_RESPAWN_KILLS: u32 = 6;
/// 联动任务到期检查精度
const FIRE_CHECK_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub ip_online: Option<bool>,
    pub ip_last_check: Option<String>,
    pub latency_ms: Option<u64>,
    pub last_transition: Option<String>,
    pub program_running: bool,
    pub program_path_exists: bool,
    pub linkage_enabled: bool,
    pub pendings: Vec<PendingDto>,
    pub monitor_alive: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingDto {
    pub rule_id: u32,
    pub note: String,
    pub action_desc: String,
    pub due_ts: String,
    pub remaining_sec: u64,
}

/// 已排期的联动任务
pub struct PendingAction {
    pub rule_id: u32,
    pub note: String,
    pub action: RuleAction,
    /// 任务的触发条件（online/offline），IP 状态与之不符时任务作废
    pub trigger: String,
    pub fire_at: Instant,
    pub due_ts: String,
}

/// 监控线程的运行时状态（非持久化）
pub struct MonitorInner {
    pub last_tick: Instant,
    pub ip_online: Option<bool>,
    pub ip_last_check: Option<String>,
    pub latency_ms: Option<u64>,
    pub last_transition: Option<String>,
    pub pending: Vec<PendingAction>,
}

impl MonitorInner {
    pub fn new() -> Self {
        Self {
            last_tick: Instant::now(),
            ip_online: None,
            ip_last_check: None,
            latency_ms: None,
            last_transition: None,
            pending: Vec::new(),
        }
    }
}

fn now_ts() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

pub fn spawn_thread(_app: AppHandle, state: Arc<AppState>) {
    // 联动任务到期检查线程：独立于主循环，保证延迟精度
    {
        let st = state.clone();
        std::thread::spawn(move || loop {
            fire_pending(&st);
            std::thread::sleep(FIRE_CHECK_INTERVAL);
        });
    }
    std::thread::spawn(move || {
        logger::log(INFO, CAT_MONITOR, "IP 状态监控线程已启动");
        loop {
            let cfg = state.config_snapshot();

            if cfg.monitor_ip.trim().is_empty() {
                let mut m = state.monitor.lock().unwrap();
                m.last_tick = Instant::now();
                m.ip_online = None;
                m.ip_last_check = Some(now_ts());
                m.latency_ms = None;
                drop(m);
            } else {
                let t0 = Instant::now();
                let online = netcheck::ping_once(&cfg.monitor_ip, cfg.ping_timeout_ms);
                let latency = t0.elapsed().as_millis() as u64;

                let mut transition: Option<bool> = None;
                let mut first_result = false;
                {
                    let mut m = state.monitor.lock().unwrap();
                    m.last_tick = Instant::now();
                    m.ip_last_check = Some(now_ts());
                    m.latency_ms = Some(latency);
                    match m.ip_online {
                        Some(prev) if prev != online => transition = Some(online),
                        None => first_result = true,
                        _ => {}
                    }
                    m.ip_online = Some(online);
                }
                if first_result {
                    let word = if online { "在线" } else { "离线" };
                    logger::log(
                        INFO,
                        CAT_MONITOR,
                        &format!("初始检测完成：{} 当前{word}（{latency} ms）", cfg.monitor_ip),
                    );
                }
                if let Some(online) = transition {
                    on_transition(&state, &cfg, online);
                }
            }

            std::thread::sleep(Duration::from_secs(cfg.poll_interval_sec.clamp(1, 60) as u64));
        }
    });
}

fn fmt_delay_text(sec: u32) -> String {
    if sec == 0 {
        "立即".to_string()
    } else if sec < 60 {
        format!("{sec} 秒")
    } else if sec % 60 == 0 {
        format!("{} 分钟", sec / 60)
    } else {
        format!("{} 分 {} 秒", sec / 60, sec % 60)
    }
}

/// IP 状态变化处理：作废失效任务，按规则调度新任务
fn on_transition(state: &Arc<AppState>, cfg: &Config, online: bool) {
    let new_trigger = if online { TRIGGER_ONLINE } else { TRIGGER_OFFLINE };
    let word = if online { "上线" } else { "离线" };
    logger::log(
        INFO,
        CAT_MONITOR,
        &format!("IP 状态变化：{} 已{word}", cfg.monitor_ip),
    );

    // 状态反转后，触发条件与新状态不符的待执行任务一律作废
    {
        let mut m = state.monitor.lock().unwrap();
        m.last_transition = Some(now_ts());
        let all: Vec<PendingAction> = std::mem::take(&mut m.pending);
        let (stale, keep): (Vec<_>, Vec<_>) = all
            .into_iter()
            .partition(|p| p.trigger != new_trigger);
        m.pending = keep;
        for p in &stale {
            logger::log(
                INFO,
                CAT_MONITOR,
                &format!(
                    "IP 已{word}，作废待执行任务「{}」（{}）",
                    p.note,
                    p.action.desc()
                ),
            );
        }
    }

    if !cfg.linkage_enabled {
        return;
    }
    for rule in cfg.rules.iter().filter(|r| r.enabled && r.trigger == new_trigger) {
        schedule_rule(state, rule, word);
    }
}

fn schedule_rule(state: &AppState, rule: &LinkageRule, state_word: &str) {
    let mut m = state.monitor.lock().unwrap();
    if m.pending.iter().any(|p| p.rule_id == rule.id) {
        return; // 该规则已有任务在排队
    }
    let delay = rule.delay_sec;
    let due_ts = Local::now()
        .checked_add_signed(chrono::Duration::seconds(delay as i64))
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_else(now_ts);
    m.pending.push(PendingAction {
        rule_id: rule.id,
        note: rule.note.clone(),
        action: rule.action.clone(),
        trigger: rule.trigger.clone(),
        fire_at: Instant::now() + Duration::from_secs(delay as u64),
        due_ts: due_ts.clone(),
    });
    let when = if delay == 0 {
        "立即".to_string()
    } else {
        format!("{} 后", fmt_delay_text(delay))
    };
    logger::log(
        INFO,
        CAT_MONITOR,
        &format!(
            "联动规则「{}」：IP 已{}，{}{}（预定 {due_ts}）",
            rule.note,
            state_word,
            when,
            rule.action.desc()
        ),
    );
}

/// 到期执行联动任务
fn fire_pending(state: &Arc<AppState>) {
    let due: Vec<PendingAction> = {
        let mut m = state.monitor.lock().unwrap();
        let all = std::mem::take(&mut m.pending);
        let (due, keep): (Vec<_>, Vec<_>) = all
            .into_iter()
            .partition(|p| Instant::now() >= p.fire_at);
        m.pending = keep;
        due
    };
    for t in due {
        logger::log(
            INFO,
            CAT_MONITOR,
            &format!("联动规则「{}」到期：{}", t.note, t.action.desc()),
        );
        match execute_action(state, &t.action) {
            Ok(msg) => logger::log(
                INFO,
                CAT_MONITOR,
                &format!("规则「{}」执行成功：{msg}", t.note),
            ),
            Err(e) => logger::log(
                ERROR,
                CAT_MONITOR,
                &format!("规则「{}」执行失败：{e}", t.note),
            ),
        }
    }
}

/// 取消待执行任务：rule_id=None 取消全部，返回被取消的任务
pub fn cancel_pending(state: &AppState, rule_id: Option<u32>) -> Vec<PendingAction> {
    let mut m = state.monitor.lock().unwrap();
    let all = std::mem::take(&mut m.pending);
    let (cancelled, keep): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|p| rule_id.map(|id| p.rule_id == id).unwrap_or(true));
    m.pending = keep;
    cancelled
}

/// 执行一个规则动作，返回结果描述
pub fn execute_action(state: &AppState, action: &RuleAction) -> Result<String, String> {
    match action.kind.as_str() {
        ACT_START => {
            do_start(state)?;
            Ok("已启动监控程序".to_string())
        }
        crate::config::ACT_STOP => {
            do_stop(state);
            Ok("已彻底关闭监控程序".to_string())
        }
        ACT_SHOW => {
            let cfg = state.config_snapshot();
            if cfg.program_path.trim().is_empty() {
                return Err("监控程序路径未配置".into());
            }
            let n = process::show_windows(&cfg.program_path);
            if n == 0 {
                Err("未找到可显示的监控程序窗口".into())
            } else {
                Ok(format!("已显示 {n} 个窗口"))
            }
        }
        ACT_HIDE => {
            let cfg = state.config_snapshot();
            if cfg.program_path.trim().is_empty() {
                return Err("监控程序路径未配置".into());
            }
            let n = process::hide_windows(&cfg.program_path);
            if n == 0 {
                Err("监控程序当前没有可见窗口".into())
            } else {
                Ok(format!("已隐藏 {n} 个窗口"))
            }
        }
        ACT_RUN => {
            let pid = run_command(&action.path, &action.args, action.hidden)?;
            Ok(format!("已执行「{}」（PID {pid}）", action.path.trim()))
        }
        other => Err(format!("未知动作类型：{other}")),
    }
}

/// 分割命令行参数（支持双引号包裹含空格的参数）
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for ch in s.chars() {
        if ch == '"' {
            in_quote = !in_quote;
        } else if ch.is_whitespace() && !in_quote {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 执行自定义脚本/程序。按扩展名智能选择宿主：
/// .ps1 → powershell；.bat/.cmd → cmd；其余按可执行文件直接启动。
fn run_command(script: &str, args: &str, hidden: bool) -> Result<u32, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let path = script.trim();
    if path.is_empty() {
        return Err("未设置脚本或程序路径".into());
    }
    let p = Path::new(path);
    if !p.is_file() {
        return Err(format!("文件不存在：{path}"));
    }
    let argv = split_args(args);
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let (program, cmd_args): (&str, Vec<String>) = match ext.as_str() {
        "ps1" => (
            "powershell.exe",
            vec![
                "-NoProfile".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                path.into(),
            ]
            .into_iter()
            .chain(argv)
            .collect(),
        ),
        "bat" | "cmd" => ("cmd.exe", vec!["/c".into(), path.into()].into_iter().chain(argv).collect()),
        _ => (path, argv),
    };
    let mut c = Command::new(program);
    c.args(&cmd_args);
    if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
        c.current_dir(dir);
    }
    if hidden {
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let child = c.spawn().map_err(|e| format!("启动失败：{e}"))?;
    Ok(child.id())
}

pub fn build_status(state: &AppState) -> Status {
    let cfg = state.config_snapshot();
    let m = state.monitor.lock().unwrap();
    let path_exists = !cfg.program_path.trim().is_empty() && Path::new(&cfg.program_path).is_file();
    let program_running =
        path_exists && process::is_exe_running(cfg.program_path.trim_end_matches('"'));
    let pendings = m
        .pending
        .iter()
        .map(|p| PendingDto {
            rule_id: p.rule_id,
            note: p.note.clone(),
            action_desc: p.action.desc(),
            due_ts: p.due_ts.clone(),
            remaining_sec: (p.fire_at.duration_since(Instant::now()).as_secs() + 1).max(1),
        })
        .collect();
    Status {
        ip_online: m.ip_online,
        ip_last_check: m.ip_last_check.clone(),
        latency_ms: m.latency_ms,
        last_transition: m.last_transition.clone(),
        program_running,
        program_path_exists: path_exists,
        linkage_enabled: cfg.linkage_enabled,
        pendings,
        monitor_alive: m.last_tick.elapsed() < HEARTBEAT_TIMEOUT,
    }
}

/// 启动监控程序（幂等：已在运行时跳过）
pub fn do_start(state: &AppState) -> Result<(), String> {
    let cfg = state.config_snapshot();
    if cfg.program_path.trim().is_empty() {
        let e = "无法启动监控程序：程序路径未配置";
        logger::log(WARN, CAT_MONITOR, e);
        return Err(e.to_string());
    }
    if !Path::new(&cfg.program_path).is_file() {
        let e = format!("无法启动监控程序：文件不存在 {}", cfg.program_path);
        logger::log(ERROR, CAT_MONITOR, &e);
        return Err(e);
    }
    if process::is_exe_running(&cfg.program_path) {
        logger::log(INFO, CAT_MONITOR, "监控程序已在运行，跳过本次启动");
        return Ok(());
    }
    match process::spawn_program(&cfg.program_path, cfg.launch_hidden) {
        Ok(pid) => {
            let mode = if cfg.launch_hidden {
                "后台隐藏窗口"
            } else {
                "正常显示窗口"
            };
            logger::log(
                INFO,
                CAT_MONITOR,
                &format!("监控程序已启动（PID {pid}，{mode}）"),
            );
            if cfg.launch_hidden {
                rehide_later(cfg.program_path.clone());
            }
            Ok(())
        }
        Err(e) => {
            logger::log(ERROR, CAT_MONITOR, &format!("监控程序启动失败：{e}"));
            Err(format!("监控程序启动失败：{e}"))
        }
    }
}

/// 关闭监控程序：同时结束同目录守护进程（互拉守护），并做防复活验证。
///
/// 掌上看家采集端采用互拉守护架构（主程序 spawn AvsLoader，AvsLoader 再拉起主程序），
/// 只杀主程序会在数秒内被复活，因此必须：
/// 1. 同时结束主程序与全部守护进程，打破互拉；
/// 2. 持续监视一段时间，任何复活立即再次强制结束。
pub fn do_stop(state: &AppState) {
    let cfg = state.config_snapshot();
    if cfg.program_path.trim().is_empty() {
        logger::log(WARN, CAT_MONITOR, "无法关闭监控程序：程序路径未配置");
        return;
    }
    let exe = cfg.program_path.trim().to_string();

    let main_pids = process::find_pids_by_path(&exe);
    if main_pids.is_empty() {
        logger::log(INFO, CAT_MONITOR, "监控程序未在运行，无需关闭");
        return;
    }
    let pid_list = main_pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(", ");

    let guardians = process::find_guardians(&exe);
    if !guardians.is_empty() {
        let desc = guardians
            .iter()
            .map(|(n, p)| format!("{n}(PID {p})"))
            .collect::<Vec<_>>()
            .join("、");
        logger::log(
            INFO,
            CAT_MONITOR,
            &format!("发现同目录守护进程：{desc}，将一并结束以防自动重启"),
        );
    }
    logger::log(INFO, CAT_MONITOR, &format!("正在关闭监控程序（PID {pid_list}）…"));

    // 1) 礼貌请求主程序退出（WM_CLOSE），同时立即强杀守护进程
    process::post_wm_close(&main_pids);
    if !guardians.is_empty() {
        let gpids: Vec<u32> = guardians.iter().map(|(_, p)| *p).collect();
        process::terminate_pids(&gpids);
    }

    // 2) 等待主程序退出，超时强杀
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline && !process::alive_pids(&main_pids).is_empty() {
        std::thread::sleep(Duration::from_millis(150));
    }
    let still = process::alive_pids(&main_pids);
    if !still.is_empty() {
        process::terminate_pids(&still);
    }

    // 3) 防复活验证：持续监视 STAY_DEAD_SECS 秒，任何复活立即再次结束
    let mut kills = 0u32;
    let deadline = Instant::now() + Duration::from_secs(STAY_DEAD_SECS);
    loop {
        let m = process::find_pids_by_path(&exe);
        let g = process::find_guardians(&exe);
        if m.is_empty() && g.is_empty() {
            break;
        }
        kills += 1;
        if kills > MAX_RESPAWN_KILLS {
            logger::log(
                ERROR,
                CAT_MONITOR,
                "监控程序多次自动重启，已停止自动对抗（可能存在未识别的守护机制），请手动排查",
            );
            return;
        }
        let mut parts: Vec<String> = m.iter().map(|p| format!("主程序(PID {p})")).collect();
        parts.extend(g.iter().map(|(n, p)| format!("{n}(PID {p})")));
        logger::log(
            WARN,
            CAT_MONITOR,
            &format!(
                "检测到自动重启（第 {kills} 次）：{}，已强制结束",
                parts.join("、")
            ),
        );
        let all: Vec<u32> = m.iter().copied().chain(g.iter().map(|(_, p)| *p)).collect();
        process::terminate_pids(&all);
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(600));
    }

    // 4) 最终确认
    if process::find_pids_by_path(&exe).is_empty() && process::find_guardians(&exe).is_empty() {
        logger::log(
            INFO,
            CAT_MONITOR,
            &format!("监控程序已彻底关闭（原 PID {pid_list}），并已保持关闭"),
        );
    } else {
        logger::log(ERROR, CAT_MONITOR, "关闭流程结束，但仍检测到相关进程，请手动检查");
    }
}

/// 部分程序启动后会延迟创建窗口，启动后的前几秒内持续补隐藏
fn rehide_later(exe: String) {
    std::thread::spawn(move || {
        for delay_ms in [800u64, 1500, 2500, 3500] {
            std::thread::sleep(Duration::from_millis(delay_ms));
            process::hide_windows(&exe);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ACT_STOP;
    use crate::state::AppState;
    use std::sync::Mutex;

    fn make_state(cfg: Config) -> Arc<AppState> {
        Arc::new(AppState {
            config_path: std::env::temp_dir().join("ipmon-test-config.json"),
            logs_dir: std::env::temp_dir(),
            config: Mutex::new(cfg),
            monitor: Mutex::new(MonitorInner::new()),
        })
    }

    /// 规则调度：方向、反转作废、取消、总开关
    #[test]
    fn rule_schedule_and_cancel() {
        let cfg = Config {
            linkage_enabled: true,
            rules: Config::default_rules(),
            ..Config::default()
        };
        let state = make_state(cfg);

        // 上线 → 调度规则1（关闭监控程序）
        on_transition(&state, &state.config_snapshot(), true);
        {
            let m = state.monitor.lock().unwrap();
            assert_eq!(m.pending.len(), 1, "上线应调度 1 条规则");
            assert_eq!(m.pending[0].rule_id, 1);
            assert_eq!(m.pending[0].action.kind, ACT_STOP);
        }

        // 离线 → 规则1作废，调度规则2（启动监控程序）
        on_transition(&state, &state.config_snapshot(), false);
        {
            let m = state.monitor.lock().unwrap();
            assert_eq!(m.pending.len(), 1, "反转后应只剩离线规则");
            assert_eq!(m.pending[0].rule_id, 2);
        }

        // 取消全部
        let cancelled = cancel_pending(&state, None);
        assert_eq!(cancelled.len(), 1);
        assert!(state.monitor.lock().unwrap().pending.is_empty());

        // 按规则 ID 取消
        on_transition(&state, &state.config_snapshot(), true);
        let cancelled = cancel_pending(&state, Some(1));
        assert_eq!(cancelled.len(), 1);
        assert!(state.monitor.lock().unwrap().pending.is_empty());

        // 总开关关闭时不调度
        let cfg_off = Config {
            linkage_enabled: false,
            rules: Config::default_rules(),
            ..Config::default()
        };
        on_transition(&state, &cfg_off, true);
        assert!(state.monitor.lock().unwrap().pending.is_empty());
    }

    /// 多条规则同时调度（同触发条件并行执行）
    #[test]
    fn multiple_rules_parallel() {
        let mut rules = Config::default_rules();
        rules.push(LinkageRule {
            id: 3,
            enabled: true,
            trigger: TRIGGER_ONLINE.into(),
            delay_sec: 0,
            action: RuleAction::simple(crate::config::ACT_SHOW),
            note: "上线时显示窗口".into(),
        });
        let cfg = Config {
            linkage_enabled: true,
            rules,
            ..Config::default()
        };
        let state = make_state(cfg);
        on_transition(&state, &state.config_snapshot(), true);
        let m = state.monitor.lock().unwrap();
        assert_eq!(m.pending.len(), 2, "上线应同时调度规则 1 和 3");
    }

    /// 自定义脚本执行（隐藏方式运行 cmd 脚本写文件）
    #[test]
    fn run_command_script() {
        let out = std::env::temp_dir().join("ipmon_runcommand_test.txt");
        let _ = std::fs::remove_file(&out);
        let script = std::env::temp_dir().join("ipmon_runcommand_test.bat");
        std::fs::write(
            &script,
            format!("@echo off\r\necho hello > \"{}\"\r\n", out.display()),
        )
        .unwrap();
        let pid = run_command(
            script.to_string_lossy().as_ref(),
            "",
            true,
        )
        .expect("运行脚本失败");
        assert!(pid > 0);
        // 等待脚本完成
        for _ in 0..20 {
            if out.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(out.exists(), "脚本应已执行并写出文件");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(&script);
    }

    /// 真实采集端彻底关闭测试：验证互拉守护被打破、关闭后保持关闭。
    /// 采集端未安装时自动跳过。
    #[test]
    fn real_streamer_stays_closed() {
        let exe = r"D:\Program Files\AtHomeVideoStreamer\AtHomeVideoStreamer.exe";
        if !Path::new(exe).is_file() {
            return; // 本机未安装采集端，跳过
        }
        let cfg = Config {
            program_path: exe.to_string(),
            ..Config::default()
        };
        let state = make_state(cfg);

        // 前置：确保采集端在运行（隐藏方式启动）
        if process::find_pids_by_path(exe).is_empty() {
            process::spawn_program(exe, true).expect("启动采集端失败");
            std::thread::sleep(Duration::from_secs(3));
        }
        assert!(
            !process::find_pids_by_path(exe).is_empty(),
            "测试前置失败：采集端应已在运行"
        );

        // 执行彻底关闭（含守护进程与防复活验证）
        do_stop(&state);

        // 关闭后持续观察 15 秒：主程序与守护进程都必须保持关闭
        for round in 1..=5 {
            std::thread::sleep(Duration::from_secs(3));
            assert!(
                process::find_pids_by_path(exe).is_empty(),
                "第 {round} 次检查：采集端复活了！"
            );
            assert!(
                process::find_guardians(exe).is_empty(),
                "第 {round} 次检查：守护进程仍存活！"
            );
        }
    }
}
