# Capture main window of a process to PNG. Forces window topmost first (ASCII only)
param(
    [string]$ProcessName = "ip-linkage-monitor",
    [string]$OutFile = "test-artifacts/ui-main.png"
)
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class WinCap {
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    public static int[] RectOf(IntPtr h) {
        RECT r; GetWindowRect(h, out r);
        return new int[] { r.Left, r.Top, r.Right - r.Left, r.Bottom - r.Top };
    }
    public static void ForceTop(IntPtr h) {
        ShowWindow(h, 9); // SW_RESTORE
        IntPtr topmost = new IntPtr(-1);
        SetWindowPos(h, topmost, 0, 0, 0, 0, 0x0001 | 0x0002 | 0x0040); // NOSIZE|NOMOVE|SHOWWINDOW
    }
    public static void UnTop(IntPtr h) {
        IntPtr notop = new IntPtr(-2);
        SetWindowPos(h, notop, 0, 0, 0, 0, 0x0001 | 0x0002 | 0x0040);
    }
}
"@

$p = Get-Process $ProcessName -ErrorAction Stop | Select-Object -First 1
if ($p.MainWindowHandle -eq [IntPtr]::Zero) { throw "main window handle is zero" }
[WinCap]::ForceTop($p.MainWindowHandle)
Start-Sleep -Milliseconds 800
$rect = [WinCap]::RectOf($p.MainWindowHandle)
$x, $y, $w, $h = $rect
if ($w -le 0 -or $h -le 0) { throw "invalid window size: $w x $h" }

$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($x, $y, 0, 0, $bmp.Size)
$g.Dispose()
$full = if ([System.IO.Path]::IsPathRooted($OutFile)) { $OutFile } else { Join-Path $PSScriptRoot $OutFile }
$dir = Split-Path $full -Parent
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
$bmp.Save($full, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
[WinCap]::UnTop($p.MainWindowHandle)
Write-Output ("OK {0}x{1} -> {2}" -f $w, $h, $full)
