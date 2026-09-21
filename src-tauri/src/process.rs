use std::path::Path;
use std::time::{Duration, Instant};
use winapi::shared::minwindef::{BOOL, DWORD, FALSE, LPARAM, TRUE};
/// WaitForSingleObject 超时返回值（winapi 未导出的 winbase 常量）
const WAIT_TIMEOUT: u32 = 258;
use winapi::shared::windef::HWND;
use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
use winapi::um::processthreadsapi::{
    CreateProcessW, OpenProcess, TerminateProcess, PROCESS_INFORMATION, STARTUPINFOW,
};
use winapi::um::psapi::GetModuleFileNameExW;
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::tlhelp32::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use winapi::um::winbase::STARTF_USESHOWWINDOW;
use winapi::um::winnt::{
    PROCESS_QUERY_INFORMATION, PROCESS_TERMINATE, PROCESS_VM_READ, SYNCHRONIZE,
};
use winapi::um::winuser::{
    EnumWindows, GetWindow, GetWindowLongPtrW, GetWindowLongW, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, PostMessageW, SetForegroundWindow, ShowWindow, GWL_EXSTYLE, GWL_STYLE,
    GW_OWNER, SW_HIDE, SW_RESTORE, SW_SHOW, WM_CLOSE, WS_CAPTION, WS_EX_TOOLWINDOW,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 启动监控程序。hidden=true 时通过 STARTF_USESHOWWINDOW + SW_HIDE 让窗口不显示。
pub fn spawn_program(exe: &str, hidden: bool) -> Result<u32, String> {
    let exe_w = wide(exe);
    let mut cmd_w = wide(&format!("\"{exe}\""));
    let dir_w = Path::new(exe)
        .parent()
        .map(|p| wide(&p.to_string_lossy()))
        .unwrap_or_else(|| wide("C:\\"));

    unsafe {
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        if hidden {
            si.dwFlags = STARTF_USESHOWWINDOW;
            si.wShowWindow = SW_HIDE as u16;
        }
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessW(
            exe_w.as_ptr(),
            cmd_w.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            FALSE,
            0,
            std::ptr::null_mut(),
            dir_w.as_ptr(),
            &mut si,
            &mut pi,
        );
        if ok == 0 {
            let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            return Err(format!("CreateProcess 失败（错误码 {code}）"));
        }
        let pid = pi.dwProcessId;
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        Ok(pid)
    }
}

fn query_image_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let n = GetModuleFileNameExW(h, std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32);
        CloseHandle(h);
        if n == 0 {
            None
        } else {
            Some(from_wide(&buf[..n as usize]))
        }
    }
}

fn pids_in_snapshot() -> Vec<u32> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                out.push(entry.th32ProcessID);
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
}

/// 按完整可执行文件路径查找运行中的进程 PID
pub fn find_pids_by_path(exe: &str) -> Vec<u32> {
    let target = exe.trim().to_lowercase();
    let target_name = Path::new(&target)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if target_name.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                let name = from_wide(&entry.szExeFile).to_lowercase();
                if name == target_name {
                    if let Some(full) = query_image_path(entry.th32ProcessID) {
                        if full.to_lowercase() == target {
                            out.push(entry.th32ProcessID);
                        }
                    }
                }
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
}

pub fn is_exe_running(exe: &str) -> bool {
    !find_pids_by_path(exe).is_empty()
}

fn pid_alive(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(SYNCHRONIZE, 0, pid);
        if !h.is_null() {
            let r = WaitForSingleObject(h, 0);
            CloseHandle(h);
            return r == WAIT_TIMEOUT;
        }
    }
    pids_in_snapshot().contains(&pid)
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let list = &mut *(lparam as *mut Vec<HWND>);
    list.push(hwnd);
    TRUE
}

fn windows_of_pid(pid: DWORD) -> Vec<HWND> {
    let mut all: Vec<HWND> = Vec::new();
    unsafe { EnumWindows(Some(enum_proc), &mut all as *mut _ as LPARAM) };
    all.into_iter()
        .filter(|&h| unsafe {
            let mut wpid: DWORD = 0;
            GetWindowThreadProcessId(h, &mut wpid);
            wpid == pid
        })
        .collect()
}

