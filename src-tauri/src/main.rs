#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod config;
mod logger;
mod monitor;
mod netcheck;
mod process;
mod registry;
mod state;
mod tray;

use crate::state::AppState;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Manager, WindowEvent};

/// 数据目录解析：优先「程序所在路径\data」（便携模式，需可写），
/// 不可写（如放在 Program Files）时回退 %APPDATA%。
/// 首次切换到便携目录时，自动迁移旧的配置与日志。
fn resolve_data_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let exe_dir = std::env::current_exe()
        .map_err(|e| format!("获取程序路径失败：{e}"))?
        .parent()
        .ok_or("无法定位程序目录")?
        .to_path_buf();
    let portable = exe_dir.join("data");

    let writable = std::fs::create_dir_all(&portable).is_ok()
        && std::fs::write(portable.join(".write_test"), b"ok").is_ok();
    if !writable {
        let _ = std::fs::remove_file(portable.join(".write_test"));
        let fallback = app.path().app_data_dir().map_err(|e| e.to_string())?;
        return Ok(fallback); // 回退时由调用方记录日志（logger 尚未初始化，先写文件不合适）
    }
    let _ = std::fs::remove_file(portable.join(".write_test"));

    // 迁移旧 %APPDATA% 配置与日志（仅当便携目录还没有配置时）
    if let Ok(old) = app.path().app_data_dir() {
        let old_cfg = old.join("config.json");
        let new_cfg = portable.join("config.json");
        if old_cfg.is_file() && !new_cfg.exists() {
            if std::fs::copy(&old_cfg, &new_cfg).is_ok() {
                std::fs::write(
                    portable.join(".migrated_from_appdata"),
                    old_cfg.display().to_string(),
                )
                .ok();
            }
        }
        let old_logs = old.join("logs");
        let new_logs = portable.join("logs");
        if old_logs.is_dir() && !new_logs.exists() {
            let _ = std::fs::create_dir_all(&new_logs);
            if let Ok(rd) = std::fs::read_dir(&old_logs) {
                for f in rd.flatten() {
                    let _ = std::fs::copy(f.path(), new_logs.join(f.file_name()));
                }
            }
        }
    }
    Ok(portable)
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();
            let data_dir = resolve_data_dir(&handle)?;
            std::fs::create_dir_all(data_dir.join("logs"))
                .map_err(|e| format!("创建日志目录失败：{e}"))?;
            logger::init(handle.clone(), data_dir.join("logs"));

            let app_state = Arc::new(state::AppState::load(&data_dir)?);
            app.manage(app_state.clone());
            monitor::spawn_thread(handle.clone(), app_state);

            if let Err(e) = tray::create(&handle) {
                logger::log(
                    logger::WARN,
                    logger::CAT_SYSTEM,
                    &format!("系统托盘创建失败（程序继续运行）：{e}"),
                );
            }

            logger::log(
                logger::INFO,
                logger::CAT_SYSTEM,
                &format!("应用程序已启动（数据目录：{}）", data_dir.display()),
            );
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let state = window.app_handle().state::<Arc<AppState>>();
                let close_to_tray = state.config_snapshot().close_to_tray;
                if close_to_tray {
                    api.prevent_close();
                    let _ = window.hide();
                    logger::log(
                        logger::INFO,
                        logger::CAT_SYSTEM,
                        "主窗口已最小化到托盘，程序在后台继续运行（托盘菜单可退出）",
                    );
                } else {
                    logger::log(logger::INFO, logger::CAT_SYSTEM, "应用程序退出（主窗口关闭）");
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::set_config,
            commands::get_status,
            commands::cancel_pending,
            commands::read_registry_path,
            commands::browse_program_path,
            commands::manual_start,
            commands::manual_stop,
            commands::show_program_window,
            commands::hide_program_window,
            commands::test_ip,
            commands::run_action,
            commands::add_rule,
            commands::update_rule,
            commands::delete_rule,
            commands::toggle_rule,
            commands::reset_rules,
            commands::get_config_path,
            commands::export_config,
            commands::import_config,
            commands::get_logs,
            commands::export_logs,
            commands::clear_logs,
        ])
        .run(tauri::generate_context!())
        .expect("应用程序运行失败");
}
