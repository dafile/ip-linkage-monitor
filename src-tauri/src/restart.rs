//! 定时重启「本监控程序」。
//!
//! 目的：程序长期运行后，Windows 蓝牙栈/系统状态可能产生陈旧缓存
//! （表现为蓝牙已关却仍显示在线、检测逐渐不灵敏）；定期自重启可让
//! 系统栈与程序内部会话回到干净状态。
//!
//! 注意：这里只重启本程序自身——**绝不重启或关机电脑**，也不影响被监控的采集端。

use crate::logger::{self, CAT_SYSTEM, INFO};
use crate::state::AppState;
use chrono::{Local, NaiveDate, NaiveTime};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::AppHandle;

/// 调度检查周期
const TICK: Duration = Duration::from_secs(20);

/// 界面展示用的「下次计划重启」文本
static NEXT_DUE_TEXT: OnceLock<Mutex<Option<String>>> = OnceLock::new();

pub fn next_due_text() -> Option<String> {
    NEXT_DUE_TEXT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .clone()
}

fn set_next_due_text(v: Option<String>) {
    *NEXT_DUE_TEXT.get_or_init(|| Mutex::new(None)).lock().unwrap() = v;
}

/// 解析 HH:MM；非法输入回退到 04:00
pub fn parse_hhmm(s: &str) -> NaiveTime {
    NaiveTime::parse_from_str(s.trim(), "%H:%M")
        .unwrap_or_else(|_| NaiveTime::from_hms_opt(4, 0, 0).unwrap())
}

/// 每日模式：今天到点且今天尚未执行过 → 返回今天；否则 None
pub fn daily_due(
    now: chrono::DateTime<Local>,
    hhmm: &str,
    last_run: Option<NaiveDate>,
) -> Option<NaiveDate> {
    let t = parse_hhmm(hhmm);
    let today = now.date_naive();
    if last_run == Some(today) {
        return None;
    }
    if now.time() >= t {
        Some(today)
    } else {
        None
    }
}

fn next_daily_text(now: chrono::DateTime<Local>, hhmm: &str) -> String {
    let t = parse_hhmm(hhmm);
    let date = if now.time() < t {
        now.date_naive()
    } else {
        now.date_naive() + chrono::Days::new(1)
    };
    format!("{} {}", date.format("%m-%d"), t.format("%H:%M"))
}

fn interval_text(anchor: Instant, minutes: u32) -> String {
    let remain = Duration::from_secs(minutes as u64 * 60).saturating_sub(anchor.elapsed());
    let at = Local::now() + chrono::Duration::from_std(remain).unwrap_or_default();
    at.format("%m-%d %H:%M").to_string()
}

pub fn human_interval(minutes: u32) -> String {
    match minutes {
        60 => "1 小时".into(),
        180 => "3 小时".into(),
        360 => "6 小时".into(),
        720 => "12 小时".into(),
        m if m % 60 == 0 && m >= 60 => format!("{} 小时", m / 60),
        m => format!("{m} 分钟"),
    }
}

