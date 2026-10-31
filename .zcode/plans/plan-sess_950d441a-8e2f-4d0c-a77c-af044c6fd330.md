## Context（问题与目标）

你看到的多个托盘图标，是因为程序**完全没有单实例约束**：每启动一次 = 一个独立进程，各自创建一只托盘图标、各自跑监控循环（同一个状态变化可能被执行两次联动规则），还共用同一份 `data\config.json` 与日志文件（并发写有覆盖/交错风险）。

目标：
1. 全系统只允许运行一个实例——重复启动时，**第二个进程立即退出**；
2. 把已运行实例的窗口**恢复显示并置前**（覆盖"最小化到托盘"状态）；
3. 重复启动事件记入运行日志，便于排查。

## 实现方案（3 处代码改动）

采用 Tauri 官方插件 `tauri-plugin-single-instance`（Windows 下基于命名互斥量 + 消息转发，成熟可靠）。

1. **`src-tauri/Cargo.toml`** — 依赖区新增：
   `tauri-plugin-single-instance = "2"`

2. **`src-tauri/src/main.rs:67`** — 在 `tauri::Builder::default()` 上**第一个**链式注册插件（官方要求先于其他插件）：
   ```rust
   .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
       logger::log(logger::INFO, logger::CAT_SYSTEM,
           "检测到重复启动：已聚焦正在运行的窗口");
       tray::show_main(app);
   }))
   ```
   - 第二个进程由插件自动退出，回调在**已运行的第一个实例**中执行；
   - 回调时机在首个实例 setup 完成后，logger 已初始化，日志必然落盘。

3. **`src-tauri/src/tray.rs:99`** — 把现有窗口恢复函数提权复用：
   `fn show_main` → `pub(crate) fn show_main`（保持 `show() + unminimize() + set_focus()`）。
   追加防焦点抢占兜底：`set_focus()` 后若 `is_focused()` 仍为 false，则调用
   `request_user_attention(Some(Informational))` 闪动任务栏（Windows 偶发拒绝抢前台时仍能引起注意）。

> 不改前端、不改配置格式、不动监控/联动逻辑。第二实例退出后不会产生第二个托盘图标，也不会重复执行规则。

## 版本与发布（沿用本仓库既定流程）

- 版本号 1.5.0 → **1.5.1**：`src-tauri/Cargo.toml`、`src-tauri/tauri.conf.json`、`ui/index.html` 顶栏
- 更新 `发布/ip-linkage-monitor-v1.5.1.exe` 与 `发布/使用说明.txt`
- 本地 commit → push → GitHub Release `v1.5.1`（附绿色 exe）

## 验证步骤（本机可完整验证）

1. `cargo build --release` + `cargo test --release`（现有 16 个测试不应回归）
2. `taskkill /IM ip-linkage-monitor.exe /F` 清掉当前残留多实例 → 启动一个新实例
3. **重复启动测试**：再次执行同一 exe →
   - `tasklist` 中 `ip-linkage-monitor.exe` 计数仍为 **1**（第二进程已自退）；
   - 运行日志出现「检测到重复启动：已聚焦正在运行的窗口」；
   - 截图确认原窗口被显示并置于前台（`scripts/capture.ps1`）。
4. **托盘场景测试**：让窗口最小化到托盘（点击窗口关闭按钮，close_to_tray 默认开启）→ 再次启动 exe → 窗口应从托盘中恢复并置前。
5. 托盘图标只应存在一个（旧残留实例需先全部退出）。

## 影响面

- 仅 `main.rs` / `tray.rs` / `Cargo.toml` 三处；单实例行为对"关闭到托盘后再次双击图标"的场景尤其关键；
- 已存在的多个旧实例需要手动结束一次（新版启动后它们会因文件被占用无法热替换，属正常）。