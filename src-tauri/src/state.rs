use crate::config::Config;
use crate::logger;
use crate::monitor::MonitorInner;
use crate::registry;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 全局共享状态：配置 + 监控线程运行时状态
pub struct AppState {
    pub config_path: PathBuf,
    pub logs_dir: PathBuf,
    pub config: Mutex<Config>,
    pub monitor: Mutex<MonitorInner>,
}

impl AppState {
    pub fn load(data_dir: &Path) -> Result<Self, String> {
        let config_path = data_dir.join("config.json");
        let first_run = !config_path.exists();
        let mut config: Config = if !first_run {
            fs::read_to_string(&config_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        } else {
            // 首次运行：默认从注册表读取监控程序路径
            let mut c = Config::default();
            if let Ok(p) = registry::read_program_path() {
                c.program_path = p;
            }
            c
        };

        let mut notices: Vec<(bool, String)> = Vec::new(); // (是否警告, 内容)
        if first_run {
            if config.program_path.is_empty() {
                notices.push((
                    true,
                    "首次运行：未能从注册表读取监控程序路径，请在设置中手动选择".into(),
                ));
            } else {
                notices.push((
                    false,
                    format!("首次运行：已从注册表读取监控程序路径 {}", config.program_path),
                ));
            }
        }
        // 旧版本配置升级：播种默认联动规则（仅一次，用户删光后不会再生）
        if !config.rules_seeded {
            if config.rules.is_empty() {
                config.rules = Config::default_rules();
                notices.push((
                    false,
                    "已生成默认联动规则：IP 上线 10 秒后关闭监控程序，离线 10 秒后启动（可在设置中自由调整）"
                        .into(),
                ));
            }
            config.rules_seeded = true;
        }

        let state = Self {
            config_path,
            logs_dir: data_dir.join("logs"),
            config: Mutex::new(config),
            monitor: Mutex::new(MonitorInner::new()),
        };

        if !notices.is_empty() {
            let _ = state.save_config();
            for (warn, msg) in &notices {
                logger::log(
                    if *warn { logger::WARN } else { logger::INFO },
                    logger::CAT_CONFIG,
                    msg,
                );
            }
        }
        Ok(state)
    }

    pub fn config_snapshot(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    /// 原子写入配置（先写临时文件再重命名）
    pub fn save_config(&self) -> Result<(), String> {
        let cfg = self.config_snapshot();
        let json = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
        let tmp = self.config_path.with_extension("json.tmp");
        fs::write(&tmp, json).map_err(|e| format!("写入配置失败：{e}"))?;
        fs::rename(&tmp, &self.config_path).map_err(|e| format!("保存配置失败：{e}"))?;
        Ok(())
    }
}