fn is_toolwindow(h: HWND) -> bool {
    unsafe { (GetWindowLongPtrW(h, GWL_EXSTYLE) & WS_EX_TOOLWINDOW as isize) != 0 }
}

/// 显示监控程序的主窗口（无属主、带标题栏的非工具窗口）
pub fn show_windows(exe: &str) -> usize {
    let mut n = 0;
    for pid in find_pids_by_path(exe) {
        for h in windows_of_pid(pid) {
            let is_main = unsafe {
                GetWindow(h, GW_OWNER).is_null()
                    && !is_toolwindow(h)
                    && (GetWindowLongW(h, GWL_STYLE) & WS_CAPTION as i32) != 0
            };
            if !is_main {
                continue;
            }
            unsafe {
                if IsIconic(h) != 0 {
                    ShowWindow(h, SW_RESTORE);
                } else {
                    ShowWindow(h, SW_SHOW);
                }
                SetForegroundWindow(h);
            }
            n += 1;
        }
    }
    n
}

/// 隐藏监控程序的可见顶层窗口
pub fn hide_windows(exe: &str) -> usize {
    let mut n = 0;
    for pid in find_pids_by_path(exe) {
        for h in windows_of_pid(pid) {
            let hideable = unsafe {
                IsWindowVisible(h) != 0 && GetWindow(h, GW_OWNER).is_null() && !is_toolwindow(h)
            };
            if !hideable {
                continue;
            }
            unsafe { ShowWindow(h, SW_HIDE) };
            n += 1;
        }
    }
    n
}

/// 关闭进程：先发 WM_CLOSE 礼貌退出，超过 graceful_ms 仍存活则强制终止。
/// 成功返回已关闭的 PID 列表，失败返回未关闭 PID 的描述。
/// （do_stop 已改用更彻底的同时终止+防复活逻辑，此函数保留供测试与外部调用）
#[allow(dead_code)]
pub fn close_pids(pids: &[u32], graceful_ms: u32) -> Result<Vec<u32>, String> {
    let mut closed = Vec::new();
    let mut failed = Vec::new();
    for &pid in pids {
        for h in windows_of_pid(pid) {
            unsafe { PostMessageW(h, WM_CLOSE, 0, 0) };
        }
        let deadline = Instant::now() + Duration::from_millis(graceful_ms as u64);
        while Instant::now() < deadline && pid_alive(pid) {
            std::thread::sleep(Duration::from_millis(100));
        }
        if !pid_alive(pid) {
            closed.push(pid);
            continue;
        }
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid);
            if h.is_null() {
                failed.push(pid);
                continue;
            }
            TerminateProcess(h, 1);
            WaitForSingleObject(h, 5000);
            CloseHandle(h);
            if pid_alive(pid) {
                failed.push(pid);
            } else {
                closed.push(pid);
            }
        }
    }
    if failed.is_empty() {
        Ok(closed)
    } else {
        Err(failed.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", "))
    }
}

/// 立即强制结束进程列表，返回已成功终止的 PID
pub fn terminate_pids(pids: &[u32]) -> Vec<u32> {
    let mut killed = Vec::new();
    for &pid in pids {
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid);
            if !h.is_null() {
                if TerminateProcess(h, 1) != 0 {
                    killed.push(pid);
                }
                WaitForSingleObject(h, 3000);
                CloseHandle(h);
            }
        }
    }
    killed
}

/// 返回列表中仍然存活的 PID
pub fn alive_pids(pids: &[u32]) -> Vec<u32> {
    pids.iter().copied().filter(|&p| pid_alive(p)).collect()
}

/// 向进程的顶层窗口投递 WM_CLOSE（礼貌退出请求，不等待）
pub fn post_wm_close(pids: &[u32]) {
    for &pid in pids {
        for h in windows_of_pid(pid) {
            unsafe { PostMessageW(h, WM_CLOSE, 0, 0) };
        }
    }
}

fn is_windows_dir(dir: &str) -> bool {
    let d = dir.trim_end_matches('\\').to_lowercase();
    d == "c:\\windows" || d.starts_with("c:\\windows\\")
}

