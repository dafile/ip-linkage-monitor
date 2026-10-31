# Report ip-linkage-monitor window state (ASCII only)
param([string]$ProcessName = "ip-linkage-monitor")
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class WinStat {
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
}
"@
$procs = @(Get-Process $ProcessName -ErrorAction SilentlyContinue)
Write-Output ("process_count=" + $procs.Count)
if ($procs.Count -eq 0) { exit 0 }
$p = $procs | Select-Object -First 1
$h = $p.MainWindowHandle
Write-Output ("pid=" + $p.Id)
Write-Output ("main_window_handle=" + $h)
if ($h -ne [IntPtr]::Zero) {
    Write-Output ("is_iconic=" + [WinStat]::IsIconic($h))
    Write-Output ("is_visible=" + [WinStat]::IsWindowVisible($h))
    Write-Output ("is_foreground=" + ([WinStat]::GetForegroundWindow() -eq $h))
} else {
    Write-Output "window_hidden_or_no_main_window=true"
}
