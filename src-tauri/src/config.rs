use serde::{Deserialize, Serialize};

pub const TRIGGER_ONLINE: &str = "online";
pub const TRIGGER_OFFLINE: &str = "offline";

pub const ACT_START: &str = "start_program";
pub const ACT_STOP: &str = "stop_program";
pub const ACT_SHOW: &str = "show_window";
pub const ACT_HIDE: &str = "hide_window";
pub const ACT_RUN: &str = "run_command";

fn default_true() -> bool {
    true
}

/// 规则的执行动作：内置程序控制，或自定义执行脚本/程序
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleAction {
    /// start_program | stop_program | show_window | hide_window | run_command
    #[serde(rename = "type")]
    pub kind: String,
    /// run_command 专用：脚本/程序路径
    #[serde(default)]
    pub path: String,
    /// run_command 专用：启动参数
    #[serde(default)]
    pub args: String,
    /// run_command 专用：隐藏窗口执行（不弹出控制台）
    #[serde(default = "default_true")]
    pub hidden: bool,
}

impl RuleAction {
    pub fn simple(kind: &str) -> Self {
        Self {
            kind: kind.to_string(),
            path: String::new(),
            args: String::new(),
            hidden: true,
        }
    }

    pub fn desc(&self) -> String {
        match self.kind.as_str() {
            ACT_START => "启动监控程序".to_string(),
            ACT_STOP => "彻底关闭监控程序".to_string(),
            ACT_SHOW => "显示监控程序窗口".to_string(),
            ACT_HIDE => "隐藏监控程序窗口".to_string(),
            ACT_RUN => format!(
                "执行「{}」",
                if self.path.trim().is_empty() { "(未设置脚本)" } else { self.path.trim() }
            ),
            other => format!("未知动作({other})"),
        }
    }

    pub fn valid_kind(&self) -> bool {
        matches!(
            self.kind.as_str(),
            ACT_START | ACT_STOP | ACT_SHOW | ACT_HIDE | ACT_RUN
        )
    }
}

/// 一条联动规则：IP 状态变化（上线/离线）后延迟执行某动作
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkageRule {
    pub id: u32,
    pub enabled: bool,
    /// online | offline
    pub trigger: String,
    /// 延迟秒数，0 = 立即执行
    pub delay_sec: u32,
    pub action: RuleAction,
    /// 用户备注
    #[serde(default)]
    pub note: String,
}

/// 应用配置，持久化到 %APPDATA%\com.athome.ipmonitor\config.json
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {    /// 监控程序（掌上看家采集端）exe 路径，程序控制类动作的对象
    pub program_path: String,
    /// 被监控的 IP 地址
    pub monitor_ip: String,
    /// 监控方式：ip（Ping）| bluetooth（蓝牙邻近）
    pub monitor_mode: String,
    /// 蓝牙模式：要监控的设备名称或 MAC
    pub bt_device: String,
    /// 蓝牙查询时长倍数（×1.28 秒/单位）
    pub bt_scan_timeout_mult: u32,
    /// 联动总开关（关闭时不调度任何规则）
    pub linkage_enabled: bool,
    /// 启动监控程序时隐藏其窗口（后台运行）
    pub launch_hidden: bool,
    /// 关闭主窗口时最小化到托盘（false = 直接退出程序）
    pub close_to_tray: bool,
    /// IP 检测间隔（秒）
    pub poll_interval_sec: u32,
    /// Ping 超时（毫秒）
    pub ping_timeout_ms: u32,
    /// 联动规则列表
    pub rules: Vec<LinkageRule>,
    /// 是否已完成默认规则播种（避免用户删光规则后被再次生成）
    pub rules_seeded: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            program_path: String::new(),
            monitor_ip: "172.30.109.238".to_string(),
            monitor_mode: "ip".to_string(),
            bt_device: String::new(),
            bt_scan_timeout_mult: 4,
            linkage_enabled: false,
            launch_hidden: true,
            close_to_tray: true,
            poll_interval_sec: 3,
            ping_timeout_ms: 2000,
            rules: Vec::new(),
            rules_seeded: false,
        }
    }
}

impl Config {
    /// 默认规则（用户确认的策略方向：上线→关闭，离线→启动）
    pub fn default_rules() -> Vec<LinkageRule> {
        vec![
            LinkageRule {
                id: 1,
                enabled: true,
                trigger: TRIGGER_ONLINE.into(),
                delay_sec: 10,
                action: RuleAction::simple(ACT_STOP),
                note: "IP 上线 10 秒后关闭监控程序".into(),
            },
            LinkageRule {
                id: 2,
                enabled: true,
                trigger: TRIGGER_OFFLINE.into(),
                delay_sec: 10,
                action: RuleAction::simple(ACT_START),
                note: "IP 离线 10 秒后启动监控程序".into(),
            },
        ]
    }
}