/// 启动定时重启调度线程（软重启：重置监控服务，不退出进程、不重建窗口）
pub fn spawn_thread(_app: AppHandle, state: Arc<AppState>) {
    std::thread::spawn(move || {
        let boot = Local::now();
        let mut interval_anchor = Instant::now();
        // 启动时若已过今天的定时点，今天不再触发（避免刚启动就重启）
        let cfg0 = state.config_snapshot();
        let mut last_daily: Option<NaiveDate> = {
            let t = parse_hhmm(&cfg0.auto_restart_daily_time);
            if boot.time() >= t {
                Some(boot.date_naive())
            } else {
                None
            }
        };
        let mut last_key = String::new();
        let mut local_epoch = state.service_epoch.load(Ordering::Relaxed);

        loop {
            let cfg = state.config_snapshot();

            // 软重启（手动按钮或其他来源）后重新计时
            let epoch = state.service_epoch.load(Ordering::Relaxed);
            if epoch != local_epoch {
                local_epoch = epoch;
                interval_anchor = Instant::now();
                let now = Local::now();
                last_daily = if now.time() >= parse_hhmm(&cfg.auto_restart_daily_time) {
                    Some(now.date_naive())
                } else {
                    None
                };
                last_key.clear();
            }

            if !cfg.auto_restart_enabled {
                if !last_key.is_empty() {
                    interval_anchor = Instant::now();
                    last_key.clear();
                }
                set_next_due_text(None);
                std::thread::sleep(TICK);
                continue;
            }

            // 配置变化时重置计时锚点，保证「改成 N 小时」从当前时刻起算
            let key = format!(
                "{}|{}|{}",
                cfg.auto_restart_mode, cfg.auto_restart_interval_min, cfg.auto_restart_daily_time
            );
            if key != last_key {
                interval_anchor = Instant::now();
                let now = Local::now();
                last_daily = if now.time() >= parse_hhmm(&cfg.auto_restart_daily_time) {
                    Some(now.date_naive())
                } else {
                    None
                };
                last_key = key;
            }

            let now = Local::now();
            if cfg.auto_restart_mode == "daily" {
                set_next_due_text(Some(next_daily_text(now, &cfg.auto_restart_daily_time)));
                if daily_due(now, &cfg.auto_restart_daily_time, last_daily).is_some() {
                    last_daily = Some(now.date_naive());
                    let when = parse_hhmm(&cfg.auto_restart_daily_time);
                    logger::log(
                        INFO,
                        CAT_SYSTEM,
                        &format!(
                            "定时重启：到达每日计划时间 {}，执行软重启（重置检测状态与蓝牙会话，窗口与进程不变）",
                            when.format("%H:%M")
                        ),
                    );
                    local_epoch = request_soft_restart(&state);
                    interval_anchor = Instant::now();
                }
            } else {
                let minutes = cfg.auto_restart_interval_min.clamp(5, 1440);
                set_next_due_text(Some(interval_text(interval_anchor, minutes)));
                if interval_anchor.elapsed() >= Duration::from_secs(minutes as u64 * 60) {
                    logger::log(
                        INFO,
                        CAT_SYSTEM,
                        &format!(
                            "定时重启：已连续运行 {}，执行软重启（重置检测状态与蓝牙会话，窗口与进程不变）",
                            human_interval(minutes)
                        ),
                    );
                    local_epoch = request_soft_restart(&state);
                    interval_anchor = Instant::now();
                }
            }

            std::thread::sleep(TICK);
        }
    });
}

/// 请求软重启：递增服务代号并**立即**重置监控状态（同步生效，避免界面残留旧状态）。
/// 监控线程会用代号守卫丢弃重置前发出的在途检测结果。
/// 不退出进程、不重建窗口，也不影响电脑与采集端。返回新代号。
pub fn request_soft_restart(state: &AppState) -> u64 {
    let epoch = state.service_epoch.fetch_add(1, Ordering::SeqCst) + 1;
    crate::monitor::apply_soft_reset(state);
    epoch
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parse_hhmm_fallback() {
        assert_eq!(parse_hhmm("07:30").format("%H:%M").to_string(), "07:30");
        assert_eq!(parse_hhmm(" 23:59 ").format("%H:%M").to_string(), "23:59");
        // 非法输入回退 04:00
        assert_eq!(parse_hhmm("25:99").format("%H:%M").to_string(), "04:00");
        assert_eq!(parse_hhmm("").format("%H:%M").to_string(), "04:00");
    }

    #[test]
    fn daily_due_logic() {
        let before = Local.with_ymd_and_hms(2026, 11, 19, 3, 59, 0).unwrap();
        let after = Local.with_ymd_and_hms(2026, 11, 19, 4, 0, 0).unwrap();
        // 未到点：不触发
        assert!(daily_due(before, "04:00", None).is_none());
        // 到点且今天未执行：触发
        assert_eq!(daily_due(after, "04:00", None), Some(after.date_naive()));
        // 今天已执行：不重复触发
        assert!(daily_due(after, "04:00", Some(after.date_naive())).is_none());
        // 昨天执行过不影响今天
        let yesterday = after.date_naive() - chrono::Days::new(1);
        assert_eq!(
            daily_due(after, "04:00", Some(yesterday)),
            Some(after.date_naive())
        );
    }

    #[test]
    fn interval_labels() {
        assert_eq!(human_interval(30), "30 分钟");
        assert_eq!(human_interval(60), "1 小时");
        assert_eq!(human_interval(180), "3 小时");
        assert_eq!(human_interval(360), "6 小时");
        assert_eq!(human_interval(720), "12 小时");
    }

    #[test]
    fn next_daily_text_rolls_to_tomorrow() {
        let before = Local.with_ymd_and_hms(2026, 11, 19, 3, 0, 0).unwrap();
        let after = Local.with_ymd_and_hms(2026, 11, 19, 5, 0, 0).unwrap();
        assert_eq!(next_daily_text(before, "04:00"), "11-19 04:00");
        assert_eq!(next_daily_text(after, "04:00"), "11-20 04:00");
    }
}
