//! 系统托盘：左键单击显示主窗口，右键菜单（显示/联动开关/彻底关闭监控程序/退出）
use crate::logger::{self, CAT_SYSTEM, INFO, WARN};
use crate::monitor;
use crate::state::AppState;
use std::sync::{Arc, Mutex};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Wry};

/// 托盘运行时对象（保持存活 + 供命令同步联动勾选状态）
pub struct TrayState {
    pub link_item: Mutex<Option<CheckMenuItem<Wry>>>,
    pub _tray: Mutex<Option<TrayIcon<Wry>>>,
}

pub fn create(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    app.manage(TrayState {
        link_item: Mutex::new(None),
        _tray: Mutex::new(None),
    });

    let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    let link = CheckMenuItem::with_id(
        app,
        "toggle_linkage",
        "联动开关",
        true,
        app.state::<Arc<AppState>>().config_snapshot().linkage_enabled,
        None::<&str>,
    )?;
    let stop = MenuItem::with_id(app, "stop_program", "彻底关闭监控程序", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出程序", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&show, &sep1, &link, &sep2, &stop, &quit])?;

    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/icon.png"))?;

    let tray = TrayIconBuilder::with_id("main-tray")
        .icon(icon)
        .tooltip("掌上看家联动IP监控")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main(app),
            "toggle_linkage" => {
                let state = app.state::<Arc<AppState>>();
                let new_val = {
                    let mut cfg = state.config.lock().unwrap();
                    cfg.linkage_enabled = !cfg.linkage_enabled;
                    cfg.linkage_enabled
                };
                if let Err(e) = state.save_config() {
                    logger::log(WARN, CAT_SYSTEM, &format!("托盘切换联动保存失败：{e}"));
                }
                logger::log(
                    INFO,
                    CAT_SYSTEM,
                    &format!("托盘操作：联动开关 → {}", if new_val { "开" } else { "关" }),
                );
                if let Some(ts) = app.try_state::<TrayState>() {
                    if let Some(item) = ts.link_item.lock().unwrap().as_ref() {
                        let _ = item.set_checked(new_val);
                    }
                }
                let _ = app.emit("linkage-changed", new_val);
            }
            "stop_program" => {
                let st = app.state::<Arc<AppState>>().inner().clone();
                logger::log(INFO, CAT_SYSTEM, "托盘操作：彻底关闭监控程序");
                std::thread::spawn(move || monitor::do_stop(&st));
            }
            "quit" => {
                logger::log(INFO, CAT_SYSTEM, "用户从托盘退出程序");
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;

    if let Some(ts) = app.try_state::<TrayState>() {
        *ts.link_item.lock().unwrap() = Some(link);
        *ts._tray.lock().unwrap() = Some(tray);
    }
    logger::log(INFO, CAT_SYSTEM, "系统托盘图标已创建");
    Ok(())
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}
