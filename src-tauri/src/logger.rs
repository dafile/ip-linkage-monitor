use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tauri::{AppHandle, Emitter};

pub const INFO: &str = "INFO";
pub const WARN: &str = "WARN";
pub const ERROR: &str = "ERROR";

pub const CAT_SYSTEM: &str = "系统";
pub const CAT_MONITOR: &str = "监控";
pub const CAT_USER: &str = "操作";
pub const CAT_CONFIG: &str = "配置";

static APP: OnceLock<AppHandle> = OnceLock::new();
static LOGS_DIR: OnceLock<PathBuf> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub ts: String,
    pub level: String,
    pub category: String,
    pub message: String,
}

pub fn init(app: AppHandle, dir: PathBuf) {
    let _ = APP.set(app);
    let _ = LOGS_DIR.set(dir);
}

/// 写入当天日志文件并向前端推送实时事件
pub fn log(level: &str, category: &str, message: &str) {
    let entry = LogEntry {
        ts: Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        level: level.to_string(),
        category: category.to_string(),
        message: message.to_string(),
    };
    if let Some(dir) = LOGS_DIR.get() {
        let file = dir.join(format!("log-{}.jsonl", Local::now().format("%Y%m%d")));
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&file) {
            let _ = writeln!(f, "{}", serde_json::to_string(&entry).unwrap_or_default());
        }
    }
    if let Some(app) = APP.get() {
        let _ = app.emit("log-entry", &entry);
    }
    #[cfg(debug_assertions)]
    println!("[{}] [{}] [{}] {}", entry.ts, entry.level, entry.category, entry.message);
}

#[derive(Debug, Deserialize)]
pub struct LogFilter {
    pub date: Option<String>,
    #[serde(default)]
    pub levels: Vec<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    pub keyword: Option<String>,
    pub limit: Option<u32>,
}

fn file_for_date(dir: &Path, date: &str) -> PathBuf {
    let digits: String = date.chars().filter(|c| c.is_ascii_digit()).collect();
    dir.join(format!("log-{digits}.jsonl"))
}

/// 按条件查询日志，返回按时间倒序的条目
pub fn query(dir: &Path, f: &LogFilter) -> Vec<LogEntry> {
    let mut files: Vec<PathBuf> = match &f.date {
        Some(d) => vec![file_for_date(dir, d)],
        None => fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
                    .collect()
            })
            .unwrap_or_default(),
    };
    if f.date.is_none() {
        files.sort();
        files.reverse();
    }
    let kw = f.keyword.as_deref().unwrap_or("").trim().to_lowercase();
    let limit = f.limit.unwrap_or(2000).max(1) as usize;
    let mut out: Vec<LogEntry> = Vec::new();
    'outer: for file in files {
        let Ok(content) = fs::read_to_string(&file) else { continue };
        for line in content.lines().rev() {
            let Ok(e) = serde_json::from_str::<LogEntry>(line) else { continue };
            if !f.levels.is_empty() && !f.levels.iter().any(|l| l.eq_ignore_ascii_case(&e.level)) {
                continue;
            }
            if !f.categories.is_empty() && !f.categories.contains(&e.category) {
                continue;
            }
            if !kw.is_empty() && !e.message.to_lowercase().contains(&kw) {
                continue;
            }
            out.push(e);
            if out.len() >= limit {
                break 'outer;
            }
        }
    }
    out
}

fn csv_field(v: &str) -> String {
    if v.contains(',') || v.contains('"') || v.contains('\n') || v.contains('\r') {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_string()
    }
}

/// 导出为带 BOM 的 CSV（Excel 直接打开中文不乱码）
pub fn export_csv(dir: &Path, f: &LogFilter, target: &Path) -> Result<usize, String> {
    let entries = query(dir, f);
    let mut csv = String::from("\u{FEFF}时间,级别,类别,内容\r\n");
    for e in &entries {
        csv.push_str(&format!(
            "{},{},{},{}\r\n",
            csv_field(&e.ts),
            csv_field(&e.level),
            csv_field(&e.category),
            csv_field(&e.message)
        ));
    }
    fs::write(target, csv.as_bytes()).map_err(|e| format!("写入导出文件失败：{e}"))?;
    Ok(entries.len())
}

/// 清空全部日志文件，返回删除数量
pub fn clear(dir: &Path) -> Result<usize, String> {
    let mut n = 0;
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "jsonl").unwrap_or(false) && fs::remove_file(&p).is_ok() {
                n += 1;
            }
        }
    }
    Ok(n)
}
