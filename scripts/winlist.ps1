# List visible top-level windows of a process (ASCII source, UTF-8 console output)
param([string]$ProcessName = "ip-linkage-monitor")
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
Add-Type -TypeDefinition @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class WinList {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
}
"@
$target = @(Get-Process $ProcessName -ErrorAction SilentlyContinue)
Write-Output ("process_count=" + $target.Count)
if ($target.Count -eq 0) { exit 0 }
$targetId = $target[0].Id
$found = 0
$cb = [WinList+EnumProc]{
    param($h, $l)
    $wpid = 0
    [WinList]::GetWindowThreadProcessId($h, [ref]$wpid) | Out-Null
    if ($wpid -eq $targetId -and [WinList]::IsWindowVisible($h)) {
        $sb = New-Object System.Text.StringBuilder 256
        [WinList]::GetWindowTextW($h, $sb, 256) | Out-Null
        $t = $sb.ToString()
        if ($t.Length -gt 0) { Write-Output ("window=" + $t); $script:found++ }
    }
    return $true
}
[WinList]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
Write-Output ("titled_windows=" + $script:found)
$fg = [WinList]::GetForegroundWindow()
$fsb = New-Object System.Text.StringBuilder 256
[WinList]::GetWindowTextW($fg, $fsb, 256) | Out-Null
Write-Output ("foreground=" + $fsb.ToString())