/// 主程序同目录下的其他 exe 文件名（排除主程序自身与卸载/安装/更新器）
pub fn sibling_exe_names(exe: &str) -> Vec<String> {
    let p = Path::new(exe);
    let Some(dir) = p.parent() else { return Vec::new() };
    let main_name = p
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut names = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let path = e.path();
            if !path.is_file() {
                continue;
            }
            if !path
                .extension()
                .map(|x| x.eq_ignore_ascii_case("exe"))
                .unwrap_or(false)
            {
                continue;
            }
            let n = path
                .file_name()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if n.is_empty() || n == main_name {
                continue;
            }
            if n.starts_with("unins")
                || n.contains("uninstall")
                || n.contains("setup")
                || n.contains("update")
            {
                continue;
            }
            names.push(n);
        }
    }
    names
}

/// 查找主程序同目录下的守护/加载进程（如 AvsLoader.exe），返回 (进程名, PID)。
/// 掌上看家采用互拉守护：主程序 spawn AvsLoader，AvsLoader 再拉起主程序，
/// 只杀其中一个会被另一个复活，必须同时结束。系统目录（C:\Windows 下）一律跳过以防误杀。
pub fn find_guardians(exe: &str) -> Vec<(String, u32)> {
    let p = Path::new(exe);
    let dir_str = match p.parent() {
        Some(d) => d.to_string_lossy().trim_end_matches('\\').to_lowercase(),
        None => return Vec::new(),
    };
    if dir_str.is_empty() || is_windows_dir(&dir_str) {
        return Vec::new();
    }
    let names = sibling_exe_names(exe);
    if names.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                let name = from_wide(&entry.szExeFile).to_lowercase();
                if names.contains(&name) {
                    if let Some(full) = query_image_path(entry.th32ProcessID) {
                        let parent_dir = Path::new(&full)
                            .parent()
                            .map(|d| {
                                d.to_string_lossy().trim_end_matches('\\').to_lowercase()
                            })
                            .unwrap_or_default();
                        if parent_dir == dir_str {
                            out.push((name, entry.th32ProcessID));
                        }
                    }
                }
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// winver.exe（关于 Windows 对话框）：小巧无害、可礼貌关闭，适合做进程控制测试
    const TEST_EXE: &str = r"C:\Windows\System32\winver.exe";

    fn wait_gone(exe: &str, ms: u64) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_millis(ms);
        while std::time::Instant::now() < deadline {
            if !is_exe_running(exe) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        false
    }

    #[test]
    fn spawn_hide_show_close_lifecycle() {
        // 隐藏方式启动
        let pid = spawn_program(TEST_EXE, true).expect("spawn failed");
        std::thread::sleep(Duration::from_millis(1800));
        assert!(is_exe_running(TEST_EXE), "启动后应能找到进程");
        let pids = find_pids_by_path(TEST_EXE);
        assert!(pids.contains(&pid), "按路径查找应包含启动的 PID {pid}, got {pids:?}");

        // 显示窗口（winver 可能尊重 SW_HIDE 保持隐藏，先显示确保窗口可见）
        let shown = show_windows(TEST_EXE);
        assert!(shown >= 1, "至少应显示 1 个主窗口");

        // 隐藏窗口（此时窗口刚被显示，必然可隐藏）
        let hidden = hide_windows(TEST_EXE);
        assert!(hidden >= 1, "至少应隐藏 1 个窗口");

        // 礼貌关闭（WM_CLOSE 应使 winver 退出，无需强杀）
        close_pids(&pids, 5000).expect("关闭失败");
        assert!(wait_gone(TEST_EXE, 4000), "关闭后进程应退出");
    }

    #[test]
    fn normal_spawn_then_close() {
        let pid = spawn_program(TEST_EXE, false).expect("spawn failed");
        std::thread::sleep(Duration::from_millis(1800));
        assert!(find_pids_by_path(TEST_EXE).contains(&pid));
        let pids = find_pids_by_path(TEST_EXE);
        close_pids(&pids, 5000).expect("关闭失败");
        assert!(wait_gone(TEST_EXE, 4000), "关闭后进程应退出");
    }
}
